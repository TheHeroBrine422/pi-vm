//! VM lifecycle: create / resume / boot / attach / teardown.
//!
//! Process model (the "one shell" design):
//!   - virtiofsd  -> background child (log file)
//!   - CH         -> background child (console file, api-socket)
//!   - ssh        -> foreground child that inherits the terminal (M2 attach)
//!
//! VM lifetime = this process's lifetime. Exiting the ssh session (Ctrl-C
//! while booting, or Ctrl-C/SIGTERM in any state — a Ctrl-C while attached goes
//! to the guest first and becomes a clean stop if it takes the ssh client
//! down) runs the teardown: graceful
//! guest poweroff -> SIGTERM -> SIGKILL -> kill virtiofsd -> delete the
//! tap (the shared bridge persists).
//!
//! Orphan protection: every child carries PR_SET_PDEATHSIG=SIGTERM (set in
//! pre_exec, see child_pdeathsig) — if pi-vm dies for ANY reason, including
//! SIGKILL, the kernel SIGTERMs each child: CH's SIGTERM is an immediate
//! host-side poweroff, virtiofsd exits, the ssh client exits (the remote
//! session ends). `kill -9 pi-vm` therefore stops the VM instead of
//! orphaning it. (PR_SET_PDEATHSIG on pi-vm itself does NOT do this — it
//! only signals pi-vm when its parent, e.g. the shell, dies.)

use std::io::{IsTerminal, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::util::{run, run_ok, run_ok_priv, run_priv, sha256_file, virtiofsd_path};
use crate::state::{self, VmHome, VmMeta, VmState};

/// Failure classes with distinct exit codes (so the tool is scriptable).
pub enum BootFail {
    /// Usage/preflight error (missing image, no KVM, name collision, network)
    Preflight(String), // 2
    /// VM failed to boot (CH exited during boot)
    BootFailed(String), // 3
    /// ssh did not become reachable within the timeout
    SshUnreachable(String), // 4
    /// VM is in use by another pi-vm process
    InUse(String), // 5
    /// Interrupted (Ctrl-C / SIGTERM) before a normal stop
    Interrupted(String), // 130
    /// Other
    Other(String), // 1
}

impl BootFail {
    pub fn code(&self) -> i32 {
        match self {
            BootFail::Preflight(_) => 2,
            BootFail::BootFailed(_) => 3,
            BootFail::SshUnreachable(_) => 4,
            BootFail::InUse(_) => 5,
            BootFail::Interrupted(_) => 130,
            BootFail::Other(_) => 1,
        }
    }
    pub fn msg(&self) -> &str {
        match self {
            BootFail::Preflight(s)
            | BootFail::BootFailed(s)
            | BootFail::SshUnreachable(s)
            | BootFail::InUse(s)
            | BootFail::Interrupted(s)
            | BootFail::Other(s) => s,
        }
    }
}

/// Requested VM configuration (create flags).
pub struct VmConfig {
    /// None = --no-mount VM (no virtiofsd, no /workspace in the guest).
    pub project: Option<String>,
    pub name: Option<String>,
    pub vcpus: u32,
    pub memory_mb: u32,
    pub disk_gb: u32,
    pub kvm: bool,
    /// Default: owner uid of the project dir (the ownership guarantee).
    pub squash_uid: Option<u32>,
}

/// Create a new VM: id, meta, ssh key, disk copy + resize.
pub fn create_vm(home: &VmHome, cfg: &VmConfig) -> Result<VmMeta, String> {
    // preflight
    if let Some(p) = &cfg.project
        && !std::path::Path::new(p).is_dir()
    {
        return Err(format!("project dir not found: {p}"));
    }
    if cfg.kvm && !std::path::Path::new("/dev/kvm").exists() {
        return Err("/dev/kvm not found — CH requires KVM (no TCG mode). Fix KVM (module loaded, /dev/kvm present + accessible).".into());
    }
    let prepped = home.prepped_image();
    if !prepped.exists() {
        return Err(format!(
            "pre-baked base image missing at {} — run `pi-vm bake` first",
            prepped.display()
        ));
    }
    // Record the base image's sha256 in the meta (so the user can tell
    // whether the VM is based on the most recent bake — compare against
    // `pi-vm version`). None only if the image is unreadable, in which
    // case the `cp` below fails with a clear error anyway.
    let base_image_sha256 = sha256_file(&prepped);
    // name uniqueness
    if let Some(name) = &cfg.name {
        for id in home.list_vm_ids() {
            if let Ok(m) = VmMeta::load(home, &id)
                && m.name.as_deref() == Some(name.as_str()) {
                    return Err(format!("a VM named '{name}' already exists ({id}) — use `pi-vm resume {name}` or pick another name"));
                }
        }
    }

    let id = state::new_id();
    // Shared-bridge static networking: the MAC is host-side only and the
    // address is a static pick from the host pool — both travel to the
    // guest on the per-VM config disk (write_config_disk; the baked
    // pi-vm-net script reads /dev/vdb). No DHCP, no lease polling.
    let mac = state::random_mac();
    let hn = host_net(home);
    let ip = allocate_ip(home, &hn.pool)?;
    let pool = hn.pool.clone();
    let squash = match cfg.squash_uid {
        Some(u) => u,
        None => match &cfg.project {
            Some(p) => project_owner_uid(p)
                .ok_or_else(|| format!(
                    "cannot determine the owner of {p} — pass --squash-uid explicitly"
                ))?,
            None => current_uid(),
        },
    };
    let now = state::now();

    // Master snapshot adoption: a default-spec create (4 vCPU / 8 G /
    // the master's disk size) restores the bake-time master snapshot.
    // The master (meta + disk + snapshot) is NEVER modified: the adopter
    // gets its own directory, an instant qcow2 copy-on-write layer on
    // the master disk, a fresh config disk, and its own (IP, MAC) from
    // the pool. Its first boot is a CH restore; right after the restore
    // it is renumbered over ssh (the master's IP is baked into the
    // restored memory). Any other spec falls through to the cold path.
    if cfg.vcpus == 4 && cfg.memory_mb == 8192
        && let Ok(w) = VmMeta::load(home, state::WARM_ID)
        && w.name.as_deref() == Some("warm-default")
        && w.state == VmState::Stopped
        && cfg.disk_gb == w.disk_gb
        && home.snapshot_dir("default").join("state.json").exists()
    {
        let dir = home.vms_dir().join(&id);
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        // Disk = a per-VM copy of the master's disk (never modified).
        // CH 53's qcow2 parser rejects backing chains (MaxNestingDepth),
        // so a layer is out; `cp --reflink=auto` is ~0.1 s on a
        // reflink fs (this host is btrfs) and falls back to a plain
        // copy elsewhere (2.3 G sparse ≈ 1–2 s).
        let master_disk = w.vm_dir(home).join("disk.qcow2");
        let disk = dir.join("disk.qcow2");
        if run("cp", &["--reflink=auto", master_disk.to_str().unwrap(), disk.to_str().unwrap()]).is_err()
            && run("cp", &[master_disk.to_str().unwrap(), disk.to_str().unwrap()]).is_err()
        {
            return Err("cp of the master disk failed (see the log above)".into());
        }
        let meta = VmMeta {
            id: id.clone(),
            name: cfg.name.clone(),
            created: now,
            updated: now,
            project: cfg.project.as_ref().map(|p| {
                std::fs::canonicalize(p)
                    .map(|c| c.to_string_lossy().to_string())
                    .unwrap_or_else(|_| p.clone())
            }),
            subnet: pool,
            ip,
            mac,
            vcpus: cfg.vcpus,
            memory_mb: cfg.memory_mb,
            disk_gb: cfg.disk_gb,
            kvm: cfg.kvm,
            squash_uid: squash,
            base_image_sha256,
            state: VmState::Stopped,
            ch_pid: None,
            net_gen: Some(3),
            snapshot: true,
        };
        meta.save(home)?;
        write_config_disk(home, &meta, &hn)?;
        println!(
            "   created VM {} {} (master adoption — first boot is a snapshot restore)",
            meta.id,
            meta.name.as_deref().unwrap_or("(no name)")
        );
        return Ok(meta);
    }
    let meta = VmMeta {
        id: id.clone(),
        name: cfg.name.clone(),
        created: now,
        updated: now,
        project: cfg.project.as_ref().map(|p| {
            std::fs::canonicalize(p)
                .map(|c| c.to_string_lossy().to_string())
                .unwrap_or_else(|_| p.clone())
        }),
        subnet: pool,
        ip,
        mac,
        vcpus: cfg.vcpus,
        memory_mb: cfg.memory_mb,
        disk_gb: cfg.disk_gb,
        kvm: cfg.kvm,
        squash_uid: squash,
        base_image_sha256,
        state: VmState::Stopped,
        ch_pid: None,
        net_gen: Some(3),
        snapshot: false,
    };
    let dir = meta.vm_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    meta.save(home)?;
    write_config_disk(home, &meta, &hn)?;

    // disk: copy pre-baked base + resize (fs grows on boot via the
    // runcmd / pi-vm-net growpart + btrfs resize — idempotent)
    let disk = meta.disk_path(home);
    let t_disk = std::time::Instant::now();
    run("cp", &[prepped.to_str().unwrap(), disk.to_str().unwrap()])?;
    let t_cp = t_disk.elapsed().as_secs_f64();
    // Grow-only host-side resize: shrinking the virtual size below the
    // baked fs size makes the root unmountable (btrfs metadata
    // references chunks beyond the truncated disk — the initramfs hangs
    // on the root UUID). A VM smaller than the baked size keeps the
    // base's virtual size (sparse — no storage cost) and pi-vm-net
    // shrinks the fs to the requested size (vm-disk-gb on the config
    // disk) while it is still mountable.
    let baked_gb = prepped_virtual_gb(&prepped)?;
    if meta.disk_gb > baked_gb {
        run("qemu-img", &["resize", &disk.to_string_lossy(), &format!("{}G", meta.disk_gb)])?;
    }
    let t_resize = t_disk.elapsed().as_secs_f64() - t_cp;
    println!("   disk: copy {:.2}s, resize {:.2}s (base {}G, requested {}G)", t_cp, t_resize, baked_gb, meta.disk_gb);

    println!("   created VM {} {} ({} vcpus, {} MB, {} GB disk)",
        meta.id, meta.name.as_deref().unwrap_or("(no name)"), meta.vcpus, meta.memory_mb, meta.disk_gb);
    Ok(meta)
}

/// The base image's virtual size in GiB (from `qemu-img info`).
fn prepped_virtual_gb(prepped: &std::path::PathBuf) -> Result<u32, String> {
    let out = run("qemu-img", &["info", prepped.to_str().unwrap()])?;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("virtual size:") {
            // "100 GiB (107374182400 bytes)" — prefer the byte count.
            if let Some(pos) = rest.rfind(" bytes") {
                let num = rest[..pos].rsplit(' ').next().unwrap_or("");
                if let Ok(b) = num.parse::<u64>() {
                    return Ok((b / 1073741824).max(1) as u32);
                }
            }
            if let Ok(g) = rest.split_whitespace().next().unwrap_or("1").parse::<u32>() {
                return Ok(g.max(1));
            }
        }
    }
    Err(format!("qemu-img info {}: no 'virtual size' line", prepped.display()))
}

