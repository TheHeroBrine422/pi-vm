//! VM lifecycle: create / resume / boot / attach / teardown.
//!
//! Process model (the "one shell" design):
//!   - virtiofsd  -> background child (log file)
//!   - CH         -> background child (console file, api-socket)
//!   - ssh        -> foreground child that inherits the terminal (M2 attach)
//!
//! VM lifetime = this process's lifetime. Exiting the ssh session (Ctrl-C
//! while booting, or SIGTERM in any state) runs the teardown: graceful
//! guest poweroff -> SIGTERM -> SIGKILL -> kill virtiofsd -> delete the
//! bridge/tap/iptables.
//!
//! Orphan protection: every child carries PR_SET_PDEATHSIG=SIGTERM (set in
//! pre_exec, see child_pdeathsig) — if pi-vm dies for ANY reason, including
//! SIGKILL, the kernel SIGTERMs each child: CH's SIGTERM is an immediate
//! host-side poweroff, virtiofsd exits, the ssh client exits (the remote
//! session ends). `kill -9 pi-vm` therefore stops the VM instead of
//! orphaning it. (PR_SET_PDEATHSIG on pi-vm itself does NOT do this — it
//! only signals pi-vm when its parent, e.g. the shell, dies.)

use std::io::Write;
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::util::{run, run_ok, run_ok_priv, run_priv, runcmd_block, virtiofsd_path};
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
            BootFail::Other(_) => 1,
        }
    }
    pub fn msg(&self) -> &str {
        match self {
            BootFail::Preflight(s)
            | BootFail::BootFailed(s)
            | BootFail::SshUnreachable(s)
            | BootFail::InUse(s)
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
    pub extra_disk: u32,
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
    let (subnet, ip, mac) = state::allocate_subnet(home)?;
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
        subnet,
        ip,
        mac,
        vcpus: cfg.vcpus,
        memory_mb: cfg.memory_mb,
        disk_gb: cfg.disk_gb,
        extra_disk: cfg.extra_disk > 0,
        kvm: cfg.kvm,
        squash_uid: squash,
        state: VmState::Stopped,
        ch_pid: None,
    };
    let dir = meta.vm_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    meta.save(home)?;

    // ssh key
    let key = meta.ssh_key_path(home);
    if !key.exists() {
        run("ssh-keygen", &["-t", "ed25519", "-N", "", "-f", &key.to_string_lossy()])?;
    }

    // disk: copy pre-baked base + resize (fs grows on boot via the
    // runcmd / pi-vm-net growpart + btrfs resize — idempotent)
    let disk = meta.disk_path(home);
    run("cp", &[prepped.to_str().unwrap(), disk.to_str().unwrap()])?;
    run("qemu-img", &["resize", &disk.to_string_lossy(), &format!("{}G", meta.disk_gb)])?;
    if cfg.extra_disk > 0 {
        run("qemu-img", &[
            "create", "-f", "qcow2",
            &dir.join("extra.qcow2").to_string_lossy(),
            &format!("{}G", cfg.extra_disk),
        ])?;
    }

    println!("   created VM {} {} ({} vcpus, {} MB, {} GB disk)",
        meta.id, meta.name.as_deref().unwrap_or("(no name)"), meta.vcpus, meta.memory_mb, meta.disk_gb);
    Ok(meta)
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

/// The bridge's host IP for a subnet like 172.16.2.0/24 -> 172.16.2.1.
fn host_ip_of(subnet: &str) -> String {
    let base = subnet.split('/').next().unwrap_or("");
    let base = base.strip_suffix(".0").unwrap_or(base);
    format!("{base}.1")
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

/// Boot the VM and (if attach) drop into ssh. Returns the exit code.
pub fn boot(home: &VmHome, meta: VmMeta, attach: bool, console: bool, shell: bool) -> Result<i32, BootFail> {
    let boot_start = std::time::Instant::now();
    let _lock = state::VmLock::acquire(&meta, home)
        .map_err(BootFail::InUse)?;

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

    // --- network (idempotent, same as v1; privileged via auto-sudo) ---
    let br = meta.bridge_name();
    let tap = meta.tap_name();
    let host_ip = host_ip_of(&meta.subnet);
    if !run_ok_priv("ip", &["link", "show", &br]) {
        run_priv("ip", &["link", "add", &br, "type", "bridge"])
            .map_err(BootFail::Preflight)?;
    }
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
    // means the guest is unreachable, and the boot would burn the full
    // 180 s ssh timeout with no diagnostics ("No route to host").
    run_priv("ip", &["link", "set", &tap, "master", &br])
        .map_err(BootFail::Preflight)?;
    run_priv("ip", &["link", "set", &br, "up"])
        .map_err(BootFail::Preflight)?;
    run_priv("ip", &["link", "set", &tap, "up"])
        .map_err(BootFail::Preflight)?;
    if !run_priv("ip", &["addr", "show", "dev", &br]).unwrap_or_default().contains(&format!("{host_ip}/")) {
        run_priv("ip", &["addr", "add", &format!("{host_ip}/24"), "dev", &br])
            .map_err(BootFail::Preflight)?;
    }
    // Verify the network is actually up before booting CH (a silent failure
    // here used to surface 180 s later as "boot timed out").
    {
        let links = run_priv("ip", &["-o", "link", "show"]).unwrap_or_default();
        let br_line = links.lines().find(|l| l.contains(&format!(": {br}:"))).unwrap_or("");
        let tap_line = links.lines().find(|l| l.contains(&format!(": {tap}:"))).unwrap_or("");
        let (br_up, tap_up) = (link_is_up(br_line), link_is_up(tap_line));
        let tap_master = tap_line.contains(&format!("master {br}"));
        if !br_up || !tap_up || !tap_master {
            return Err(BootFail::Preflight(format!(
                "network setup failed: bridge {br} up={br_up}, tap {tap} up={tap_up} enslaved={tap_master} — the ip commands need CAP_NET_ADMIN (check that sudo works); the VM would be unreachable"
            )));
        }
    }
    run_ok_priv("sysctl", &["-w", "net.ipv4.ip_forward=1"]);
    if run_ok("which", &["iptables"]) {
        if !run_ok_priv("iptables", &["-t", "nat", "-C", "POSTROUTING", "-s", &meta.subnet, "-j", "MASQUERADE"]) {
            run_ok_priv("iptables", &["-t", "nat", "-A", "POSTROUTING", "-s", &meta.subnet, "-j", "MASQUERADE"]);
        }
        if !run_ok_priv("iptables", &["-C", "FORWARD", "-i", &br, "-j", "ACCEPT"]) {
            run_ok_priv("iptables", &["-A", "FORWARD", "-i", &br, "-j", "ACCEPT"]);
        }
        if !run_ok_priv("iptables", &["-C", "FORWARD", "-o", &br, "-j", "ACCEPT"]) {
            run_ok_priv("iptables", &["-A", "FORWARD", "-o", &br, "-j", "ACCEPT"]);
        }
    }

    // --- ssh key + cloud-init NoCloud ISO ---
    let key = meta.ssh_key_path(home);
    if !key.exists() {
        run("ssh-keygen", &["-t", "ed25519", "-N", "", "-f", &key.to_string_lossy()])
            .map_err(BootFail::Preflight)?;
    }
    let pubkey = std::fs::read(key.with_extension("pub"))
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .unwrap_or_default();

    let nocloud = meta.vm_dir(home).join("nocloud");
    let _ = std::fs::remove_dir_all(&nocloud);
    std::fs::create_dir_all(&nocloud)
        .map_err(|e| BootFail::Preflight(e.to_string()))?;
    let ts = state::now();
    std::fs::write(
        nocloud.join("meta-data"),
        format!(
            "instance-id: pi-{}-{ts}\nvm-ip: {}\nvm-host: {}\nvm-mac: {}\n",
            meta.id,
            meta.ip,
            host_ip_of(&meta.subnet),
            meta.mac
        ),
    )
    .map_err(|e| BootFail::Preflight(e.to_string()))?;
    std::fs::write(nocloud.join("user-data"), user_data(&meta, &pubkey))
        .map_err(|e| BootFail::Preflight(e.to_string()))?;
    // ssh-pubkey on the ISO: the baked image's pi-vm-net unit (the
    // cloud-init-independent fallback) installs it the same way the runcmd does.
    std::fs::write(nocloud.join("ssh-pubkey"), format!("{}\n", pubkey))
        .map_err(|e| BootFail::Preflight(e.to_string()))?;

    let iso = meta.vm_dir(home).join("nocloud.iso");
    run("genisoimage", &[
        "-output", &iso.to_string_lossy(),
        "-volid", "CIDATA", "-joliet", "-rock", "-uid", "0", "-gid", "0",
        &nocloud.to_string_lossy(),
    ])
    .map_err(BootFail::Preflight)?;
    // Pre-warm the ISO in the host page cache: the guest's first virtio-blk
    // read (the pi-vm-net unit's ISO scan) is slow on a cold cache, and that
    // slowness pushed ssh_ready out by minutes on slow hosts.
    let _ = std::fs::read(&iso);

    // --- orphan protection ---
    // (1) The children get PR_SET_PDEATHSIG=SIGTERM via pre_exec
    // (child_pdeathsig): if pi-vm dies — for any reason, including SIGKILL —
    // the kernel SIGTERMs each child: CH's SIGTERM is an immediate
    // host-side poweroff, virtiofsd exits, the ssh client exits. The VM
    // stops and doesn't leak (unflushed guest data may be lost — PLAN.md §3b.3).
    // (2) pi-vm itself gets PR_SET_PDEATHSIG, so a dead shell (the terminal
    // is closed) SIGTERMs pi-vm -> the SIGTERM handler tears down.
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
            with_pdeathsig(&mut cmd);
            let mut child = cmd
                .spawn()
                .map_err(|e| BootFail::Preflight(format!("spawn virtiofsd: {e}")))?;
            thread::sleep(Duration::from_secs(1));
            if child.try_wait().ok().flatten().is_some() {
                return Err(BootFail::Preflight("virtiofsd died at startup — see virtiofsd.log".into()));
            }
            Some(child)
        }
        None => None,
    };

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
        format!("path={},image_type=raw,readonly=on", iso.display()),
        "--net".into(),
        format!("tap={},mac={}", meta.tap_name(), meta.mac),
        "--console".into(),
        format!("file={}", meta.console_path(home).display()),
    ]);
    if meta.extra_disk {
        ch_args.push("--disk".into());
        ch_args.push(format!("path={},image_type=qcow2", meta.vm_dir(home).join("extra.qcow2").display()));
    }
    if meta.project.is_some() {
        ch_args.push("--fs".into());
        ch_args.push(format!(
            "tag=workspace,socket={}",
            meta.vm_dir(home).join("fs.sock").display()
        ));
    }
    let mut ch_cmd = Command::new(&ch);
    ch_cmd.args(&ch_args);
    ch_cmd.stdin(Stdio::null());
    ch_cmd.stdout(console_file.try_clone().unwrap());
    ch_cmd.stderr(console_file);
    with_pdeathsig(&mut ch_cmd);
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

    // record running state
    let mut meta = meta;
    meta.state = VmState::Running;
    meta.ch_pid = Some(ch_child.id() as i32);
    meta.updated = state::now();
    if let Err(e) = meta.save(home) {
        // state save failed — don't orphan the running children
        teardown(home, &meta, &mut ch_child, &mut vfs_child, &api_sock);
        return Err(BootFail::Other(e));
    }

    match &meta.project {
        Some(p) => println!("==> VM {} booting ({} vcpus, {} MB, share {} -> uid {squash})",
            meta.id, meta.vcpus, meta.memory_mb, p),
        None => println!("==> VM {} booting ({} vcpus, {} MB, no shared mount)",
            meta.id, meta.vcpus, meta.memory_mb),
    }
    println!("   connect: ssh -i {} root@{}", key.display(), meta.ip);

    // Signal handling: Ctrl-C (SIGINT) and SIGTERM set separate flags.
    // Ctrl-C interrupts the boot-wait only (while attached it goes to the
    // remote session); SIGTERM tears down in every state.
    let interrupted = Arc::new(AtomicBool::new(false));
    let terminated = Arc::new(AtomicBool::new(false));
    install_signal_handler(interrupted.clone(), terminated.clone());

    // --- wait for ssh ---
    let boot_deadline = 180;
    let mut waited = 0;
    let mut ready = false;
    let mut console_pos: u64 = 0;
    while waited < boot_deadline {
        thread::sleep(Duration::from_secs(2));
        waited += 2;
        if interrupted.load(Ordering::Relaxed) || terminated.load(Ordering::Relaxed) {
            break;
        }
        if ch_child.try_wait().ok().flatten().is_some() {
            teardown(home, &meta, &mut ch_child, &mut vfs_child, &api_sock);
            return Err(BootFail::BootFailed("CH exited during boot — see console.log".into()));
        }
        if console {
            // live-tail the console file (display only — the ssh probe below
            // still runs)
            if let Ok(data) = std::fs::read(meta.console_path(home))
                && data.len() as u64 > console_pos {
                    let start = (console_pos as usize).min(data.len());
                    print!("{}", String::from_utf8_lossy(&data[start..]));
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                    console_pos = data.len() as u64;
                }
        }
        if tcp_open(&meta.ip, 22) {
            let probe = Command::new("ssh")
                .args([
                    "-i", &key.to_string_lossy(),
                    "-o", "StrictHostKeyChecking=no",
                    "-o", "UserKnownHostsFile=/dev/null",
                    "-o", "BatchMode=yes",
                    "-o", "ConnectTimeout=5",
                    &format!("root@{}", meta.ip),
                    "true",
                ])
                .output();
            if probe.map(|o| o.status.success()).unwrap_or(false) {
                ready = true;
                break;
            }
        }
        if waited % 20 == 0 && !console {
            print!("   booting… ({waited}s)");
            std::io::Write::flush(&mut std::io::stdout()).ok();
        }
    }
    println!();
    if !ready {
        if interrupted.load(Ordering::Relaxed) || terminated.load(Ordering::Relaxed) {
            eprintln!("interrupted — tearing down");
        } else {
            eprintln!("VM did not become ssh-reachable in {boot_deadline}s — console tail:");
            tail_file(&meta.console_path(home), 20);
        }
        teardown(home, &meta, &mut ch_child, &mut vfs_child, &api_sock);
        return Err(BootFail::SshUnreachable("boot timed out".into()));
    }

    // Log the boot timing (helps diagnose slow boots: preflight = network +
    // ISO + virtiofsd + CH spawn; guest boot = CH spawn -> ssh-ready).
    let preflight = ch_spawned_at.duration_since(boot_start).as_secs_f64();
    let guest_boot = std::time::Instant::now().duration_since(ch_spawned_at).as_secs_f64();
    println!("   boot timing: preflight {:.1}s, guest boot {:.1}s, total {:.1}s to ssh-ready",
        preflight, guest_boot, preflight + guest_boot);

    // --- attach or foreground-wait ---
    let code = if attach {
        println!("==> attached — exiting the ssh session stops the VM (Ctrl-C goes to the remote session; SIGTERM stops the VM)");
        let key_s = key.to_string_lossy().into_owned();
        let target = format!("root@{}", meta.ip);
        let mut ssh = Command::new("ssh");
        ssh.arg("-i")
            .arg(&key_s)
            .arg("-o")
            .arg("StrictHostKeyChecking=no")
            .arg("-o")
            .arg("UserKnownHostsFile=/dev/null")
            .arg("-t")
            .arg(&target);
        // Drop into /workspace (baked into the image, so `cd` always works —
        // before virtiofs has mounted it's an empty dir, and the files
        // appear in the cwd when the mount lands). Default: the agent.
        // --shell: a plain interactive shell. Exiting the shell (or the
        // agent, then the shell) stops the VM.
        let attach_cmd = if shell {
            "cd /workspace; bash"
        } else {
            "cd /workspace; pi"
        };
        ssh.arg(attach_cmd);
        with_pdeathsig(&mut ssh);
        // Poll rather than block on status(): a SIGTERM to the supervisor
        // must tear the VM down even while attached (the old blocking
        // status() made `kill <pi-vm-pid>` a no-op — the flag was set but
        // never checked). Ctrl-C (SIGINT) is deliberately NOT checked here:
        // the terminal delivers it to the foreground group and ssh forwards
        // it to the remote session (docker-attach semantics).
        match ssh
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Err(e) => {
                eprintln!("ssh attach failed: {e} — the VM is still running; reconnect with `pi-vm resume {}`", meta.id);
                1
            }
            Ok(mut ssh) => loop {
                if terminated.load(Ordering::Relaxed) {
                    eprintln!("SIGTERM — stopping the ssh session and the VM");
                    libc_kill(ssh.id(), 15);
                }
                match ssh.try_wait() {
                    Ok(Some(s)) => break s.code().unwrap_or(0),
                    Ok(None) => thread::sleep(Duration::from_millis(200)),
                    Err(e) => {
                        eprintln!("ssh attach failed: {e} — the VM is still running; reconnect with `pi-vm resume {}`", meta.id);
                        break 1;
                    }
                }
            },
        }
    } else {
        // non-interactive: run until CH exits or we're interrupted
        println!("==> running in foreground (Ctrl-C stops the VM)");
        loop {
            thread::sleep(Duration::from_secs(1));
            if interrupted.load(Ordering::Relaxed) || terminated.load(Ordering::Relaxed) {
                break;
            }
            if ch_child.try_wait().ok().flatten().is_some() {
                break;
            }
        }
        0
    };

    teardown(home, &meta, &mut ch_child, &mut vfs_child, &api_sock);
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