/// Owner uid of the project dir (the default squash target).
fn project_owner_uid(project: &str) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(project).ok().map(|m| m.uid())
    }
    #[cfg(not(unix))]
    {
        let _ = project;
        None
    }
}

/// The invoking user's uid (the default squash target for --no-mount VMs,
/// where there is no project dir to take an owner from).
fn current_uid() -> u32 {
    crate::util::run("id", &["-u"])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Load + reconcile an existing VM for resume.
pub fn resume_vm(home: &VmHome, target: &str) -> Result<VmMeta, String> {
    let meta = state::resolve(home, target)?; // load() reconciles stale "running"
    if !meta.disk_path(home).exists() {
        return Err(format!("VM {} disk missing at {} — nothing to resume", meta.id, meta.disk_path(home).display()));
    }
    Ok(meta)
}

/// True if an `ip -o link show` line has the UP flag (an up interface shows
/// UP — or LOWER_UP — in its <flags>; a down one shows neither).
fn link_is_up(line: &str) -> bool {
    line.split('<')
        .nth(1)
        .and_then(|s| s.split('>').next())
        .map(|flags| {
            flags
                .split(',')
                .any(|f| {
                    let f = f.trim();
                    f == "UP" || f == "LOWER_UP"
                })
        })
        .unwrap_or(false)
}

/// CH boot-mode args: Some(--kernel/--initramfs/--cmdline) when the
/// direct-boot assets are usable, None otherwise. A usable cmdline must
/// carry root= (an empty one would boot the kernel with no root device).
pub fn direct_boot_args(home: &VmHome) -> Option<Vec<String>> {
    if !(home.kernel().exists() && home.initramfs().exists() && home.kernel_cmdline().exists()) {
        return None;
    }
    let cmdline = std::fs::read_to_string(home.kernel_cmdline()).ok()?;
    if !cmdline.trim().contains("root=") {
        return None;
    }
    Some(vec![
        "--kernel".into(),
        home.kernel().display().to_string(),
        "--initramfs".into(),
        home.initramfs().display().to_string(),
        "--cmdline".into(),
        cmdline,
    ])
}

/// The terminal tab name for an attach session:
/// `pi-vm <first-8-of-id> <project-basename>` (the basename is omitted
/// for --no-mount VMs).
fn attach_title(meta: &VmMeta) -> String {
    let id8: String = meta.id.chars().take(8).collect();
    let mut title = format!("pi-vm {id8}");
    if let Some(base) = meta.project
        .as_ref()
        .and_then(|p| {
            std::path::Path::new(p)
                .components()
                .next_back()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
        })
        .filter(|b| !b.is_empty())
    {
        title.push(' ');
        title.push_str(&base);
    }
    title
}

/// The prefix for the attach command: sets the terminal tab name via an
/// OSC escape sequence sent to the inherited stdout. Only when stdout is
/// a tty — otherwise the sequence would leak into piped output (the
/// attach decision itself is gated on stdin being a tty).
fn attach_title_prefix(meta: &VmMeta) -> Option<String> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    // Pass the title as a printf argument (not as the format string) so
    // a `%` or `\` in the basename is not interpreted; single quotes in
    // the title are escaped for the shell.
    let quoted = attach_title(meta).replace('\'', "'\\''");
    Some(format!("printf '\\e]0;%s\\a' '{quoted}'; "))
}

/// Boot the VM and (if attach) drop into ssh. Returns the exit code.
/// `timeout_s`: stop the VM N seconds after launch
/// (the deadline starts now; boot time counts against it — pass a budget
/// with a boot buffer). `rm`: the caller will auto-delete the VM after a
/// normal stop (the --rm flag — only affects the stop message; the
/// deletion itself happens in the caller, after the lock is released).
pub fn boot(home: &VmHome, meta: VmMeta, attach: bool, shell: bool,
           timeout_s: Option<u64>, rm: bool) -> Result<i32, BootFail> {
    crate::util::prime_sudo(); // before any timing: a stale sudo lease must not prompt inside the measured window
    let boot_start = std::time::Instant::now();
    let deadline = timeout_s
        .map(|s| boot_start + Duration::from_secs(s));
    if let Some(t) = timeout_s {
        println!("   timeout: the VM stops {t}s after launch");
    }
    // The resume case: the previous stop may still be finishing (the
    // meta says stopped, the supervisor still holds the flock while the
    // guest powers off). Wait it out (up to 120 s); a RUNNING meta with
    // a held lock is a genuine contention — fail fast.
    let _lock = {
        let mut attempt = 0u32;
        loop {
            match state::VmLock::acquire(&meta, home) {
                Ok(l) => break l,
                Err(_e) if attempt < 60 && meta.state == VmState::Stopped => {
                    if attempt == 0 {
                        println!("   waiting for the in-flight stop to finish (lock held) ...");
                    }
                    std::thread::sleep(Duration::from_secs(2));
                    attempt += 1;
                }
                Err(e) => return Err(BootFail::InUse(e)),
            }
        }
    };
    let t_lock = boot_start.elapsed().as_secs_f64();

    // VMs created by an older pi-vm (legacy per-VM /24 + NoCloud ISO, the
    // DHCP-era build, or any other generation): their disks carry an old
    // baked image whose pi-vm-net script looks for the old network config
    // (the NoCloud ISO / a DHCP lease) — gone now. Fail fast with a clear
    // pointer instead of burning the 300 s ssh deadline.
    if state::is_legacy_networking(&meta) {
        return Err(BootFail::Preflight(format!(
            "VM {} was created by an older pi-vm (networking generation {}; recorded subnet: {}) and its disk still carries the old baked image — it cannot find its network config (the NoCloud ISO / DHCP are gone). Recreate it: `pi-vm rm <id>` then `pi-vm create <same-path>` on the re-baked image (project data lives on the host; only the guest rootfs is replaced)",
            meta.id, meta.net_gen.unwrap_or(0), meta.subnet
        )));
    }

    // DIRECT KERNEL BOOT (the only mode): the kernel assets are extracted
    // by `bake`; without them no VM can boot. Checked before any side
    // effects (network, ISO, children).
    let direct_args = direct_boot_args(home).ok_or_else(|| BootFail::Preflight(format!(
        "direct-boot kernel assets missing ({} / {} / {}) — run `pi-vm bake` to extract them",
        home.kernel().display(), home.initramfs().display(), home.kernel_cmdline().display()
    )))?;

    // Self-heal: if the disk was not cleanly closed (pi-vm was killed,
    // or the guest poweroff stalled and the SIGTERM fallback fired), CH's
    // refcount rebuild fails with InvalidClusterIndex on the undersized
    // refcount table in the base image (1 refcount cluster covers 2 GiB,
    // but the file is 2.16 GiB). A full qemu-img convert rewrites the
    // refcount table correctly. Paid only on the resume after an unclean
    // shutdown; a clean shutdown needs no rebuild at all.
    let disk = meta.disk_path(home);
    if !qemu_img_cleanly_closed(&disk) {
        println!("   disk not cleanly closed — rebuilding the refcount table (qemu-img convert, may take a while)");
        let tmp = disk.with_file_name("disk.qcow2.rebuild");
        match run("qemu-img", &["convert", "-O", "qcow2", disk.to_str().unwrap(), tmp.to_str().unwrap()]) {
            Ok(_) => {
                if let Err(e) = std::fs::rename(&tmp, &disk) {
                    eprintln!("   (rebuild rename failed: {e} — trying to boot anyway)");
                    let _ = std::fs::remove_file(&tmp);
                } else {
                    println!("   refcount table rebuilt");
                }
            }
            Err(e) => {
                eprintln!("   (rebuild failed: {e} — trying to boot anyway)");
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    // --- network (idempotent, privileged via auto-sudo) ---
    // Shared bridge serves every VM; this VM gets its own TAP + a static
    // pool IP (written to the config disk by create_vm).
    ensure_shared_network(home).map_err(BootFail::Preflight)?;
    let tap = meta.tap_name();
    if !run_ok_priv("ip", &["link", "show", &tap]) {
        run_priv("ip", &["tuntap", "add", "dev", &tap, "mode", "tap"])
            .map_err(BootFail::Preflight)?;
    }
    // Let the invoking (non-root) user open the TAP so CH can use it (a
    // root-created tap is unreadable by the rootless CH process — the v1
    // blocker, mirrored here)
    if !crate::util::is_root()
        && let Ok(user) = std::env::var("USER") {
            run_ok_priv("chown", &[&user, &format!("/dev/{tap}")]);
        }
    // These must succeed — a down tap or a tap not enslaved to the bridge
    // means the guest can't reach its config + the host can't reach the
    // guest, and the boot would burn the full ssh deadline with no
    // diagnostics ("No route to host").
    run_priv("ip", &["link", "set", &tap, "master", BRIDGE])
        .map_err(BootFail::Preflight)?;
    run_priv("ip", &["link", "set", &tap, "up"])
        .map_err(BootFail::Preflight)?;
    // Port isolation: the TAP is an ISOLATED bridge port — it can talk to
    // the bridge (the host side) but never to other ports, so VMs cannot
    // reach each other on the shared L2 domain. This is the real policy
    // (bridged traffic never touches iptables FORWARD); the FORWARD DROP
    // rule is the fallback for routed paths. Best effort — an old
    // iproute without `bridge link set isolated` degrades to the iptables
    // fallback only.
    run_ok_priv("bridge", &["link", "set", "dev", &tap, "isolated", "on"]);
    {
        let links = run_priv("ip", &["-o", "link", "show"]).unwrap_or_default();
        let tap_line = links.lines().find(|l| l.contains(&format!(": {tap}:"))).unwrap_or("");
        let tap_up = link_is_up(tap_line);
        let tap_master = tap_line.contains(&format!("master {BRIDGE}"));
        if !tap_up || !tap_master {
            return Err(BootFail::Preflight(format!(
                "network setup failed: tap {tap} up={tap_up} enslaved={tap_master} — the ip commands need CAP_NET_ADMIN (check that sudo works); the VM would be unreachable"
            )));
        }
    }
    let t_net = boot_start.elapsed().as_secs_f64();

    // --- orphan protection ---
    // (1) The children get PR_SET_PDEATHSIG=SIGTERM via pre_exec
    // (child_pdeathsig): if pi-vm dies — for any reason, including SIGKILL —
    // the kernel SIGTERMs each child: CH's SIGTERM is an immediate
    // host-side poweroff, virtiofsd exits, the ssh client exits. The VM
    // stops and doesn't leak (unflushed guest data may be lost — PLAN.md §3b.3).
    // (2) pi-vm itself gets PR_SET_PDEATHSIG, so a dead shell (the terminal
    // is closed) SIGTERMs pi-vm -> the SIGTERM handler tears down.
    // (3) The VM daemons (CH, virtiofsd) are additionally setsid()'d out of
    // the terminal's process group (with_pdeathsig_isolated): a Ctrl-C
    // reaches only pi-vm (whose handler runs the graceful stop) and ssh
    // (which forwards it to the guest session), never the daemons —
    // CH's own SIGINT handler is an immediate hard exit, and virtiofsd's
    // default SIGINT disposition would drop the guest's /workspace mount,
    // both before pi-vm's handler could act.
    set_pdeathsig();
    let squash = meta.squash_uid;
    let mut vfs_child: Option<Child> = match &meta.project {
        Some(project) => {
            let vfs = virtiofsd_path()
                .ok_or_else(|| BootFail::Preflight("virtiofsd not found — run `pi-vm install`".into()))?;
            let sock = meta.vm_dir(home).join("fs.sock");
            let _ = std::fs::remove_file(&sock);
            run_ok("chmod", &["a+w", project]); // sandbox guarantee (best effort)
            let vfs_log = std::fs::File::create(meta.vm_dir(home).join("virtiofsd.log"))
                .map_err(|e| BootFail::Preflight(e.to_string()))?;
            let mut cmd = Command::new(&vfs);
            cmd.args([
                &format!("--socket-path={}", sock.display()),
                &format!("--shared-dir={}", project),
                "--cache=auto",
                &format!("--translate-uid=squash-guest:0:{squash}:65536"),
                &format!("--translate-gid=squash-guest:0:{squash}:65536"),
            ]);
            cmd.stdin(Stdio::null());
            cmd.stdout(vfs_log.try_clone().unwrap());
            cmd.stderr(vfs_log); // v1: both streams to the log (not the user's terminal)
            with_pdeathsig_isolated(&mut cmd);
            let mut child = cmd
                .spawn()
                .map_err(|e| BootFail::Preflight(format!("spawn virtiofsd: {e}")))?;
            // Wait for the socket to appear (virtiofsd creates it within
            // milliseconds) — polling replaces a fixed 1 s sleep (boot
            // profile: it was 1.00 s of the 1.15 s supervisor time).
            // Capped at 2 s: proceed anyway if it hasn't appeared — CH
            // fails loudly with a clear error.
            let sock_wait = std::time::Instant::now() + Duration::from_secs(2);
            while !sock.exists() {
                if child.try_wait().ok().flatten().is_some() {
                    return Err(BootFail::Preflight("virtiofsd died at startup — see virtiofsd.log".into()));
                }
                if std::time::Instant::now() > sock_wait {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Some(child)
        }
        None => None,
    };
    let t_vfsd = boot_start.elapsed().as_secs_f64();

    // --- CH (background child) ---
    let ch = home.ch_binary();
    let cpus = if meta.kvm {
        format!("boot={},nested=on", meta.vcpus)
    } else {
        format!("boot={}", meta.vcpus)
    };
    let api_sock = meta.vm_dir(home).join("ch.sock");
    let _ = std::fs::remove_file(&api_sock);
    let console_file = std::fs::File::create(meta.console_path(home))
        .map_err(|e| BootFail::Preflight(e.to_string()))?;
    // Boot mode: DIRECT KERNEL BOOT (CH loads the kernel + initramfs
    // straight from the pre-baked image — no UEFI, no GRUB). The assets
    // were preflight-checked above (fail fast, before any side effects).
    let mut ch_args: Vec<String> = vec![
        "--api-socket".into(),
        format!("path={}", api_sock.display()),
        "--cpus".into(),
        cpus,
        "--memory".into(),
        format!("size={}M,shared=on", meta.memory_mb),
    ];
    ch_args.extend(direct_args);
    ch_args.extend(["--disk".into(),
        format!("path={},image_type=qcow2", meta.disk_path(home).display()),
        "--disk".into(),
        format!("path={},image_type=raw,readonly=on", meta.vm_dir(home).join("config.raw").display()),
        "--net".into(),
        format!("tap={},mac={}", meta.tap_name(), meta.mac),
        "--console".into(),
        format!("file={}", meta.console_path(home).display()),
    ]);
    if meta.project.is_some() {
        ch_args.push("--fs".into());
        ch_args.push(format!(
            "tag=workspace,socket={}",
            meta.vm_dir(home).join("fs.sock").display()
        ));
    }
    let mut ch_cmd = Command::new(&ch);
    if meta.snapshot {
        // Snapshot-pool adoption: a BARE VMM + an API restore (CH 53
        // diverts to the VmBoot branch when --kernel is passed — the
        // bare VMM is the only restore path; the snapshot's
        // config.json carries cpus/memory/devices).
        ch_cmd.arg("--api-socket").arg(format!("path={}", api_sock.display()));
    } else {
        ch_cmd.args(&ch_args);
    }
    ch_cmd.stdin(Stdio::null());
    ch_cmd.stdout(console_file.try_clone().unwrap());
    ch_cmd.stderr(console_file);
    with_pdeathsig_isolated(&mut ch_cmd);
    let mut ch_child = match ch_cmd.spawn()
    {
        Ok(c) => c,
        Err(e) => {
            // CH failed to start — don't orphan virtiofsd
            if let Some(vfs) = vfs_child.as_mut() {
                let _ = vfs.kill();
                let _ = vfs.wait();
            }
            return Err(BootFail::Preflight(format!("spawn CH: {e}")));
        }
    };

    let ch_spawned_at = std::time::Instant::now();
    let t_chspawn = ch_spawned_at.duration_since(boot_start).as_secs_f64();

    // Snapshot-pool adoption: wait for the VMM's API socket, then
    // restore (Copy memory mode + immediate resume). The VM comes up
    // mid-run: sshd + the network + (if re-bound before the restore —
    // the virtiofsd above) /workspace are all already established.
    let snapshot_restore = meta.snapshot;
    // Where the adoption time goes, for the boot timing line (None on the
    // cold path). Declared here because the restore block fills it.
    let mut restore_timing: Option<String> = None;
    if snapshot_restore {
        // Per-VM snapshot built from the NEVER-modified master snapshot:
        // memory-ranges is hard-linked (read-only during a restore — zero
        // cost on any fs; a full copy is the fallback), the two small
        // JSON files are copied and path-patched (disk/console/fs.sock/
        // config.raw -> this VM's dir; tap + MAC -> this VM's).
        let master = VmMeta::load(home, state::WARM_ID)
            .map_err(|e| BootFail::Preflight(format!("master meta lost before restore: {e}")))?;
        let mdir_s = master.vm_dir(home).display().to_string().replace('\\', "/");
        let vdir_s = meta.vm_dir(home).display().to_string().replace('\\', "/");
        let (m_tap, v_tap) = (master.tap_name(), meta.tap_name());
        let (m_mac, v_mac) = (master.mac.clone(), meta.mac.clone());
        let (msnap, vsnap) = (home.snapshot_dir("default"), meta.vm_dir(home).join("snapshot"));
        std::fs::create_dir_all(&vsnap)
            .map_err(|e| BootFail::Preflight(format!("create {}: {e}", vsnap.display())))?;
        if std::fs::hard_link(msnap.join("memory-ranges"), vsnap.join("memory-ranges")).is_err() {
            run("cp", &[
                "--reflink=auto",
                msnap.join("memory-ranges").to_str().unwrap(),
                vsnap.join("memory-ranges").to_str().unwrap()
            ])
            .map_err(BootFail::Preflight)?;
        }
        for name in ["config.json", "state.json"] {
            let text = std::fs::read_to_string(msnap.join(name))
                .map_err(|e| BootFail::Preflight(format!("read {name}: {e}")))?;
            let patched = text.replace(&mdir_s, &vdir_s).replace(&m_tap, &v_tap).replace(&m_mac, &v_mac);
            std::fs::write(vsnap.join(name), patched)
                .map_err(|e| BootFail::Preflight(format!("write {name}: {e}")))?;
        }
        let sd = std::time::Instant::now() + Duration::from_secs(20);
        while !api_sock.exists() {
            if ch_child.try_wait().ok().flatten().is_some() {
                return Err(BootFail::BootFailed("CH exited before the restore — see console.log".into()));
            }
            if std::time::Instant::now() > sd {
                return Err(BootFail::Preflight("the CH api socket never appeared (restore)".into()));
            }
            thread::sleep(Duration::from_millis(20));
        }
        // Serialize the adoption: every restored guest shares the master's
        // (IP, MAC) in memory until it is renumbered, and two such guests
        // on one bridge make the pre-renumbering ssh ambiguous. The lock
        // is held from the restore call until the VM's own IP is up
        // (flock releases on drop / process exit).
        use fs2::FileExt as _;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(home.root.join("master-adopt.lock"))
            .map_err(|e| BootFail::Preflight(format!("open master-adopt.lock: {e}")))?;
        lock.lock_exclusive().map_err(|e| BootFail::Preflight(format!("master-adopt.lock: {e}")))?;
        let t_adopt_lock = boot_start.elapsed().as_secs_f64();
        let body = format!(
            "{{\"source_url\":\"file://{}\",\"memory_restore_mode\":\"Copy\",\"resume\":true}}",
            vsnap.display().to_string().replace('\\', "/")
        );
        let (st, resp) = match api_put_unix(&api_sock, "/api/v1/vm.restore", &body) {
            Ok(r) => r,
            Err(e) => return Err(BootFail::Preflight(format!("vm.restore call failed: {e}"))),
        };
        if st != 204 {
            return Err(BootFail::Preflight(format!(
                "vm.restore failed (HTTP {st}): {}",
                resp.lines().last().unwrap_or("").chars().take(200).collect::<String>()
            )));
        }
        println!("   snapshot restore (copy mode) — renumbering to {} ...", meta.ip);
        let t_restore = boot_start.elapsed().as_secs_f64();
        if !wait_ssh_ready(&master.ip, 30) {
            return Err(BootFail::BootFailed(format!(
                "the restored VM never came up at the master's IP ({}) — see console.log",
                master.ip
            )));
        }
        let t_master = boot_start.elapsed().as_secs_f64();
        if let Err(e) = renumber_guest(&master.ip, &meta.ip, &meta.subnet, &meta.mac) {
            return Err(BootFail::BootFailed(format!("post-restore renumbering failed: {e}")));
        }
        if !wait_ssh_ready(&meta.ip, 30) {
            return Err(BootFail::BootFailed(format!(
                "the VM never came up at its own IP ({}) after renumbering — see console.log \
                 and /tmp/pi-vm-renumber.log in the guest (resume it to read the log)",
                meta.ip
            )));
        }
        println!(
            "   renumbered {} -> {} (master untouched; this VM is now at its own IP)",
            master.ip, meta.ip
        );
        // The phases the [supervisor]/[guest] split cannot see:
        // adopt-lock = waiting for a concurrent adopter; restore = the
        // 204 (Copy reads the memory file — page-cache bound);
        // master-ssh = the restored guest answering the master's IP;
        // renumber = applying the new (IP, MAC) + the guest answering
        // its own IP.
        let t_own = boot_start.elapsed().as_secs_f64();
        restore_timing = Some(format!(
            "adopt-lock {:.2}s, restore {:.2}s, master-ssh {:.2}s, renumber {:.2}s",
            t_adopt_lock - t_chspawn, t_restore - t_adopt_lock,
            t_master - t_restore, t_own - t_master
        ));
    }

    // record running state
    let mut meta = meta;
    meta.state = VmState::Running;
    meta.ch_pid = Some(ch_child.id() as i32);
    meta.updated = state::now();
    if snapshot_restore {
        // one-shot: the first boot was the restore; resumes are cold
        // (the disk has diverged from the template snapshot)
        meta.snapshot = false;
    }
    if let Err(e) = meta.save(home) {
        // state save failed — don't orphan the running children
        teardown(home, &meta, &mut ch_child, &mut vfs_child, &api_sock, rm);
        return Err(BootFail::Other(e));
    }

    match &meta.project {
        Some(p) => println!("==> VM {} booting ({} vcpus, {} MB, share {} -> uid {squash})",
            meta.id, meta.vcpus, meta.memory_mb, p),
        None => println!("==> VM {} booting ({} vcpus, {} MB, no shared mount)",
            meta.id, meta.vcpus, meta.memory_mb),
    }

    // Signal handling: Ctrl-C (SIGINT) and SIGTERM set separate flags.
    // Ctrl-C interrupts the boot-wait only (while attached it goes to the
    // remote session via ssh — and becomes a clean stop only if that Ctrl-C
    // takes the ssh client down); SIGTERM tears down in every state.
    // Ctrl-Z (SIGTSTP)
    // sets a flag the wait loops turn into a one-line hint — the default
    // suspend action is replaced, so Ctrl-Z no longer hangs the session
    // (the VM daemons ignore SIGTSTP/SIGINT themselves — see
    // child_pdeathsig_isolated — because the terminal sends them to the
    // whole foreground group).
    let interrupted = Arc::new(AtomicBool::new(false));
    let terminated = Arc::new(AtomicBool::new(false));
    let tstp = Arc::new(AtomicBool::new(false));
    install_signal_handler(interrupted.clone(), terminated.clone(), tstp.clone());

    // --- wait for ssh (the IP is known — static pool assignment) ---
    // Respect --timeout (the VM can't be "ready" later than it is going
    // to be killed anyway), capped at 300 s.
    let boot_deadline = timeout_s.map(|t| t.min(300)).unwrap_or(300);
    let mut waited_ms = 0u64;
    let mut ready = false;
    // 500 ms cadence (boot profile: 1 s polling added up to 1 s of
    // quantization after the guest was ready).
    while waited_ms < boot_deadline * 1000 {
        thread::sleep(Duration::from_millis(500));
        waited_ms += 500;
        if interrupted.load(Ordering::Relaxed) || terminated.load(Ordering::Relaxed) {
            break;
        }
        if tstp.swap(false, Ordering::Relaxed) {
            eprintln!("{SIGTSTP_HINT}");
        }
        if let Some(status) = ch_child.try_wait().ok().flatten() {
            let detail = status.code().map(|c| format!("exit {c}")).unwrap_or_else(|| "signalled".to_string());
            teardown(home, &meta, &mut ch_child, &mut vfs_child, &api_sock, rm);
            return Err(BootFail::BootFailed(format!("CH exited during boot ({detail}) — see console.log")));
        }
        if tcp_open(&meta.ip, 22) {
            let probe = Command::new("ssh")
                .args(ssh_opts())
                .arg(format!("root@{}", meta.ip))
                .arg("true")
                .output();
            if probe.map(|o| o.status.success()).unwrap_or(false) {
                ready = true;
                break;
            }
        }
        if waited_ms % 10000 == 0 {
            print!("   booting… ({}s)", waited_ms / 1000);
            std::io::Write::flush(&mut std::io::stdout()).ok();
        }
    }
    println!();
    if !ready {
        if interrupted.load(Ordering::Relaxed) || terminated.load(Ordering::Relaxed) {
            eprintln!(
                "interrupted — tearing down (int={} term={})",
                interrupted.load(Ordering::Relaxed),
                terminated.load(Ordering::Relaxed)
            );
            teardown(home, &meta, &mut ch_child, &mut vfs_child, &api_sock, rm);
            return Err(BootFail::Interrupted("interrupted before ssh became reachable".into()));
        }
        eprintln!("VM did not become ssh-reachable in {boot_deadline}s (ip {} — check config.raw + the console tail):", meta.ip);
        tail_file(&meta.console_path(home), 20);
        teardown(home, &meta, &mut ch_child, &mut vfs_child, &api_sock, rm);
        return Err(BootFail::SshUnreachable("boot timed out".into()));
    }

    // Log the boot timing (helps diagnose slow boots: the supervisor
    // phases before CH spawn + the guest side CH spawn -> ssh-ready;
    // the guest internals — kernel vs userspace — are in console.log +
    // the guest journal / `systemd-analyze` on the VM).
    let preflight = ch_spawned_at.duration_since(boot_start).as_secs_f64();
    let guest_boot = std::time::Instant::now().duration_since(ch_spawned_at).as_secs_f64();
    println!("   boot timing: [supervisor] lock {:.2}s, net+tap {:.2}s, virtiofsd {:.2}s, ch-spawn {:.2}s | [guest] {:.1}s (ch-spawn -> ssh-ready), total {:.1}s{}",
        t_lock, t_net - t_lock, t_vfsd - t_net, t_chspawn - t_vfsd, guest_boot, preflight + guest_boot,
        restore_timing.as_ref().map(|s| format!(" | [restore] {s}")).unwrap_or_default());

    // --- attach or foreground-wait ---
    // `attach_failed`: the attach ssh session did not run normally (spawn
    // failed, wait failed, or ssh exited with 255 — a transport/auth
    // failure — while the guest was still up). Normal stops — user exit
    // (any remote status, docker-like), the timeout/SIGTERM kill of the
    // session (`stop_announced`), and a guest-initiated poweroff
    // (`guest_stopped`) — are NOT failures and report Ok. A failed attach
    // must surface as Err (exit 1, same as before): the --rm auto-delete
    // skips Err, so an Ok(1) here would delete the VM after a transient
    // ssh hiccup.
    let mut attach_failed = false;
    // A non-interactive run stopped by Ctrl-C/SIGTERM is NOT a normal stop
    // (README: exit 130, the disk is left intact for `pi-vm resume`, and
    // --rm does not delete it).
    let mut interrupted_stop = false;
    let code = if attach {
        println!("==> attached — exiting the ssh session stops the VM (Ctrl-C goes to the remote session; SIGTERM stops the VM)");
        let target = format!("root@{}", meta.ip);
        let mut ssh = Command::new("ssh");
        ssh.args(ssh_opts())
            .arg("-t")
            .arg(&target);
        // Drop into /workspace (baked into the image, so `cd` always works —
        // before virtiofs has mounted it's an empty dir, and the files
        // appear in the cwd when the mount lands). Default: pi.
        // --shell: a plain interactive shell. Exiting the session stops the VM.
        let attach_cmd = if shell {
            "cd /workspace; bash"
        } else {
            "cd /workspace; pi"
        };
        // Set the terminal tab name before the session command runs
        // (only when stdout is a tty).
        let attach_cmd = match attach_title_prefix(&meta) {
            Some(prefix) => format!("{prefix}{attach_cmd}"),
            None => attach_cmd.to_string(),
        };
        ssh.arg(attach_cmd);
        with_pdeathsig(&mut ssh);
        // Poll rather than block on status(): a SIGTERM to the supervisor
        // must tear the VM down even while attached (the old blocking
        // status() made `kill <pi-vm-pid>` a no-op — the flag was set but
        // never checked). Ctrl-C (SIGINT) is NOT a stop here: the terminal
        // delivers it to the foreground group and the live ssh forwards it
        // to the remote session; if that Ctrl-C takes the ssh client down
        // (exit 255 within 30 s of the Ctrl-C), the 255 branch treats it
        // as a clean stop (docker-attach-ish: the interrupt reaches the
        // guest first, then the VM stops cleanly).
        match ssh
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Err(e) => {
                eprintln!("ssh attach failed: {e}");
                attach_failed = true;
                1
            }
            Ok(mut ssh) => {
                let mut stop_announced = false; // we killed ssh (timeout / SIGTERM / Ctrl-C)
                let mut guest_stopped = false; // CH exited on its own (guest poweroff)
                let mut ctrl_c_at: Option<std::time::Instant> = None; // last Ctrl-C during attach
                loop {
                    if tstp.swap(false, Ordering::Relaxed) {
                        eprintln!("{SIGTSTP_HINT}");
                    }
                    if interrupted.swap(false, Ordering::Relaxed) {
                        // Ctrl-C during attach: the terminal also delivers
                        // it to the live ssh, which forwards it to the
                        // guest session — so it does NOT stop the VM here.
                        // If that Ctrl-C takes the ssh client down (exit
                        // 255), the 255 branch below (within a short
                        // window) treats it as a clean stop instead of the
                        // 120 s failure grace period.
                        ctrl_c_at = Some(std::time::Instant::now());
                    }
                    let timed_out = deadline
                        .map(|d| std::time::Instant::now() >= d)
                        .unwrap_or(false);
                    if terminated.load(Ordering::Relaxed) || timed_out {
                        if !stop_announced {
                            if timed_out {
                                eprintln!("timeout — stopping the VM");
                            }
                            eprintln!("stopping the ssh session and the VM");
                            stop_announced = true;
                        }
                        libc_kill(ssh.id(), 15);
                    }
                    // Guest-initiated stop (the user ran `poweroff` in
                    // the guest): sshd dies a few
                    // seconds BEFORE CH exits, and a live ssh client then
                    // hangs on the dead connection for the full TCP
                    // retransmit timeout (~15 min). Kill it as soon as CH
                    // is gone and treat it as a normal stop.
                    if ch_child.try_wait().ok().flatten().is_some() {
                        if let Ok(None) = ssh.try_wait() {
                            libc_kill(ssh.id(), 15);
                        }
                        guest_stopped = true;
                    }
                    match ssh.try_wait() {
                        Ok(Some(s)) => {
                            let c = s.code().unwrap_or(0);
                            if c == 255 && !stop_announced && !guest_stopped {
                                // A Ctrl-C within the last 30 s: the user
                                // interrupted the session — ssh forwarded it
                                // to the guest, then the ssh client exited
                                // on the signal. A clean stop, not a failure
                                // (the grace period below is for TRANSIENT
                                // failures with no user intent).
                                if ctrl_c_at
                                    .map(|t| t.elapsed() <= Duration::from_secs(30))
                                    .unwrap_or(false)
                                {
                                    eprintln!("interrupted (Ctrl-C) — stopping the VM");
                                    stop_announced = true;
                                } else {
                                // 255 = an ssh TRANSPORT failure (connection /
                                // auth — a remote session ending with status
                                // 1-254 is a normal end, docker-like).
                                //
                                // Two very different causes: (A) the guest
                                // is powering off — sshd stops MID-shutdown,
                                // but CH exits only at the end (a full guest
                                // shutdown takes 45-120 s); (B) a transient
                                // ssh hiccup while the guest is still up.
                                // A 255 cannot distinguish them, so give the
                                // guest the full stop budget: CH exiting in
                                // the window means (A) — a normal stop;
                                // otherwise it was (B) — report the failure
                                // (the VM is left intact for `pi-vm resume`).
                                let t0 = std::time::Instant::now();
                                let deadline = t0 + Duration::from_secs(120);
                                let mut hint_printed = false;
                                loop {
                                    // A SIGTERM to the supervisor ends the
                                    // grace wait immediately and takes the
                                    // normal-stop path (the signal is
                                    // authoritative — the ssh 255 it caused
                                    // is not a failure). (Catches the race
                                    // where ssh's own signal-exit is reaped
                                    // before the handler flags `terminated`.)
                                    if terminated.load(Ordering::Relaxed) {
                                        eprintln!("interrupted — stopping the VM");
                                        stop_announced = true;
                                        break;
                                    }
                                    if ch_child.try_wait().ok().flatten().is_some() {
                                        guest_stopped = true;
                                        break;
                                    }
                                    if std::time::Instant::now() >= deadline {
                                        break;
                                    }
                                    if !hint_printed
                                        && t0.elapsed() >= Duration::from_secs(15)
                                    {
                                        eprintln!("guest still up — waiting for the shutdown to finish (a transient ssh failure will be reported when the window closes)");
                                        hint_printed = true;
                                    }
                                    thread::sleep(Duration::from_millis(500));
                                }
                                }
                            }
                            if c == 255 && !stop_announced && !guest_stopped {
                                // ssh died on its own while the guest is
                                // still up — a transient connection/auth
                                // failure, not a normal stop.
                                eprintln!("ssh session ended with a transport error (255) while the VM is still up — treating as an attach failure");
                                attach_failed = true;
                            }
                            // A stop we caused (timeout / SIGTERM) or a
                            // guest-initiated poweroff is a NORMAL stop:
                            // report 0 (a planned stop is not an error —
                            // and --rm deletes on Ok). A remote session
                            // status (1-254) propagates, docker-like.
                            break if stop_announced || guest_stopped { 0 } else { c };
                        }
                        Ok(None) => thread::sleep(Duration::from_millis(200)),
                        Err(e) => {
                            eprintln!("ssh attach failed: {e}");
                            attach_failed = true;
                            break 1;
                        }
                    }
                }
            },
        }
    } else {
        // non-interactive: run until CH exits, the timeout fires, or we're
        // interrupted
        println!("==> running in foreground (Ctrl-C stops the VM)");
        loop {
            thread::sleep(Duration::from_secs(1));
            if interrupted.load(Ordering::Relaxed) || terminated.load(Ordering::Relaxed) {
                eprintln!("interrupted — stopping the VM");
                interrupted_stop = true;
                break;
            }
            if tstp.swap(false, Ordering::Relaxed) {
                eprintln!("{SIGTSTP_HINT}");
            }
            if deadline.map(|d| std::time::Instant::now() >= d).unwrap_or(false) {
                eprintln!("timeout — stopping the VM");
                break;
            }
            if ch_child.try_wait().ok().flatten().is_some() {
                break;
            }
        }
        0
    };

    teardown(home, &meta, &mut ch_child, &mut vfs_child, &api_sock, rm);
    if attach_failed {
        // The VM was stopped by the teardown above (the disk persists).
        // Err — not Ok(1) — so a --rm caller does not delete it.
        return Err(BootFail::Other(format!(
            "ssh attach failed — the VM is stopped; reconnect with `pi-vm resume {}`",
            meta.id
        )));
    }
    if interrupted_stop {
        // Err so a --rm caller does not delete the interrupted run (the
        // README promises the disk is left intact for `pi-vm resume`).
        return Err(BootFail::Interrupted("interrupted (Ctrl-C / SIGTERM) — the VM was torn down".into()));
    }
    Ok(code)
}

/// Teardown: graceful GUEST poweroff (ssh `systemctl poweroff`) up to 120 s,
/// then SIGTERM (immediate host-side poweroff — unflushed data may be lost),
/// then SIGKILL, then virtiofsd.
///
/// IMPORTANT: CH's SIGTERM is NOT a guest shutdown — it calls vmm_shutdown()
/// -> vm_delete() (immediate host-side poweroff, vCPUs halted), so the
/// guest's page cache is lost. The `vmm.shutdown` API does the same. The
/// only data-safe stop is a guest-initiated poweroff: systemd stops units
/// (capped at 30 s by the bake), the kernel flushes the page cache, the
/// ACPI poweroff is handled (by the kernel itself in direct boot), and CH
/// exits on its own.
/// True if the child has already been reaped. try_wait on a reaped child
/// returns Err(ECHILD); without this check the wait loops below spin for the
/// full window when CH already exited (e.g. guest crash) and print a
/// misleading "did not exit in 60s" message.
fn child_reaped(ch: &mut Child) -> bool {
    match ch.try_wait() {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => e.raw_os_error() == Some(10), // ECHILD
    }
}

fn teardown(home: &VmHome, meta: &VmMeta, ch: &mut Child, vfs: &mut Option<Child>, _api_sock: &std::path::Path, rm: bool) {
    if rm {
        println!("==> stopping VM (--rm: it will be deleted after the stop)");
    } else {
        println!("==> stopping VM (disk persists; `pi-vm resume {}` reboots it)", meta.id);
    }
    // 1. Graceful: ask the GUEST to power off (ssh `systemctl poweroff`).
    // CH's SIGTERM is an immediate host-side poweroff (vm_delete), so the
    // only clean stop is a guest-initiated shutdown — the guest flushes its
    // page cache and CH exits on its own when the ACPI poweroff lands.
    if !child_reaped(ch) {
        // timeout 30: a stalled ssh session (slow guest I/O) must not block
        // the teardown — the poweroff command is usually delivered even
        // when the session's response stalls, and the wait below still
        // gives the guest time to finish shutting down.
        let _ = std::process::Command::new("timeout")
            .args(["30", "ssh"])
            .args(ssh_opts())
            .arg(format!("root@{}", meta.ip))
            .arg("systemctl poweroff")
            .output();
        // 2. Wait for the guest poweroff (CH exits on its own; the bake caps
        // the guest's systemd stop timeout at 30 s, so a healthy guest is
        // done well within this window). 90 s + the 30 s ssh window above
        // keeps the total stop bounded at ~120 s.
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        while !child_reaped(ch) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(200));
        }
    }
    // 3. SIGTERM — immediate host-side poweroff; unflushed data may be lost.
    // (Child::kill() sends SIGKILL on Unix — must use libc kill(15))
    if !child_reaped(ch) {
        eprintln!("   (guest did not power off in ~2 min — SIGTERM; unflushed data may be lost)");
        libc_kill(ch.id(), 15);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !child_reaped(ch) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(200));
        }
    }
    // 4. SIGKILL — last resort
    if !child_reaped(ch) {
        eprintln!("   (CH did not exit after SIGTERM — SIGKILL; the disk may need recreating)");
        libc_kill(ch.id(), 9);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !child_reaped(ch) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(200));
        }
    }
    // 5. virtiofsd
    if let Some(vfs) = vfs.as_mut() {
        let _ = vfs.kill();
        let _ = vfs.wait();
    }
    let _ = ch.wait();

    // 6. network (per-VM tap; the shared bridge + pool rules persist —
    //    `ensure_shared_network` self-heals on the next boot). Best
    //    effort: a missed cleanup orphans the tap on the shared bridge,
    //    which is harmless (the next create uses a new tap).
    state::cleanup_network(meta);

    // 7. state
    let mut m = meta.clone();
    m.state = VmState::Stopped;
    m.ch_pid = None;
    m.updated = state::now();
    let _ = m.save(home);
}