fn teardown(home: &VmHome, meta: &VmMeta, ch: &mut Child, vfs: &mut Option<Child>, _api_sock: &std::path::Path) {
    println!("==> stopping VM (disk persists; `pi-vm resume {}` reboots it)", meta.id);
    // 1. Graceful: ask the GUEST to power off (ssh `systemctl poweroff`).
    // CH's SIGTERM is an immediate host-side poweroff (vm_delete), so the
    // only clean stop is a guest-initiated shutdown — the guest flushes its
    // page cache and CH exits on its own when the ACPI poweroff lands.
    if !child_reaped(ch) {
        let key = meta.ssh_key_path(home);
        // timeout 30: a stalled ssh session (slow guest I/O) must not block
        // the teardown — the poweroff command is usually delivered even
        // when the session's response stalls, and the wait below still
        // gives the guest time to finish shutting down.
        let _ = std::process::Command::new("timeout")
            .args([
                "30",
                "ssh",
                "-i",
                key.to_str().unwrap_or("/dev/null"),
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "BatchMode=yes",
                "-o", "ConnectTimeout=5",
                &format!("root@{}", meta.ip),
                "systemctl poweroff",
            ])
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

    // 6. network (bridge + tap + iptables). Best effort: the setup path is
    // idempotent, so a missed cleanup is healed on the next boot — but a
    // stale bridge would keep the subnet allocated (the live-interface
    // check in allocate_subnet sees its .1) and leak the kernel objects.
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
/// from orphaning the VM.
fn child_pdeathsig() -> std::io::Result<()> {
    #[cfg(unix)]
    unsafe extern "C" {
        fn prctl(option: i32, arg2: i32) -> i32;
    }
    #[cfg(unix)]
    unsafe {
        if prctl(1, 15) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Attach child_pdeathsig to a spawn. (pre_exec is Unix-only; the stub
/// keeps non-Unix builds compiling — the tool itself is Linux-oriented.)
#[cfg(unix)]
fn with_pdeathsig(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| child_pdeathsig());
    }
}
#[cfg(not(unix))]
fn with_pdeathsig(_cmd: &mut Command) {}

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

fn tcp_open(ip: &str, port: u16) -> bool {
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

/// Install SIGINT/SIGTERM handlers that set the flag (signal-hook::flag).
/// True if the qcow2 was cleanly closed (per `qemu-img info`). On any
/// failure (qemu-img missing, unreadable file) assume it is fine so a
/// diagnostic hiccup never blocks the boot.
fn qemu_img_cleanly_closed(disk: &std::path::Path) -> bool {
    run("qemu-img", &["info", disk.to_str().unwrap()])
        .map(|out| out.contains("cleanly shut down: yes"))
        .unwrap_or(true)
}

/// SIGINT (Ctrl-C) and SIGTERM set separate flags: Ctrl-C only interrupts
/// the boot-wait (while attached it goes to the remote session); SIGTERM
/// tears down in every state.
fn install_signal_handler(interrupted: Arc<AtomicBool>, terminated: Arc<AtomicBool>) {
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, interrupted);
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, terminated);
}

/// The cloud-init runcmd (ported from v1 start.sh — the pre-baked
/// base carries pi config + toolchain, so per-boot work is only:
/// network, sshd, disk grow, and — for mounted VMs — mount /workspace).
///
/// Each entry is written as a normal multi-line shell script and
/// concatenated to the one-line `bash -c '...'` entry by
/// `runcmd_block` (YAML plain scalar: no `: `, no single quotes).
///
/// Order matters for the 180 s boot budget: network + sshd run FIRST so ssh
/// is up even if the virtiofs mount retry (up to 120 s) is slow or fails
/// (v1's order put the mount first; v1 had no boot timeout — the user ssh'd
/// from a second terminal — so the reorder is a v2 robustness fix).
fn user_data(meta: &VmMeta, pubkey: &str) -> String {
    // Network: wait for the virtio NIC (it can appear late, during the
    // udev coldplug), then configure ip/link/route + DNS.
    let net = format!(
        r#"
systemctl disable --now NetworkManager 2>/dev/null;
systemctl disable --now systemd-resolved 2>/dev/null;
udevadm settle --timeout=30 2>/dev/null;
IF="";
for t in $(seq 1 60); do
  IF=$(for i in /sys/class/net/*; do
    n=$(basename "$i");
    case "$n" in lo|docker*|br-*|tap-*) ;; *)
      grep -qi "{mac}" "$i/address" 2>/dev/null && echo "$n";;
    esac;
  done | head -1);
  [ -n "$IF" ] && break;
  sleep 0.5;
done;
[ -n "$IF" ] || IF=$(ls /sys/class/net 2>/dev/null | grep -vE "^lo$|^docker|^br-|^tap-" | head -1);
ip addr replace {ip}/24 dev $IF 2>/root/neterr.log;
ip link set $IF up 2>>/root/neterr.log;
ip route replace default via {host} 2>>/root/neterr.log;
rm -f /etc/resolv.conf;
printf "nameserver 8.8.8.8\nnameserver 1.1.1.1\n" > /etc/resolv.conf
"#,
        mac = meta.mac,
        ip = meta.ip,
        host = host_ip_of(&meta.subnet),
    );
    // sshd: allow root login, install the per-VM pubkey. (No sshd
    // restart here — the baked pi-vm-net unit is the sync point: it
    // runs Before=sshd.service and restarts sshd last, so ssh only
    // becomes reachable once the /workspace mount is done.)
    let ssh = format!(
        r#"
grep -q "^PermitRootLogin yes" /etc/ssh/sshd_config 2>/dev/null || echo "PermitRootLogin yes" >> /etc/ssh/sshd_config;
mkdir -p /root/.ssh;
grep -qF "{pub}" /root/.ssh/authorized_keys 2>/dev/null || echo "{pub}" >> /root/.ssh/authorized_keys;
chmod 700 /root/.ssh;
chmod 600 /root/.ssh/authorized_keys
"#,
        pub = pubkey,
    );
    let mut s = String::from("#cloud-config\nruncmd:\n");
    s.push_str(&runcmd_block(&net));
    s.push('\n');
    s.push_str(&runcmd_block(&ssh));
    s.push('\n');
    // disk grow: the host resized the qcow2 to the requested size at
    // create; the root partition + btrfs fs must follow (growpart +
    // btrfs resize are idempotent — no-op when already at max). Runs
    // after network + sshd (the 180 s boot budget wants ssh up first),
    // before the slow virtiofs mount retry. The mkdir lock avoids
    // racing the baked pi-vm-net service, which runs the same check
    // on every boot.
    s.push_str(&runcmd_block(
        "exec >> /root/vm-debug.log 2>&1;\nD=vda;\n[ -b /dev/$D ] || D=$(ls /sys/block 2>/dev/null | grep -vE \"^(loop|ram|zram)\" | head -1);\nDS=$(blockdev --getsize64 /dev/$D 2>/dev/null);\nfor i in $(seq 1 30); do\n  PS=$(blockdev --getsize64 /dev/${D}3 2>/dev/null);\n  [ -n \"$PS\" ] && break;\n  sleep 1;\ndone;\nif [ -n \"$DS\" ] && [ -n \"$PS\" ] && [ $((DS - PS)) -gt 104857600 ]; then\n  mkdir /var/lock/pi-vm-grow 2>/dev/null && {\n    growpart /dev/$D 3 2>&1 | tail -1;\n    if grep -q \" / btrfs \" /proc/mounts 2>/dev/null; then\n      btrfs filesystem resize max / 2>&1 | tail -1;\n    fi;\n    rmdir /var/lock/pi-vm-grow;\n  } || true;\nfi",
    ));
    s.push('\n');
    if meta.project.is_some() {
        s.push_str("  - mkdir -p /workspace\n");
        s.push_str(&runcmd_block(
            "for i in $(seq 1 60); do
  mount -t virtiofs workspace /workspace 2>/dev/null && break;
  sleep 2;
done",
        ));
        s.push('\n');
    }
    if meta.extra_disk {
        s.push_str(&runcmd_block(
            "EXTRA=/dev/vdc;
if [ -b $EXTRA ]; then
  if [ ! -f /mnt/extra/.fsdone ]; then
    mkdir -p /mnt/extra;
    mkfs.xfs $EXTRA 2>&1 | tail -1;
    mount $EXTRA /mnt/extra 2>/dev/null && touch /mnt/extra/.fsdone;
  else
    mkdir -p /mnt/extra;
    mount $EXTRA /mnt/extra 2>/dev/null || true;
  fi;
fi",
        ));
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::user_data;
    use crate::state::{VmMeta, VmState};

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
            extra_disk: true,
            kvm: true,
            squash_uid: 1000,
            state: VmState::Stopped,
            ch_pid: None,
        }
    }

    /// The generated user-data must stay valid YAML: each runcmd entry is
    /// a multi-line plain scalar (one `bash -c '...'` per entry) with no
    /// `: ` sequences, no ` #` sequences (a `#` at line start or after a
    /// space terminates the scalar — comments are NOT allowed in the
    /// block), and exactly one opening + one closing single quote.
    #[test]
    fn user_data_runcmd_blocks_are_valid_yaml() {
        let s = user_data(&test_meta(), "ssh-ed25519 AAAA test");
        let entries: Vec<&str> = s
            .split("  - ")
            .map(|e| e.trim_start())
            .filter(|e| e.starts_with("bash -c '"))
            .collect();
        assert_eq!(entries.len(), 5, "expected 5 runcmd entries (net, ssh, grow, mount, extra)");
        for e in &entries {
            assert!(!e.contains(": "), "runcmd entry contains ': ' (breaks the YAML plain scalar): {}", e.chars().take(80).collect::<String>());
            assert!(!e.contains(" #"), "runcmd entry contains ' #' (starts a comment, breaks the YAML plain scalar): {}", e.chars().take(80).collect::<String>());
            assert_eq!(e.matches('\'').count(), 2, "unbalanced single quotes: {}", e.chars().take(80).collect::<String>());
        }
    }
}