/// Make the kernel send SIGTERM to THIS process when its parent (the shell
/// that launched pi-vm) dies — e.g. the terminal is closed. With the
/// SIGTERM handler, that triggers the graceful teardown.
///
/// NOTE: this does NOT protect pi-vm's children — PR_SET_PDEATHSIG signals
/// the calling process on its parent's death, not the other way around.
/// The children get their own PR_SET_PDEATHSIG via child_pdeathsig().
#[cfg(unix)]
fn set_pdeathsig() {
    unsafe extern "C" {
        fn prctl(option: i32, arg2: i32) -> i32;
    }
    unsafe {
        prctl(1, 15); // PR_SET_PDEATHSIG=1, SIGTERM=15
    }
}
#[cfg(not(unix))]
fn set_pdeathsig() {}

/// Runs in the child after fork, before exec: when this process (the
/// supervisor) dies — for any reason, including SIGKILL — the kernel sends
/// SIGTERM to the child. CH's SIGTERM = vmm_shutdown (immediate host-side
/// poweroff), virtiofsd exits (its forked daemon carries its own
/// PR_SET_PDEATHSIG, so killing the parent kills both), the ssh client
/// exits (the remote session ends). This is what prevents `kill -9 pi-vm`
/// from orphaning the VM. It also sets SIGTSTP=SIG_IGN: a terminal Ctrl-Z
/// signals the whole foreground process group, and with the default action
/// the children (ssh, CH, virtiofsd) would stop — freezing the attached
/// session even though pi-vm catches the signal (the SIG_IGN disposition is
/// inherited across exec).
fn child_pdeathsig() -> std::io::Result<()> {
    #[cfg(unix)]
    unsafe extern "C" {
        fn prctl(option: i32, arg2: i32) -> i32;
        fn signal(signum: i32, handler: usize) -> usize;
    }
    #[cfg(unix)]
    unsafe {
        if prctl(1, 15) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        signal(signal_hook::consts::SIGTSTP, 1); // SIG_IGN
    }
    Ok(())
}

/// Like `child_pdeathsig`, but additionally detaches the child from the
/// supervisor's process group (`setsid`) and ignores SIGINT — used for
/// the VM daemons (cloud-hypervisor, virtiofsd), NOT for ssh.
///
/// Why: a Ctrl-C at the terminal delivers SIGINT to the whole foreground
/// process group. The daemons must not act on it:
/// - CH installs its own SIGINT handler, which exits *immediately* (a
///   hard power-off — no guest shutdown, verified empirically);
/// - virtiofsd has the default disposition (death) — the guest's
///   /workspace mount would vanish mid-session.
///
/// Both would happen before pi-vm's handler runs the graceful stop.
/// `setsid()` removes the daemons from the terminal's group, so a Ctrl-C
/// reaches only pi-vm (graceful handler) and ssh (which deliberately STAYS
/// in the group: it must forward the Ctrl-C to the guest session).
///
/// The `SIGINT=SIG_IGN` mask is defense in depth: it covers signals aimed
/// at the daemons' own (new) group. CH overrides the mask with its own
/// handler at startup — that is fine, since it is no longer in the
/// terminal's group; virtiofsd keeps the ignore (it installs no handler).
/// The setsid() call fails only if the child were already a
/// process-group leader, which a plain fork() child never is; on that
/// (impossible) failure the signal mask still applies.
fn child_pdeathsig_isolated() -> std::io::Result<()> {
    #[cfg(unix)]
    unsafe extern "C" {
        fn prctl(option: i32, arg2: i32) -> i32;
        fn signal(signum: i32, handler: usize) -> usize;
        fn setsid() -> i32;
    }
    #[cfg(unix)]
    unsafe {
        if prctl(1, 15) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        setsid(); // detach from the supervisor's process group (see docs)
        signal(signal_hook::consts::SIGTSTP, 1); // SIG_IGN
        signal(signal_hook::consts::SIGINT, 1);  // SIG_IGN
    }
    Ok(())
}

/// Attach child_pdeathsig to a spawn. (pre_exec is Unix-only; the stub
/// keeps non-Unix builds compiling — the tool itself is Linux-oriented.)
#[cfg(unix)]
pub fn with_pdeathsig(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(child_pdeathsig);
    }
}
#[cfg(not(unix))]
pub fn with_pdeathsig(_cmd: &mut Command) {}

#[cfg(unix)]
pub fn with_pdeathsig_isolated(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(child_pdeathsig_isolated);
    }
}
#[cfg(not(unix))]
pub fn with_pdeathsig_isolated(_cmd: &mut Command) {}

/// Send an arbitrary signal to a pid (Child::kill only sends SIGKILL).
#[cfg(unix)]
pub fn libc_kill(pid: u32, sig: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid as i32, sig); }
}
#[cfg(not(unix))]
pub fn libc_kill(_pid: u32, _sig: i32) {}

/// The dnsmasq-free host network: one bridge + one chosen /24 pool serve
/// every VM. Each VM gets a static IP from the pool (random free pick,
/// like the old per-VM allocator but within one /24 — no live-interface
/// check needed, the pool was chosen collision-free at install) delivered
/// on a per-VM config disk; the gateway is the bridge IP.
pub const BRIDGE: &str = "br-pi-vm";

pub struct HostNet {
    pub pool: String, // e.g. 172.16.42.0/24
    pub gateway: String, // e.g. 172.16.42.1
}

/// The host's VM network: the persisted choice from network.conf, or — on
/// the first run — a free /24 picked from 172.16.2-251 (a /24 is free
/// when no live interface with a prefix <= 24 sits inside it; a live
/// prefix < 24 covering 172.16/16 would swallow every candidate, in which
/// case the fallback below applies) — then persisted.
pub fn host_net(home: &VmHome) -> HostNet {
    let file = home.root.join("network.conf");
    if let Ok(txt) = std::fs::read_to_string(&file) {
        for line in txt.lines() {
            let mut f = line.splitn(2, '=');
            if f.next() == Some("pool") {
                let rest = f.next().unwrap_or("");
                let pool = rest.split_whitespace().next().unwrap_or("").to_string();
                let gateway = rest
                    .split("gateway=")
                    .nth(1)
                    .and_then(|g| g.split_whitespace().next())
                    .unwrap_or("")
                    .to_string();
                if !pool.is_empty() && !gateway.is_empty() {
                    return HostNet { pool, gateway };
                }
            }
        }
    }
    let live = std::process::Command::new("ip")
        .args(["-o", "addr", "show"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let mut taken: Vec<u8> = Vec::new();
    let mut all_taken = false;
    for line in live.lines() {
        let inet = line.split(" inet ").nth(1).unwrap_or("");
        let addr = inet.split(' ').next().unwrap_or("");
        let mut parts = addr.split('/');
        let ip4 = parts.next().unwrap_or("");
        let prefix: u8 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(32);
        let oct: Vec<u8> = ip4.split('.').filter_map(|o| o.parse().ok()).collect();
        if oct.len() != 4 || oct[0] != 172 || oct[1] != 16 {
            continue;
        }
        if prefix < 24 {
            all_taken = true;
        } else {
            taken.push(oct[2]);
        }
    }
    if all_taken {
        eprintln!("   WARN: no free 172.16.x.0/24 (a live 172.16/16-or-smaller interface covers them) — falling back to 172.16.2.0/24");
        let hn = HostNet {
            pool: "172.16.2.0/24".into(),
            gateway: "172.16.2.1".into(),
        };
        let _ = std::fs::write(&file, format!("pool={} gateway={}\n", hn.pool, hn.gateway));
        return hn;
    }
    let free: Vec<u8> = (2..=251).filter(|n| !taken.contains(n)).collect();
    let n = if free.is_empty() { 2 } else { free[state::rand_below(free.len())] };
    let hn = HostNet {
        pool: format!("172.16.{n}.0/24"),
        gateway: format!("172.16.{n}.1"),
    };
    if std::fs::write(&file, format!("pool={} gateway={}\n", hn.pool, hn.gateway)).is_err() {
        eprintln!("   WARN: could not persist the chosen pool to {} (it will be re-picked on the next run)", file.display());
    }
    hn
}

/// The pool's /24 octets (for allocate_ip; .1 is the gateway).
fn pool_range(pool: &str) -> (u8, u8, u8) {
    let oct: Vec<u8> = pool
        .split('/')
        .next()
        .unwrap_or("")
        .split('.')
        .filter_map(|o| o.parse().ok())
        .collect();
    let (a, b, c) = if oct.len() == 4 {
        (oct[0], oct[1], oct[2])
    } else {
        (172, 16, 2)
    };
    (a, b, c)
}

/// A static IP for a new VM: a random free address in the host pool
/// (.2-.254; .1 is the gateway). "Free" = not recorded in any existing
/// VM's meta (a stopped VM keeps its ip in meta.json until rm).
/// Pick a static pool IP (random free .2-.254, avoiding other VMs' meta
/// ips). The re-check loop closes the parallel-create race: a concurrent
/// create can land on the same pick between our list and our save, so
/// verify the pick is still unclaimed right before returning.
pub fn allocate_ip(home: &VmHome, pool: &str) -> Result<String, String> {
    let (a, b, c) = pool_range(pool);
    for _ in 0..8 {
        let used: Vec<String> = home
            .list_vm_ids()
            .iter()
            .filter_map(|id| VmMeta::load(home, id).ok())
            .filter_map(|m| if m.ip.is_empty() { None } else { Some(m.ip.clone()) })
            .collect();
        let free: Vec<u8> = (2..=254).filter(|n| !used.contains(&format!("{a}.{b}.{c}.{n}"))).collect();
        if free.is_empty() {
            return Err(format!("the VM pool {pool} is exhausted (no free .2-.254 address) — `pi-vm rm` some VMs"));
        }
        let ip = format!("{a}.{b}.{c}.{}", free[state::rand_below(free.len())]);
        // Re-check the pick (a concurrent create may have claimed it).
        let still_free: Vec<String> = home
            .list_vm_ids()
            .iter()
            .filter_map(|id| VmMeta::load(home, id).ok())
            .filter_map(|m| if m.ip.is_empty() { None } else { Some(m.ip.clone()) })
            .collect();
        if !still_free.contains(&ip) {
            return Ok(ip);
        }
    }
    Err(format!("could not allocate a free IP in {pool} (parallel creates are racing) — try again"))
}

/// Write the per-VM config disk (1 MB raw; plain text, NUL-padded — the
/// guest reads it straight off /dev/vdb, no mount). Deterministic device
/// position: it is CH's second disk, rootfs is first.
pub fn write_config_disk(home: &VmHome, meta: &VmMeta, hn: &HostNet) -> Result<(), String> {
    let path = meta.vm_dir(home).join("config.raw");
    let text = format!(
        "vm-ip={}\nvm-gw={}\nvm-mac={}\ninstance-id=pi-{}\nvm-disk-gb={}\n",
        meta.ip, hn.gateway, meta.mac, meta.id, meta.disk_gb
    );
    use std::io::Write;
    let mut f = std::fs::File::create(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(text.as_bytes()).map_err(|e| format!("write {}: {e}", path.display()))?;
    f.set_len(1 << 20).map_err(|e| format!("pad {}: {e}", path.display()))?;
    Ok(())
}

/// Bring up the shared bridge + pool-wide iptables rules (idempotent —
/// boot calls this before every VM; `pi-vm install` calls it once
/// ahead). Privileged via auto-sudo.
pub fn ensure_shared_network(home: &VmHome) -> Result<(), String> {
    let hn = host_net(home);
    if !run_ok_priv("ip", &["link", "show", BRIDGE]) {
        run_priv("ip", &["link", "add", BRIDGE, "type", "bridge"])?;
    }
    if !run_priv("ip", &["addr", "show", "dev", BRIDGE])
        .unwrap_or_default()
        .contains(&format!("{}/", hn.gateway))
    {
        run_priv("ip", &["addr", "add", &format!("{}/24", hn.gateway), "dev", BRIDGE])?;
    }
    run_priv("ip", &["link", "set", BRIDGE, "up"])?;
    // Explicit route for the pool: on the top-level host the pool is
    // on-link (the bridge IP sits in it) so this re-asserts the connected
    // route; at a nested level (the pool /24 differs from the outer VM's
    // own /24) it is what makes the pool reachable at all.
    run_priv("ip", &["route", "replace", &hn.pool, "dev", BRIDGE])?;
    let _ = run_ok_priv("sysctl", &["-w", "net.ipv4.ip_forward=1"]);
    // Pool-wide iptables rules (idempotent). The inter-VM isolation DROP
    // must match BEFORE the accepts (first-match wins): VMs reach the
    // internet via NAT but not each other (the per-VM /24 design gave
    // this by construction; on the shared bridge it is policy — the real
    // mechanism is bridge port isolation in boot(), this is the routed
    // fallback).
    if run_ok("which", &["iptables"]) {
        if !run_ok_priv("iptables", &["-C", "FORWARD", "-i", BRIDGE, "-o", BRIDGE, "-j", "DROP"]) {
            run_ok_priv("iptables", &["-I", "FORWARD", "1", "-i", BRIDGE, "-o", BRIDGE, "-j", "DROP"]);
        }
        if !run_ok_priv("iptables", &["-t", "nat", "-C", "POSTROUTING", "-s", &hn.pool, "-j", "MASQUERADE"])
            && !run_ok_priv("iptables", &["-t", "nat", "-A", "POSTROUTING", "-s", &hn.pool, "-j", "MASQUERADE"])
        {
            return Err(format!("could not add the NAT rule for {} (check iptables)", hn.pool));
        }
        if !run_ok_priv("iptables", &["-C", "FORWARD", "-i", BRIDGE, "-j", "ACCEPT"]) {
            run_ok_priv("iptables", &["-A", "FORWARD", "-i", BRIDGE, "-j", "ACCEPT"]);
        }
        if !run_ok_priv("iptables", &["-C", "FORWARD", "-o", BRIDGE, "-j", "ACCEPT"]) {
            run_ok_priv("iptables", &["-A", "FORWARD", "-o", BRIDGE, "-j", "ACCEPT"]);
        }
    }
    Ok(())
}

/// A raw HTTP PUT to the CH REST API over its unix socket (no curl
/// dependency). Returns (status code, response body). Used for the
/// snapshot-pool path (vm.snapshot at bake, vm.restore at boot).
pub fn api_put_unix(socket: &std::path::Path, path: &str, body: &str) -> Result<(u16, String), String> {
    use std::io::{Read, Write};
    let mut s = std::os::unix::net::UnixStream::connect(socket)
        .map_err(|e| format!("connect {socket:?}: {e}"))?;
    // CH's actix router does not honor Connection: close — it keeps the
    // connection open after the response, so read with a timeout and
    // parse whatever arrives (the status line comes first).
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let req = format!(
        "PUT {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 65536 {
                    break;
                }
            }
            Err(_) => break, // timeout or closed — use what we have
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, text))
}

/// Probe a VM's ssh (tcp open + a `true` round-trip) on a 200 ms cadence
/// for up to `cap_s` seconds. Used by the master-adoption renumbering
/// (wait for the master's baked-in IP, then the VM's own IP).
pub fn wait_ssh_ready(ip: &str, cap_s: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(cap_s);
    loop {
        if tcp_open(ip, 22)
            && Command::new("ssh")
                .args(ssh_opts())
                .arg(format!("root@{ip}"))
                .arg("true")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// The renumbering command, built separately so the single-quote
/// invariant (see renumber_guest) is unit-testable.
fn renumber_cmd(pre_ip: &str, new_ip: &str, subnet: &str, new_mac: &str) -> String {
    let prefix = subnet.rsplit('/').next().unwrap_or("24");
    // The pool gateway = the subnet base with last octet .1 (host_net's
    // gateway is always that; both the master and the VM live in the
    // same pool /24).
    let gw = subnet
        .split('/')
        .next()
        .map(|base| {
            let o: Vec<&str> = base.split('.').collect();
            if o.len() == 4 {
                format!("{}.{}.{}.1", o[0], o[1], o[2])
            } else {
                String::new()
            }
        })
        .unwrap_or_default();
    // Interface = whatever currently carries the master's address; the
    // fallback (the default route's device) covers a flushed address —
    // at this point the restored route table is still the snapshot's.
    let inner = format!(
        "IF=$(ip -o -4 addr show | grep -F {pre_ip}/ | head -1 | sed -E \"s/^[0-9]+: ([^ ]+).*/\\1/\"); \
         [ -n \"$IF\" ] || IF=$(ip route show default | sed -E \"s/.* dev ([^ ]+).*/\\1/\" | head -1); \
         [ -n \"$IF\" ] || exit 7; \
         ip link set dev $IF down && \
         ip link set dev $IF address {new_mac} && \
         ip link set dev $IF up && \
         ip addr del {pre_ip}/{prefix} dev $IF 2>/dev/null; \
         ip addr replace {new_ip}/{prefix} dev $IF && \
         ip route replace default via {gw}"
    );
    format!("setsid bash -c '{inner}' >/tmp/pi-vm-renumber.log 2>&1 </dev/null &")
}

/// Move a freshly restored guest from the master's baked-in (IP, MAC)
/// to the VM's own (the gateway is unchanged — same /24 pool): set the
/// new MAC + IP on the uplink, delete the baked-in IP, and re-add the
/// default route.
///
/// Three empirically-verified traps:
/// - the link-down FLUSHES the gateway route (the kernel drops routes
///   via a downed link) — the snapshot's default route does not survive
///   it, so the route is re-added after the up (missing it = no internet
///   for the adopted VM, ssh still works: the on-link route is enough);
/// - `ip addr replace` ADDS without removing other addresses of the same
///   prefix — the baked-in IP stays primary and pins the connected
///   route's src to the stale address (and leaves two restored guests
///   answering the master's IP at once) — so the old address is deleted
///   explicitly;
/// - deleting that address kills the ssh session CARRYING the command
///   (its source address disappears — a black-holed TCP session, and
///   ConnectTimeout does not bound an established one). So the script
///   runs DETACHED (setsid + redirected stdio): ssh returns immediately
///   and success is verified by the caller waiting for new_ip. An
///   attached run hangs forever; the failure log stays in the guest at
///   /tmp/pi-vm-renumber.log.
///
/// The inner script must contain NO single quotes (it is passed inside a
/// single-quoted bash -c). The later cold resume re-reads the VM's config
/// disk, which carries the same (IP, MAC), so the renumbering survives
/// reboots.
fn renumber_guest(pre_ip: &str, new_ip: &str, subnet: &str, new_mac: &str) -> Result<(), String> {
    let cmd = renumber_cmd(pre_ip, new_ip, subnet, new_mac);
    let out = Command::new("ssh")
        // keepalives: a black-holed session must never stall the
        // supervisor (the detach above makes this belt-and-braces)
        .args(["-o", "ServerAliveInterval=2", "-o", "ServerAliveCountMax=2"])
        .args(ssh_opts())
        .arg(format!("root@{pre_ip}"))
        .arg(&cmd)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("renumbering ssh launch failed: {}",
            String::from_utf8_lossy(&out.stderr).chars().take(200).collect::<String>()));
    }
    Ok(())
}

/// ssh options for talking to a VM's guest — the single source of truth
/// for every ssh connection this tool makes (boot probe, attach,
/// graceful poweroff, `pi-vm attach`): the host user's DEFAULT ssh key
/// (the baked image trusts it — `pi-vm bake --ssh-key`), no host-key
/// checking (the guest's host key is pi-vm's own; it persists on the
/// disk while the pool address can be re-assigned), BatchMode (so an
/// interactive prompt can never hang the supervisor), and a bounded
/// connection. The caller appends `[-t] root@<ip> <remote command...>` —
/// `-t` must come BEFORE the target (ssh treats everything after the
/// target as the command).
pub fn ssh_opts() -> Vec<String> {
    vec![
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
    ]
}

/// PUT /api/v1/vmm.shutdown over the CH unix-socket HTTP API (CH v53+).
/// WARNING: this calls vm_delete() in CH — abrupt teardown, NOT a graceful
/// poweroff. Only use for fire-and-forget cases (e.g. the bake prep boot,
/// where the guest already synced before signaling). Never use for VMs whose
/// disk must be re-opened later.
#[allow(dead_code)]
fn api_shutdown(api_sock: &std::path::Path) -> bool {
    use std::os::unix::net::UnixStream;
    let mut s = match UnixStream::connect(api_sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("   (api_shutdown: connect failed: {e})");
            return false;
        }
    };
    let req = b"PUT /api/v1/vmm.shutdown HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    // write_all: a partial write would leave the request truncated
    match s.write_all(req) {
        Ok(()) => {
            eprintln!("   (api_shutdown: request sent)");
            true
        }
        Err(e) => {
            eprintln!("   (api_shutdown: write failed: {e})");
            false
        }
    }
}

pub fn tcp_open(ip: &str, port: u16) -> bool {
    // connect_timeout is essential: a plain connect() blocks for the OS
    // TCP timeout (~130 s of dropped SYNs) while the guest's network is
    // down, which stalls the whole boot-wait loop.
    use std::net::ToSocketAddrs;
    match (ip, port).to_socket_addrs() {
        Ok(addrs) => {
            for a in addrs {
                if TcpStream::connect_timeout(&a, Duration::from_secs(2)).is_ok() {
                    return true;
                }
            }
            false
        }
        Err(_) => false,
    }
}

fn tail_file(p: &std::path::Path, n: usize) {
    if let Ok(data) = std::fs::read_to_string(p) {
        for line in data.lines().rev().take(n) {
            println!("   | {line}");
        }
    }
}

/// True if the qcow2 was cleanly closed (per `qemu-img info`). On any
/// failure (qemu-img missing, unreadable file) assume it is fine so a
/// diagnostic hiccup never blocks the boot.
fn qemu_img_cleanly_closed(disk: &std::path::Path) -> bool {
    run("qemu-img", &["info", disk.to_str().unwrap()])
        .map(|out| qemu_img_info_cleanly_closed(&out))
        .unwrap_or(true)
}

/// Parse the `qemu-img info` output: qemu prints the line
/// "cleanly shut down: no" ONLY when the image is dirty (qemu 8.2+;
/// older versions print no clean-state line at all), so the disk was
/// cleanly closed unless that line is present.
fn qemu_img_info_cleanly_closed(out: &str) -> bool {
    !out.contains("cleanly shut down: no")
}

/// The one-line hint printed when Ctrl-Z (SIGTSTP) arrives: the signal is
/// caught instead of suspending the process, so the session keeps running.
const SIGTSTP_HINT: &str = "Ctrl-Z ignored while attached — the VM stays up; use Ctrl-C or exit the guest session to stop it";

/// SIGINT (Ctrl-C) and SIGTERM set separate flags: Ctrl-C only interrupts
/// the boot-wait (while attached it goes to the remote session); SIGTERM
/// tears down in every state. SIGTSTP (Ctrl-Z) sets a flag the wait loops
/// turn into the hint — the default suspend action is replaced, so Ctrl-Z
/// no longer hangs the session.
fn install_signal_handler(interrupted: Arc<AtomicBool>, terminated: Arc<AtomicBool>, tstp: Arc<AtomicBool>) {
    // SIGINT is only honored on a tty: a non-interactive supervisor (e2e
    // gate, background run) must ignore stray group signals — they are
    // noise, not user intent. SIGTERM (stop) and SIGTSTP (Ctrl-Z hint)
    // are always honored.
    if std::io::stdin().is_terminal() {
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, interrupted);
    }
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, terminated);
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTSTP, tstp);
}

#[cfg(test)]
mod tests {
    use super::{attach_title, qemu_img_info_cleanly_closed, renumber_cmd};
    use crate::state::{VmHome, VmMeta, VmState};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    fn test_meta() -> VmMeta {
        VmMeta {
            id: "testtesttest".into(),
            name: Some("t".into()),
            created: 0,
            updated: 0,
            project: Some("/tmp/proj".into()),
            subnet: "172.16.55.0/24".into(),
            ip: "172.16.55.2".into(),
            mac: "52:54:00:00:00:55".into(),
            vcpus: 4,
            memory_mb: 8192,
            disk_gb: 80,
            kvm: true,
            squash_uid: 1000,
            base_image_sha256: None,
            state: VmState::Stopped,
            ch_pid: None,
            net_gen: Some(2),
        snapshot: false,
        }
    }

    /// The clean-state check must not run the refcount rebuild on a
    /// validly-closed disk: `qemu-img info` prints "cleanly shut down: no"
    /// only for a dirty image (a clean one gets no such line), so the
    /// absence of the line means cleanly closed.
    /// The renumbering runs inside a single-quoted `bash -c`, so the
    /// inner script must carry no single quotes, and it must be DETACHED
    /// (the command deletes the ssh session's own source address — see
    /// renumber_guest) and must restore the default route the link-down
    /// flushed.
    #[test]
    fn renumber_cmd_is_quote_safe_and_detached() {
        let c = renumber_cmd("172.16.9.114", "172.16.9.22", "172.16.9.0/24", "52:54:00:11:22:33");
        // exactly two single quotes: the bash -c wrapper itself
        assert_eq!(c.matches('\'').count(), 2, "inner script leaked a quote: {c}");
        assert!(c.starts_with("setsid bash -c '") && c.ends_with("</dev/null &"));
        assert!(c.contains("ip link set dev $IF address 52:54:00:11:22:33"));
        assert!(c.contains("ip addr del 172.16.9.114/24 dev $IF"));
        assert!(c.contains("ip addr replace 172.16.9.22/24 dev $IF"));
        assert!(c.contains("ip route replace default via 172.16.9.1"));
        // the link-down flushes the gateway route — re-adding it is the
        // difference between an adopted VM with and without internet
        assert!(c.contains("ip route replace default"));
    }

    #[test]
    fn qemu_img_info_cleanly_closed_check() {
        // clean: no clean-state line at all (the qemu 8.2+ format)
        assert!(qemu_img_info_cleanly_closed(
            "image: disk.qcow2\nfile format: qcow2\nvirtual size: 80 GiB (85899345920 bytes)\ndisk size: 1.2 GiB\n"));
        // dirty: the "cleanly shut down: no" line is present
        assert!(!qemu_img_info_cleanly_closed(
            "image: disk.qcow2\nfile format: qcow2\nvirtual size: 80 GiB (85899345920 bytes)\ndisk size: 1.2 GiB\ncleanly shut down: no\n"));
    }

    /// Ctrl-Z must not suspend the process: with the SIGTSTP flag
    /// registered, a SIGTSTP to the test process sets the flag and the
    /// process keeps running (a regression would stop the process and
    /// hang this test).
    #[test]
    fn sigtstp_is_caught_not_suspended() {
        let interrupted = Arc::new(AtomicBool::new(false));
        let terminated = Arc::new(AtomicBool::new(false));
        let tstp = Arc::new(AtomicBool::new(false));
        super::install_signal_handler(interrupted, terminated, tstp.clone());
        super::libc_kill(std::process::id(), 20); // SIGTSTP
        thread::sleep(Duration::from_millis(300));
        assert!(tstp.load(Ordering::Relaxed), "SIGTSTP flag not set");
        // reaching here = the process was not suspended
    }

    /// A child that applied child_pdeathsig (the pre_exec hook) must NOT
    /// stop on SIGTSTP: the children share pi-vm's process group, so a
    /// terminal Ctrl-Z reaches them too; the attach flow relies on them
    /// ignoring it (ssh/CH/virtiofsd keep running).
    #[test]
    #[cfg(target_os = "linux")]
    fn children_ignore_sigtstp() {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        unsafe {
            cmd.pre_exec(|| super::child_pdeathsig());
        }
        let mut child = cmd.spawn().expect("spawn sleep");
        thread::sleep(Duration::from_millis(300));
        super::libc_kill(child.id(), 20); // SIGTSTP
        thread::sleep(Duration::from_millis(300));
        // a stopped process shows state 'T' in /proc/<pid>/stat
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.id()))
            .expect("read /proc stat");
        let state = stat.rsplit(')').nth(1).expect("stat state field").chars().next().expect("state char");
        assert_ne!(state, 'T', "child stopped on SIGTSTP");
        let _ = child.kill();
        let _ = child.wait();

    }

    #[test]
    fn attach_title_uses_id_prefix_and_project_basename() {
        let m = test_meta();
        assert_eq!(attach_title(&m), "pi-vm testtest proj");
    }

    #[test]
    fn attach_title_without_project_omits_basename() {
        let mut m = test_meta();
        m.project = None;
        assert_eq!(attach_title(&m), "pi-vm testtest");
    }

    /// H1 regression: the VM-daemon hook must detach the child from the
    /// caller's process group (setsid) and ignore group-directed SIGINT /
    /// SIGTSTP — a terminal Ctrl-C must never reach cloud-hypervisor
    /// (whose own SIGINT handler is an immediate hard exit) or virtiofsd
    /// (whose default SIGINT disposition would drop the guest's
    /// /workspace mount).
    #[test]
    fn child_isolation_hook_detaches_group_and_ignores_ctrl_c() {
        use super::with_pdeathsig_isolated;
        unsafe extern "C" {
            fn getpgid(pid: i32) -> i32;
            fn getpgrp() -> i32;
        }
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        with_pdeathsig_isolated(&mut cmd);
        let mut child = cmd.spawn().expect("spawn sleep");
        let pid = child.id();
        // give pre_exec (setsid) time to run before exec
        thread::sleep(Duration::from_millis(300));
        let child_pgid = unsafe { getpgid(pid as i32) };
        assert_eq!(child_pgid, pid as i32, "child must be in its own process group (setsid)");
        assert_ne!(child_pgid, unsafe { getpgrp() }, "child must have left the caller's group");
        // a signal aimed at the child's own group (which contains only the
        // child) must be ignored, not fatal
        super::libc_kill(child_pgid as u32, 2); // SIGINT
        thread::sleep(Duration::from_millis(500));
        assert!(
            child.try_wait().ok().flatten().is_none(),
            "child died from a group SIGINT — the isolation hook is not applied"
        );
        super::libc_kill(child_pgid as u32, 20); // SIGTSTP
        thread::sleep(Duration::from_millis(300));
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid))
            .expect("read /proc stat");
        let state = stat.rsplit(')').nth(1).expect("stat state field").chars().next().expect("state char");
        assert_ne!(state, 'T', "child stopped on a group SIGTSTP");
        let _ = child.kill();
        let _ = child.wait();
    }
}


