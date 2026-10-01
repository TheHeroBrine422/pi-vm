//! `pi-vm bake` — build the pre-baked base image (pi config + full toolchain).
//!
//! Port of vm/bake.sh. The prep boot (DIRECT KERNEL BOOT — the kernel
//! assets are extracted from the raw cloud image first; the toolchain
//! packages include no kernel upgrade) runs the runcmd: it copies the pi
//! config (from the ISO), installs the toolchain, touches the marker,
//! `sync`s, signals the host over the prep bridge, then `systemctl
//! poweroff` (best effort). The host watches for the signal; if CH has
//! not exited 30s later, the host sends `vmm.shutdown` over the CH API —
//! acceptable HERE because this is fire-and-forget: the guest already
//! `sync`ed before signaling, and a torn .part fails verification (the
//! .part is preserved for debugging).
//!
//! NOTE: never use `vmm.shutdown` for VMs whose disk must be re-opened later
//! (it calls vm_delete — see PLAN.md §3b.1).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use crate::util::runcmd_block;

use clap::Args;

use crate::state::{VmHome, now};
use crate::util::{run, run_ok, run_ok_priv, run_priv};

const PREP_NET: &str = "172.16.254.0/24";
const PREP_HOST_IP: &str = "172.16.254.1";
const PREP_VM_IP: &str = "172.16.254.2";
const PREP_BR: &str = "br-pi-bake";
const PREP_TAP: &str = "tap-pi-bake";
const PREP_MAC: &str = "52:54:00:00:00:2a";

#[derive(Args)]
pub struct Bake {
    /// pi config dir to bake in (default: $PI_VM_PI_DIR, else ./pi)
    #[arg(long)]
    pub pi_dir: Option<String>,
    /// Prep boot timeout in minutes (default 7)
    #[arg(long, default_value_t = 7)]
    pub timeout: u32,
}

/// Scope guard: on ANY exit path, kill CH/listener, remove staging, delete the
/// throwaway network. The .part is PRESERVED on failure (for debugging).
struct Cleanup {
    ch: Option<Child>,
    prep_dir: PathBuf,
    stage: Option<PathBuf>,
}

impl Cleanup {
    fn kill_ch(&mut self) {
        if let Some(ch) = self.ch.as_mut() {
            // SIGKILL is fine here: the .part is either verified (moved) or
            // preserved for debugging — never re-opened by a VM.
            crate::vm::libc_kill(ch.id(), 9);
            let _ = ch.wait();
        }
        self.ch = None;
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.kill_ch();
        if let Some(stage) = &self.stage {
            let _ = std::fs::remove_dir_all(stage);
        }
        let _ = std::fs::remove_dir_all(&self.prep_dir);
        run_ok_priv("ip", &["link", "del", PREP_BR]);
        run_ok_priv("ip", &["link", "del", PREP_TAP]);
        run_ok_priv("iptables", &["-t", "nat", "-D", "POSTROUTING", "-s", PREP_NET, "-j", "MASQUERADE"]);
        run_ok_priv("iptables", &["-D", "FORWARD", "-i", PREP_BR, "-j", "ACCEPT"]);
        run_ok_priv("iptables", &["-D", "FORWARD", "-o", PREP_BR, "-j", "ACCEPT"]);
    }
}

impl Bake {
    pub fn run(&self, home: &VmHome) -> i32 {
        // --- arguments / preflight ---
        let pi_dir = match self.resolve_pi_dir() {
            Some(p) => p,
            None => {
                eprintln!("error: no pi config dir found (pass --pi-dir, set PI_VM_PI_DIR, or run from a dir containing pi/)");
                return 2;
            }
        };
        let ch = home.ch_binary();
        let img = home.cloud_image();
        let prepped = home.prepped_image();
        let part = prepped.with_file_name("fedora44-cloud-prepped.qcow2.part");
        let prep_dir = home.root.join("prep");
        let iso = prep_dir.join("nocloud.iso");
        let console = home.root.join("prep-console.log");
        let api_sock = prep_dir.join("api.sock");

        if !ch.exists() {
            eprintln!("error: CH binary not found: {} (run `pi-vm install`)", ch.display());
            return 2;
        }
        if !img.exists() {
            eprintln!("error: cloud image not found: {} (run `pi-vm install`)", img.display());
            return 2;
        }
        if !run_ok("which", &["genisoimage"]) {
            eprintln!("error: genisoimage not found (run `pi-vm install`)");
            return 2;
        }
        if !run_ok("which", &["guestfish"]) {
            eprintln!("error: guestfish not found (dnf install libguestfs)");
            return 2;
        }
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("WARN: /dev/kvm not accessible — CH requires KVM (no TCG mode) and the prep boot will fail without it");
        }

        let mut cleanup = Cleanup {
            ch: None,
            prep_dir: prep_dir.clone(),
            stage: None,
        };

        // --- [1/3] stage the pi config + copy the cloud image ---
        println!("==> [1/3] staging pi config ({}) + copying the cloud image", pi_dir.display());
        let stage = PathBuf::from(format!("/tmp/pi-vm-bake-stage-{}", now()));
        if let Err(e) = std::fs::create_dir_all(&stage) {
            eprintln!("error: create {}: {e}", stage.display());
            return 2;
        }
        cleanup.stage = Some(stage.clone());
        // cp -a <pi_dir>/. <stage>/pi/
        if let Err(e) = run("cp", &["-a", &format!("{}/.", pi_dir.display()), &format!("{}/pi", stage.display())]) {
            eprintln!("error: {e}");
            return 2;
        }
        let _ = std::fs::remove_file(&part);
        if let Err(e) = run("cp", &[img.to_str().unwrap(), part.to_str().unwrap()]) {
            eprintln!("error: {e}");
            return 2;
        }
        // Direct-boot the prep VM: extract the kernel assets from the RAW
        // cloud image — the prep boot doesn't change the kernel (none of
        // the toolchain packages is a kernel upgrade), so these are valid
        // for the prep boot. The re-extraction from the baked image (in
        // [3/3]) keeps the assets in sync with the result.
        extract_boot_assets(&img, home);
        if crate::vm::direct_boot_args(home).is_none() {
            eprintln!("error: direct-boot kernel assets unavailable (extraction failed — see above) — the prep boot cannot start; re-run `pi-vm bake`");
            return 2;
        }

        // --- [2/3] prep boot ---
        println!("==> [2/3] prep boot (throwaway bridge {PREP_BR} on {PREP_NET}; installs toolchain, then powers off — this takes a while)");
        let _ = std::fs::create_dir_all(&prep_dir);
        let nocloud = prep_dir.join("nocloud");
        let _ = std::fs::create_dir_all(&nocloud);
        std::fs::write(nocloud.join("meta-data"), format!("instance-id: pi-bake-{}\n", now()))
            .expect("write meta-data");
        if let Err(e) = run("cp", &["-a", &format!("{}/pi", stage.display()), &format!("{}/pi", nocloud.display())]) {
            eprintln!("error: {e}");
            return 2;
        }
        std::fs::write(nocloud.join("user-data"), user_data())
            .expect("write user-data");
        if let Err(e) = run(
            "genisoimage",
            &[
                "-output", iso.to_str().unwrap(),
                "-volid", "CIDATA", "-joliet", "-rock", "-uid", "0", "-gid", "0",
                &nocloud.to_string_lossy(),
            ],
        ) {
            eprintln!("error: {e}");
            return 2;
        }

        // throwaway bridge + tap + NAT (idempotent; removed by the guard;
        // privileged via auto-sudo)
        if !run_ok_priv("ip", &["link", "show", PREP_BR]) {
            if let Err(e) = run_priv("ip", &["link", "add", PREP_BR, "type", "bridge"]) {
                eprintln!("error: {e} (need CAP_NET_ADMIN — run as root or ensure sudo works)");
                return 2;
            }
        } else {
            println!("   bridge {PREP_BR} already exists");
        }
        if !run_ok_priv("ip", &["link", "show", PREP_TAP]) {
            if let Err(e) = run_priv("ip", &["tuntap", "add", "dev", PREP_TAP, "mode", "tap"]) {
                eprintln!("error: {e}");
                return 2;
            }
        } else {
            println!("   tap {PREP_TAP} already exists");
        }
        // Let the invoking (non-root) user open the TAP so CH can use it
        if !is_root()
            && let Ok(user) = std::env::var("USER") {
                run_ok_priv("chown", &[&user, &format!("/dev/{PREP_TAP}")]);
            }
        run_ok_priv("ip", &["link", "set", PREP_TAP, "master", PREP_BR]);
        run_ok_priv("ip", &["link", "set", PREP_BR, "up"]);
        run_ok_priv("ip", &["link", "set", PREP_TAP, "up"]);
        if !run_priv("ip", &["addr", "show", "dev", PREP_BR]).unwrap_or_default().contains(PREP_HOST_IP) {
            run_ok_priv("ip", &["addr", "add", &format!("{PREP_HOST_IP}/24"), "dev", PREP_BR]);
        }
        run_ok_priv("sysctl", &["-w", "net.ipv4.ip_forward=1"]);
        if run_ok("which", &["iptables"]) {
            if !run_ok_priv("iptables", &["-t", "nat", "-C", "POSTROUTING", "-s", PREP_NET, "-j", "MASQUERADE"]) {
                run_ok_priv("iptables", &["-t", "nat", "-A", "POSTROUTING", "-s", PREP_NET, "-j", "MASQUERADE"]);
            }
            if !run_ok_priv("iptables", &["-C", "FORWARD", "-i", PREP_BR, "-j", "ACCEPT"]) {
                run_ok_priv("iptables", &["-A", "FORWARD", "-i", PREP_BR, "-j", "ACCEPT"]);
            }
            if !run_ok_priv("iptables", &["-C", "FORWARD", "-o", PREP_BR, "-j", "ACCEPT"]) {
                run_ok_priv("iptables", &["-A", "FORWARD", "-o", PREP_BR, "-j", "ACCEPT"]);
            }
        }

        // signal listener: the runcmd connects to the bridge IP:9999 once the
        // toolchain + marker are on disk (after a `sync`).
        let (tx, rx) = mpsc::channel();
        let timeout_secs: u64 = self.timeout as u64 * 60;
        let _listener = thread::spawn(move || {
            let Ok(lis) = TcpListener::bind((PREP_HOST_IP, 9999)) else {
                eprintln!("   (listener bind failed)");
                return;
            };
            let _ = lis.set_nonblocking(true);
            let deadline = Instant::now() + Duration::from_secs(timeout_secs);
            loop {
                match lis.accept() {
                    Ok((mut conn, _)) => {
                        let mut buf = [0u8; 100];
                        let _ = conn.read(&mut buf);
                        let _ = tx.send(true);
                        return;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() > deadline {
                            return;
                        }
                        thread::sleep(Duration::from_millis(500));
                    }
                    Err(_) => return,
                }
            }
        });

        println!("   booting prep VM (console: {})", console.display());
        let mut ch_args: Vec<String> = vec![
            "--cpus".into(), "boot=2".into(),
            "--memory".into(), "size=4096M".into(),
        ];
        ch_args.extend(crate::vm::direct_boot_args(home).expect("direct_boot_args checked above"));
        ch_args.extend([
            "--disk".into(), format!("path={},image_type=qcow2", part.display()),
            "--disk".into(), format!("path={},image_type=raw,readonly=on", iso.display()),
            "--net".into(), format!("tap={PREP_TAP},mac={PREP_MAC}"),
            "--api-socket".into(), format!("path={}", api_sock.display()),
            "--console".into(), format!("file={}", console.display()),
        ]);
        let ch_child = match Command::new(&ch)
            .args(&ch_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: spawn CH: {e}");
                return 2;
            }
        };
        cleanup.ch = Some(ch_child);

        // wait loop: overall timeout; on the guest's signal, give the guest's
        // poweroff 30 s, then fall back to the CH API. SIGINT/SIGTERM set
        // the flag (v1's trap covered Ctrl-C; the Cleanup guard then runs).
        let interrupted = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, interrupted.clone());
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, interrupted.clone());
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let mut signal_seen = false;
        let mut rc: i32 = 0;
        let ch_ref = &mut cleanup.ch;
        loop {
            if interrupted.load(Ordering::Relaxed) {
                eprintln!("interrupted — tearing down (the .part is preserved)");
                cleanup.kill_ch();
                rc = 130;
                break;
            }
            // did CH exit?
            if let Some(status) = ch_ref.as_mut().unwrap().try_wait().ok().flatten() {
                if !status.success() {
                    rc = status.code().unwrap_or(1);
                }
                break;
            }
            // guest signal?
            if !signal_seen && rx.try_recv().is_ok() {
                signal_seen = true;
                println!("   guest signal received — waiting up to 30 s for the guest poweroff");
                let s_deadline = Instant::now() + Duration::from_secs(30);
                while ch_ref.as_mut().unwrap().try_wait().ok().flatten().is_none()
                    && Instant::now() < s_deadline
                {
                    thread::sleep(Duration::from_secs(1));
                }
                if ch_ref.as_mut().unwrap().try_wait().ok().flatten().is_none() {
                    println!("   guest poweroff did not complete — sending vmm.shutdown via the CH API");
                    if !api_shutdown(&api_sock) {
                        println!("   (vmm.shutdown API call failed — relying on the overall timeout)");
                    }
                    let w_deadline = Instant::now() + Duration::from_secs(60);
                    while ch_ref.as_mut().unwrap().try_wait().ok().flatten().is_none()
                        && Instant::now() < w_deadline
                    {
                        thread::sleep(Duration::from_secs(1));
                    }
                }
                // if CH exited during the 30s/60s windows, pick up its status
                if let Some(status) = ch_ref.as_mut().unwrap().try_wait().ok().flatten() {
                    if !status.success() {
                        rc = status.code().unwrap_or(1);
                    }
                    break;
                }
            }
            if Instant::now() > deadline {
                println!("   prep boot timed out after {} min — killing CH", self.timeout);
                cleanup.kill_ch();
                rc = 124;
                break;
            }
            thread::sleep(Duration::from_secs(3));
        }
        if let Some(mut ch) = cleanup.ch.take() {
            let _ = ch.wait();
        }

        if rc != 0 {
            dump_bake_logs(&part);
            if rc == 124 {
                eprintln!("error: prep boot timed out after {} min — see {} (the .part is preserved at {} for debugging; re-run when fixed)", self.timeout, console.display(), part.display());
            } else if rc == 130 {
                eprintln!("error: interrupted — the prep boot was stopped (the .part is preserved at {} for debugging; re-run when fixed)", part.display());
            } else {
                eprintln!("error: prep boot failed (CH exit {rc}) — see {} (the .part is preserved at {} for debugging; re-run when fixed)", console.display(), part.display());
            }
            return rc;
        }

        // --- [3/3] verify the bake, then move the .part into place ---
        println!("==> [3/3] verifying the baked image");
        println!("   waiting for the final writeback to settle (up to ~3 min)...");
        if !verify_bake(&part) {
            dump_bake_logs(&part);
            eprintln!("error: verification failed: marker / pi config missing in the baked image — see {} (the .part is preserved at {} for debugging; re-run when fixed)", console.display(), part.display());
            return 3;
        }
        // Re-extract the direct-boot assets from the baked image (best
        // effort: if this fails, the assets extracted from the raw cloud
        // image remain — still valid, the prep boot doesn't change the
        // kernel).
        extract_boot_assets(&part, home);
        // Install the cloud-init-independent network + ssh fallback
        // (best effort: a failure leaves the runcmd as the only path).
        write_net_fallback(&part);
        if let Err(e) = std::fs::rename(&part, &prepped) {
            eprintln!("error: rename {} -> {}: {e}", part.display(), prepped.display());
            return 1;
        }
        // Fix the refcount table: the Fedora cloud image's refcount table
        // is undersized (1 refcount cluster covers 2 GiB, but the file is
        // 2.16 GiB), so CH's refcount rebuild fails with InvalidClusterIndex
        // after an unclean shutdown (pi-vm killed, or the SIGTERM fallback).
        // A full qemu-img convert rewrites the refcount table correctly, so
        // the image survives an unclean shutdown. One-time cost at bake time.
        println!("   fixing the refcount table (qemu-img convert)");
        let fixed = prepped.with_file_name("fedora44-cloud-prepped.qcow2.fixed");
        match run("qemu-img", &["convert", "-O", "qcow2", prepped.to_str().unwrap(), fixed.to_str().unwrap()]) {
            Ok(_) => {
                if let Err(e) = std::fs::rename(&fixed, &prepped) {
                    eprintln!("   (refcount fix rename failed: {e} — the image works for clean shutdowns but not after an unclean one)");
                    let _ = std::fs::remove_file(&fixed);
                }
            }
            Err(e) => {
                eprintln!("   (refcount fix failed: {e} — the image works for clean shutdowns but not after an unclean one)");
                let _ = std::fs::remove_file(&fixed);
            }
        }
        println!("   pre-baked base image ready: {}", prepped.display());
        println!();
        println!("bake complete. next:");
        println!("  pi-vm create <your-project-dir>");
        0
    }

    fn resolve_pi_dir(&self) -> Option<PathBuf> {
        if let Some(p) = &self.pi_dir {
            let pb = PathBuf::from(p);
            if pb.is_dir() {
                return Some(pb);
            }
            eprintln!("WARN: --pi-dir {p} is not a directory — falling back to $PI_VM_PI_DIR / ./pi");
        }
        if let Ok(p) = std::env::var("PI_VM_PI_DIR") {
            let pb = PathBuf::from(p);
            if pb.is_dir() {
                return Some(pb);
            }
        }
        let cwd_pi = PathBuf::from("pi");
        cwd_pi.is_dir().then_some(cwd_pi)
    }
}

fn is_root() -> bool {
    run("id", &["-u"]).map(|u| u == "0").unwrap_or(true)
}

/// PUT /api/v1/vmm.shutdown over the CH unix-socket HTTP API.
/// Fire-and-forget only (see module doc — never for VMs that must reopen).
fn api_shutdown(api_sock: &std::path::Path) -> bool {
    use std::os::unix::net::UnixStream;
    let mut s = match UnixStream::connect(api_sock) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let req = b"PUT /api/v1/vmm.shutdown HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    s.write_all(req).is_ok()
}

/// The cloud-init runcmd (ported from v1 bake.sh). Each entry is
/// written as a normal multi-line shell script and concatenated to the
/// one-line `bash -c '...'` entry by `runcmd_block` (YAML plain scalar:
/// no `: `, no single quotes).
fn user_data() -> String {
    // Network for the prep boot (same as the per-VM runcmd, but logs to
    // /root/bake-net.log — the on-disk log the bake dumps on failure).
    let net = format!(
        r#"
exec > /root/bake-net.log 2>&1;
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
echo "IF=$IF";
ip addr replace {vm}/24 dev $IF;
ip link set $IF up;
ip route replace default via {host};
rm -f /etc/resolv.conf;
printf "nameserver 8.8.8.8\nnameserver 1.1.1.1\n" > /etc/resolv.conf;
echo "NET-DONE rc=$?"
"#,
        mac = PREP_MAC,
        vm = PREP_VM_IP,
        host = PREP_HOST_IP,
    );
    // Copy the pi config from the NoCloud ISO into /root/.pi/agent
    // (the guest writes its own filesystem — the only reliable path).
    let pi_config = r#"
exec >> /root/bake-debug.log 2>&1;
mkdir -p /mnt/nocloud;
if ! { timeout 15 mount -o ro /dev/vdb /mnt/nocloud 2>/dev/null && [ -d /mnt/nocloud/pi ]; }; then
  umount /mnt/nocloud 2>/dev/null;
  SEED="";
  for d in /sys/block/*; do
    n=$(basename "$d");
    case "$n" in loop*|ram*) continue;; esac;
    [ -b "/dev/$n" ] || continue;
    case "$SEED" in *" $n "*) continue;; esac;
    SEED="$SEED $n ";
    sz=$(cat "/sys/block/$n/size" 2>/dev/null);
    [ -n "$sz" ] && [ "$sz" -gt 200000 ] && continue;
    if timeout 5 blkid "/dev/$n" 2>/dev/null | grep -q iso9660; then
      timeout 15 mount -o ro "/dev/$n" /mnt/nocloud 2>/dev/null && [ -d /mnt/nocloud/pi ] && break;
    fi;
  done;
fi;
if [ -d /mnt/nocloud/pi ]; then
  mkdir -p /root/.pi/agent;
  cp -a /mnt/nocloud/pi/. /root/.pi/agent/;
  chown -R root:root /root/.pi/agent;
  echo "pi config copied from ISO ($(ls /root/.pi/agent | tr "\n" " "))";
else
  echo "pi config NOT FOUND on ISO, iso9660 mount count $(mount | grep -c iso9660)";
fi
"#;
    // The toolchain install (the long part) + bake marker + BAKE-SIGNAL.
    // NOTE: no `#` comments in this block — a `#` at the start of a
    // continuation line (or a mid-line ` #`) terminates the YAML plain
    // scalar and breaks cloud-init's user-data parse (see the test).
    let toolchain = format!(
        r#"
exec >> /root/bake-debug.log 2>&1;
echo "pi-vm baking toolchain...";
(timeout 300 dnf install -y git curl wget ripgrep fd xz bzip2 tar gzip xfsprogs python3 python3-pip cargo uv which cloud-utils-growpart btrfs-progs 2>&1 | tail -3;
 echo "dnf1 rc=${{PIPESTATUS[0]}}") || true;
(timeout 600 dnf install -y docker docker-compose 2>&1 | tail -2;
 systemctl enable --now docker 2>&1 | tail -1;
 echo "dnf2 rc=${{PIPESTATUS[0]}}") || true;
(timeout 300 dnf install -y nodejs npm 2>&1 | tail -2;
 echo "dnf3 rc=${{PIPESTATUS[0]}}") || true;
(timeout 300 dnf install -y rust clippy 2>&1 | tail -2;
 echo "dnf4 rc=${{PIPESTATUS[0]}}") || true;
(git config --system --replace-all safe.directory /workspace 2>&1 | tail -1;
 echo "gitsafe rc=${{PIPESTATUS[0]}}") || true;
(git config --system --replace-all user.name "Caleb Jones" 2>&1 | tail -1;
 git config --system --replace-all user.email "caleb@calebgj.io" 2>&1 | tail -1;
 echo "gitid rc=${{PIPESTATUS[0]}}") || true;
(timeout 300 npm install -g --ignore-scripts @earendil-works/pi-coding-agent 2>&1 | tail -3;
 echo "npm rc=${{PIPESTATUS[0]}}") || true;
(pi install npm:pi-web-access 2>&1 | tail -1;
 echo "piw rc=${{PIPESTATUS[0]}}") || true;
(pi install npm:pi-subagents 2>&1 | tail -1;
 echo "pis rc=${{PIPESTATUS[0]}}") || true;
(pi install npm:pi-goal-x 2>&1 | tail -1;
 echo "pgx rc=${{PIPESTATUS[0]}}") || true;
(timeout 300 pi update --extensions 2>&1 | tail -3;
 echo "piupd rc=${{PIPESTATUS[0]}}") || true;
grep -q SEARXNG_BASE_URL /etc/environment 2>/dev/null || echo "SEARXNG_BASE_URL=https://search.aandt.io/" >> /etc/environment;
grep -q SEARXNG_BASE_URL /root/.bashrc 2>/dev/null || echo "export SEARXNG_BASE_URL=https://search.aandt.io/" >> /root/.bashrc;
grep -q DefaultTimeoutStopSec /etc/systemd/system.conf 2>/dev/null || printf "DefaultTimeoutStopSec=30s\nFinalKillSignal=SIGKILL\n" >> /etc/systemd/system.conf;
if command -v pi >/dev/null 2>&1; then
  touch /root/.pi-vm-setup-done;
  echo "marker created (pi at $(command -v pi))";
else
  echo "marker SKIPPED (no pi)";
fi;
sync;
(echo BAKE-SIGNAL > /dev/tcp/{host}/9999) 2>/dev/null || true;
sleep 5;
systemctl poweroff
"#,
        host = PREP_HOST_IP,
    );
    let mut s = String::from("#cloud-config\nruncmd:\n");
    s.push_str(&runcmd_block(&net));
    s.push('\n');
    s.push_str(&runcmd_block(pi_config));
    s.push('\n');
    s.push_str(&runcmd_block(&toolchain));
    s.push('\n');
    s
}

/// The cloud-init-independent network + ssh fallback, written into the
/// baked image. cloud-init's NoCloud seed detection is flaky on resume
/// boots (it skips the new seed on some boots — the runcmd then never
/// runs and the VM is unreachable); this unit reads the ISO directly,
/// so the network + ssh setup always happen. Idempotent — safe to run
/// alongside the runcmd. The late timer re-applies the config 20 s in
/// (covers the udev interface-rename race, which wipes an early config).
const NET_SCRIPT: &str = r#"#!/bin/sh
# pi-vm per-boot setup. A SINGLE run with an internal convergence loop:
# for up to 60 s (1 s apart), each check is a single non-blocking attempt,
# and the loop reapplies whatever is missing until everything has converged
# (then it exits early). This gets the VM into the correct state as soon as
# the underlying devices are ready, and self-heals (e.g. the udev
# interface-rename race) continuously for a minute. Idempotent — safe to run
# alongside the runcmd (which does the same network + ssh work).
#
# The checks:
#   1. virtiofs mount  -> /workspace (the shared dir; N/A for --no-mount)
#   2. extra disk      -> /mnt/extra (N/A when --extra-disk is unset)
#   3. NoCloud ISO     -> /mnt/nocloud (provides the network + ssh config)
#   4. network         -> ip/link/route from the ISO's meta-data
#   5. ssh             -> pubkey from the ISO + sshd running
#   6. disk grow       -> root partition + btrfs fs fill the resized disk
set -u
mkdir -p /workspace /mnt/nocloud

# per-item "done" flags (1 = converged or not-applicable)
vfs_done=0; vfs_fails=0
extra_done=0
iso_done=0
net_done=0
ssh_done=0
grow_done=0

for i in $(seq 1 60); do
  # 1. virtiofs. Give up after 15 consecutive failures (assume no device —
  #    a --no-mount VM); virtiofsd starts before CH, so a real share is
  #    ready well within that window.
  if [ "$vfs_done" -eq 0 ]; then
    if grep -q " /workspace " /proc/mounts 2>/dev/null; then
      vfs_done=1
    elif mount -t virtiofs workspace /workspace 2>/dev/null; then
      vfs_done=1
    else
      vfs_fails=$((vfs_fails + 1))
      [ "$vfs_fails" -ge 15 ] && vfs_done=1
    fi
  fi

  # 2. extra disk (scratch). Try mounting first (a formatted disk mounts
  #    without re-formatting); only mkfs when the mount fails AND no
  #    .fsdone marker exists.
  if [ "$extra_done" -eq 0 ]; then
    if [ ! -b /dev/vdc ]; then
      extra_done=1
    elif grep -q " /mnt/extra " /proc/mounts 2>/dev/null; then
      extra_done=1
    else
      mkdir -p /mnt/extra
      if mount /dev/vdc /mnt/extra 2>/dev/null; then
        extra_done=1
      elif [ ! -f /mnt/extra/.fsdone ]; then
        mkfs.xfs /dev/vdc 2>&1 | tail -1
        if mount /dev/vdc /mnt/extra 2>/dev/null; then
          touch /mnt/extra/.fsdone
          extra_done=1
        fi
      fi
    fi
  fi

  # 3. NoCloud ISO. One short attempt per iteration (the loop retries — the
  #    first virtio-blk read can be slow). vdb is the known seed position;
  #    fall back to a size-filtered scan (the ISO is small; skip >100 MB).
  if [ "$iso_done" -eq 0 ]; then
    if [ -f /mnt/nocloud/meta-data ]; then
      iso_done=1
    elif timeout 3 mount -o ro /dev/vdb /mnt/nocloud 2>/dev/null && [ -f /mnt/nocloud/meta-data ]; then
      iso_done=1
    else
      umount /mnt/nocloud 2>/dev/null
      for d in /sys/block/*; do
        n=$(basename "$d")
        case "$n" in loop*|ram*|vdb) continue;; esac
        [ -b "/dev/$n" ] || continue
        sz=$(cat "/sys/block/$n/size" 2>/dev/null)
        [ -n "$sz" ] && [ "$sz" -gt 200000 ] && continue
        if timeout 3 blkid "/dev/$n" 2>/dev/null | grep -q iso9660; then
          if timeout 5 mount -o ro "/dev/$n" /mnt/nocloud 2>/dev/null && [ -f /mnt/nocloud/meta-data ]; then
            iso_done=1
            break
          fi
        fi
      done
    fi
  fi

  # 4. network (only once the ISO is readable). One NIC check per iteration
  #    (no 30 s wait — the loop retries); the MAC-based lookup also survives
  #    an interface rename (the MAC is unchanged).
  if [ "$iso_done" -eq 1 ] && [ "$net_done" -eq 0 ]; then
    IP=$(grep "^vm-ip:" /mnt/nocloud/meta-data | sed "s/^vm-ip: //")
    HOST=$(grep "^vm-host:" /mnt/nocloud/meta-data | sed "s/^vm-host: //")
    MAC=$(grep "^vm-mac:" /mnt/nocloud/meta-data | sed "s/^vm-mac: //")
    if [ -n "$IP" ]; then
      IF=$(for x in /sys/class/net/*; do
        n=$(basename "$x")
        case "$n" in lo|docker*|br-*|tap-*) ;; *)
          grep -qi "$MAC" "$x/address" 2>/dev/null && echo "$n";;
        esac
      done | head -1)
      [ -n "$IF" ] || IF=$(ls /sys/class/net 2>/dev/null | grep -vE "^lo$|^docker|^br-|^tap-" | head -1)
      if [ -n "$IF" ]; then
        ip addr replace "$IP/24" dev "$IF" 2>/dev/null
        ip link set "$IF" up 2>/dev/null
        ip route replace default via "$HOST" 2>/dev/null
        ip -4 addr show dev "$IF" 2>/dev/null | grep -qF "$IP" && net_done=1
      fi
    fi
  fi

  # 5. ssh (only once the ISO is readable). Install the key (sshd re-reads
  #    authorized_keys per connection, so no restart is needed) and make sure
  #    sshd is running — without waiting for a slow first-boot start.
  if [ "$iso_done" -eq 1 ] && [ "$ssh_done" -eq 0 ]; then
    if [ -f /mnt/nocloud/ssh-pubkey ]; then
      grep -q "^PermitRootLogin yes" /etc/ssh/sshd_config 2>/dev/null || echo "PermitRootLogin yes" >> /etc/ssh/sshd_config
      mkdir -p /root/.ssh
      grep -qF "$(cat /mnt/nocloud/ssh-pubkey)" /root/.ssh/authorized_keys 2>/dev/null || cat /mnt/nocloud/ssh-pubkey >> /root/.ssh/authorized_keys
      chmod 700 /root/.ssh
      chmod 600 /root/.ssh/authorized_keys
    fi
    if systemctl is-active --quiet sshd 2>/dev/null; then
      ssh_done=1
    else
      systemctl start --no-block sshd 2>/dev/null || true
    fi
  fi

  # 6. disk grow: the host resized the qcow2 to the requested size at
  #    create; the root partition + btrfs fs must follow (growpart +
  #    btrfs resize are idempotent — no-op when already at max). The
  #    100 MB threshold skips the aligned no-op case. The mkdir lock
  #    avoids racing the per-boot runcmd, which runs the same check.
  #    Only mark done once DS+PS are BOTH valid (the partition node
  #    can be briefly absent right after boot — retry until it is up).
  if [ "$grow_done" -eq 0 ]; then
    D=vda
    [ -b /dev/$D ] || D=$(ls /sys/block 2>/dev/null | grep -vE "^(loop|ram|zram)" | head -1)
    DS=$(blockdev --getsize64 /dev/$D 2>/dev/null)
    PS=$(blockdev --getsize64 /dev/${D}3 2>/dev/null)
    if [ -n "$DS" ] && [ -n "$PS" ]; then
      if [ $((DS - PS)) -gt 104857600 ]; then
        mkdir /var/lock/pi-vm-grow 2>/dev/null && {
          growpart /dev/$D 3 2>&1 | tail -1
          if grep -q " / btrfs " /proc/mounts 2>/dev/null; then
            btrfs filesystem resize max / 2>&1 | tail -1
          fi
          rmdir /var/lock/pi-vm-grow
        } || true
      fi
      grow_done=1
    fi
  fi

  # converged? exit early (a healthy VM breaks at ~iteration 1-2).
  if [ "$vfs_done" -eq 1 ] && [ "$extra_done" -eq 1 ] && [ "$iso_done" -eq 1 ] && [ "$net_done" -eq 1 ] && [ "$ssh_done" -eq 1 ] && [ "$grow_done" -eq 1 ]; then
    break
  fi
  sleep 1
done

exit 0
"#;

const NET_SERVICE: &str = "[Unit]\nDescription=pi-vm network and ssh setup from NoCloud ISO\nConditionPathExists=/usr/local/bin/pi-vm-net.sh\n\n[Service]\nType=oneshot\nRemainAfterExit=yes\nTimeoutStartSec=180\nExecStartPre=-/usr/bin/restorecon /usr/local/bin/pi-vm-net.sh\nExecStartPre=/usr/bin/chcon -t bin_t /usr/local/bin/pi-vm-net.sh\nExecStart=/usr/local/bin/pi-vm-net.sh\n";

// Repeating timer + ConditionPathExists (on the service): robust across hosts
// with NO fixed time guess. The timer ticks every 1 s; the service only runs
// once /usr/local/bin/pi-vm-net.sh exists (i.e. the root fs is mounted —
// whenever that is, on any host). RemainAfterExit=yes means that once the
// convergence loop finishes, the service stays active (exited) and the later
// ticks are no-ops. It never blocks multi-user (the timer is passive).
const NET_START_TIMER: &str = "[Unit]\nDescription=Start pi-vm network setup (repeating until the root fs is up)\n\n[Timer]\nOnBootSec=1s\nOnUnitActiveSec=1s\nUnit=pi-vm-net.service\n\n[Install]\nWantedBy=timers.target\n";

/// Write the net-fallback script + units into the baked image (one
/// patient guestfish RW session; the -i inspection mounts the btrfs
/// root subvolume at /sysroot).
fn write_net_fallback(part: &std::path::Path) {
    println!("   installing the cloud-init-independent net fallback");
    let tmp = std::env::temp_dir().join(format!("pi-vm-netfb-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let entries: [(&str, &str); 3] = [
        ("pi-vm-net.sh", NET_SCRIPT),
        ("pi-vm-net.service", NET_SERVICE),
        ("pi-vm-net-start.timer", NET_START_TIMER),
    ];
    for (name, content) in &entries {
        if std::fs::write(tmp.join(name), content).is_err() {
            eprintln!("   (net fallback: temp file write failed — skipped)");
            return;
        }
    }
    let mut cmds: Vec<String> = vec![];
    for (name, _) in &entries {
        cmds.push(format!("upload {} {}", tmp.join(name).display(),
            if name.ends_with(".sh") { "/usr/local/bin/pi-vm-net.sh".to_string() } else { format!("/etc/systemd/system/{}", name) }));
    }
    cmds.push("mkdir-p /etc/systemd/system/multi-user.target.wants".into());
    cmds.push("mkdir-p /etc/systemd/system/timers.target.wants".into());
    // Bake /workspace in so the attach's `cd /workspace` always works from
    // boot — before virtiofs has mounted it's an empty dir, and the files
    // appear in the cwd when the mount lands (the unit mounts over it).
    cmds.push("mkdir-p /workspace".into());
    // guestfish `ln` makes a HARD link (systemd needs a symlink); the
    // appliance has sh, so do the symlinks + chmod in one sh -c.
    cmds.push("command \"sh -c 'rm -f /etc/systemd/system/multi-user.target.wants/pi-vm-net.service /etc/systemd/system/pi-vm-net-late.service /etc/systemd/system/pi-vm-net-late.timer /etc/systemd/system/timers.target.wants/pi-vm-net-late.timer; ln -sf /etc/systemd/system/pi-vm-net-start.timer /etc/systemd/system/timers.target.wants/pi-vm-net-start.timer; chmod 755 /usr/local/bin/pi-vm-net.sh'\"".into());
    // Boot optimizations (persist to the baked image):
    //  - mask cloud-init's later stages: only the local stage matters
    //    (runcmd); main regenerates the ssh host keys on EVERY boot
    //    (breaks known_hosts-based reconnects) and costs ~5 s.
    //  - disable daemons not needed at boot (all stay installed, start
    //    on demand): chronyd, auditd, sssd, lvm2-monitor,
    //    qemu-guest-agent (we're on CH). docker/containerd stay
    //    enabled (autostart — the runcmd's `systemctl enable --now
    //    docker` persists; docker.service's Requires= pulls containerd
    //    in even if its wants symlink were missing). The redundant
    //    docker.socket wants symlink is still removed (the service
    //    itself starts at boot, so socket activation adds nothing).
    cmds.push("command \"sh -c 'for u in cloud-init-main cloud-config cloud-final; do ln -sf /dev/null /etc/systemd/system/$u.service; done; for w in /etc/systemd/system/sockets.target.wants/docker.socket /etc/systemd/system/multi-user.target.wants/chronyd.service /etc/systemd/system/multi-user.target.wants/auditd.service /etc/systemd/system/multi-user.target.wants/audit-rules.service /etc/systemd/system/multi-user.target.wants/sssd.service /etc/systemd/system/multi-user.target.wants/lvm2-monitor.service /etc/systemd/system/multi-user.target.wants/qemu-guest-agent.service; do rm -f $w; done'\"".into());
    let ok = guestfish_rw(part, &cmds);
    let _ = std::fs::remove_dir_all(&tmp);
    if ok {
        println!("   net fallback installed (pi-vm-net.service + start timer; convergence loop)");
    } else {
        eprintln!("   (net fallback install failed — the runcmd remains the only path)");
    }
}

/// Run guestfish RW commands via stdin (LIBGUESTFS_BACKEND=direct, -i
/// inspection; no --ro — this writes).
fn guestfish_rw(image: &std::path::Path, cmds: &[String]) -> bool {
    let mut child = match Command::new("guestfish")
        .env("LIBGUESTFS_BACKEND", "direct")
        .args(["-a", image.to_str().unwrap(), "-i"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    {
        let stdin = child.stdin.as_mut().expect("piped stdin");
        for c in cmds {
            let _ = stdin.write_all(c.as_bytes());
            let _ = stdin.write_all(b"\n");
        }
    }
    let status = child.wait().ok();
    status.map(|s| s.success()).unwrap_or(false)
}

/// Verification: ONE patient guestfish session (the shell in `command` is
/// chrooted into the guest root): up to ~3 min, no repeated disk reads.
fn verify_bake(part: &std::path::Path) -> bool {
    let check = "command \"sh -c 'i=0; while [ $i -lt 12 ]; do if [ -f /root/.pi-vm-setup-done ] && [ -d /root/.pi/agent ] && command -v docker >/dev/null && command -v git >/dev/null && command -v node >/dev/null && command -v rustc >/dev/null && command -v uv >/dev/null && command -v which >/dev/null && command -v cargo >/dev/null && command -v fd >/dev/null && command -v rg >/dev/null; then exit 0; fi; i=$((i+1)); sleep 15; done; exit 1'\"";
    guestfish_run(part, &[check])
}

/// Dump the on-disk runcmd logs (the serial console only captures the getty).
fn dump_bake_logs(part: &std::path::Path) {
    println!("   dumping on-disk bake logs from {}", part.display());
    let cmds = &[
        "download /sysroot/root/bake-debug.log /tmp/bake-debug-dump.log",
        "download /sysroot/root/bake-net.log /tmp/bake-net-dump.log",
    ];
    let _ = guestfish_run(part, cmds);
    if let Ok(data) = std::fs::read_to_string("/tmp/bake-debug-dump.log") {
        for line in data.lines().rev().take(40) {
            println!("   | [debug] {line}");
        }
    } else {
        println!("   | [debug] (no /root/bake-debug.log on disk — the toolchain runcmd likely never ran)");
    }
    if let Ok(data) = std::fs::read_to_string("/tmp/bake-net-dump.log") {
        for line in data.lines().rev().take(20) {
            println!("   | [net] {line}");
        }
    } else {
        println!("   | [net] (no /root/bake-net.log on disk — the network runcmd likely never ran)");
    }
    let _ = std::fs::remove_file("/tmp/bake-debug-dump.log");
    let _ = std::fs::remove_file("/tmp/bake-net-dump.log");
}

/// Extract the direct-boot assets (kernel + initramfs + kernel cmdline)
/// from the image into assets/ — `create`/`resume` boot without UEFI +
/// GRUB (CH loads the kernel straight). Used twice: on the RAW cloud
/// image before the prep boot (REQUIRED — the prep boot is direct-booted;
/// a failure aborts the bake), and on the baked image afterwards (best
/// effort: a failure leaves the raw-image assets in place — still valid,
/// the prep boot doesn't change the kernel).
///
/// Sources, in order of preference:
/// 1. the BLS entry (Fedora 44: /boot/loader/entries/*.conf) — one
///    consistent source for the kernel, initramfs, and the `options`
///    line (the kernel cmdline)
/// 2. `ls /boot` (kernel + initramfs names) + the grub.cfg fallback
///    `kernelopts` variable (cmdline)
/// console=ttyS0 is guaranteed (CH's console is the serial port).
/// Note: guestfish `command` does NOT shell-interpret — no pipes/redirects.
fn extract_boot_assets(part: &std::path::Path, home: &VmHome) {
    println!("   extracting kernel + initramfs for direct boot ...");
    let _ = std::fs::create_dir_all(home.assets_dir());

    // 1. BLS entry
    let mut kernel: Option<String> = None;
    let mut initramfs: Option<String> = None;
    let mut cmdline: Option<String> = None;
    if let Some(entries) = guestfish_out(part, &["ls /boot/loader/entries"])
        && let Some(conf) = entries.lines().find(|l| l.trim_end().ends_with(".conf"))
        && let Some(txt) = guestfish_out(part, &[&format!("cat /boot/loader/entries/{}", conf.trim())])
    {
        for line in txt.lines() {
            let line = line.trim();
            if let Some(p) = line.strip_prefix("linux ") {
                kernel = Some(p.trim().trim_start_matches("/boot/").to_string());
            } else if let Some(p) = line.strip_prefix("initrd ") {
                initramfs = Some(p.trim().trim_start_matches("/boot/").to_string());
            } else if let Some(p) = line.strip_prefix("options ") {
                let mut s = p.trim().to_string();
                if !s.contains("console=hvc0") {
                    s = with_hvc0_console(&s);
                }
                cmdline = Some(s);
            }
        }
    }
    // 2. fallbacks for the missing pieces
    if kernel.is_none() || initramfs.is_none() {
        if let Some(boot_ls) = guestfish_out(part, &["ls /boot"]) {
            if kernel.is_none() {
                kernel = boot_ls.lines().find(|l| l.trim_start().starts_with("vmlinuz-")).map(|l| l.trim().to_string());
            }
            if initramfs.is_none() {
                initramfs = boot_ls.lines().find(|l| l.trim_start().starts_with("initramfs-") && l.trim_end().ends_with(".img")).map(|l| l.trim().to_string());
            }
        }
    }
    if cmdline.is_none() {
        if let Some(cfg) = guestfish_out(part, &["cat /boot/grub2/grub.cfg"]) {
            for line in cfg.lines() {
                if let Some(k) = line.trim().strip_prefix("set kernelopts=\"") {
                    let o = k.trim_end().trim_end_matches('"');
                    if o.contains("root=") {
                        let mut s = o.to_string();
                        if !s.contains("console=hvc0") {
                            s = with_hvc0_console(&s);
                        }
                        cmdline = Some(s);
                        break;
                    }
                }
            }
        }
    }
    let (Some(kernel), Some(initramfs), Some(cmdline)) = (kernel, initramfs, cmdline) else {
        println!("   (extraction failed)");
        return;
    };
    if !guestfish_run(part, &[&format!("download /boot/{kernel} {}", home.kernel().display()),
                            &format!("download /boot/{initramfs} {}", home.initramfs().display())])
        || !home.kernel().exists() || !home.initramfs().exists()
    {
        println!("   (download failed)");
        return;
    }
    if std::fs::write(home.kernel_cmdline(), &cmdline).is_err() {
        println!("   (cmdline write failed)");
        return;
    }
    println!("   direct-boot assets ready: {kernel} + {initramfs}");
    println!("   kernel cmdline: {cmdline}");
}

/// Normalize the console params for CH: CH's `--console` device is a
/// virtio-console, which appears in the guest as hvc0 — NOT a 16550
/// serial (ttyS0). Kernel output to ttyS0 is lost (verified: writes to
/// /dev/ttyS0 never reach the console file; only the hvc0 getty does).
/// Make hvc0 the console (the last console= is the primary) and drop the
/// useless ttyS0 params. console=hvc0 is guaranteed.
fn with_hvc0_console(s: &str) -> String {
    let parts: Vec<&str> = s.split_whitespace()
        .filter(|p| !p.starts_with("console=ttyS0"))
        .collect();
    let has_hvc0 = parts.iter().any(|p| p.starts_with("console=hvc0"));
    let mut out = parts.join(" ");
    if !has_hvc0 {
        out.push_str(" console=hvc0");
    }
    out
}

/// Run guestfish commands via stdin, capturing stdout (LIBGUESTFS_BACKEND=direct).
/// (The interactive prompt lines are noise — callers parse tolerantly.)
fn guestfish_out(image: &std::path::Path, cmds: &[&str]) -> Option<String> {
    let mut child = Command::new("guestfish")
        .env("LIBGUESTFS_BACKEND", "direct")
        .args(["-a", image.to_str()?, "-i", "--ro"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    for c in cmds {
        stdin.write_all(c.as_bytes()).ok()?;
        stdin.write_all(b"\n").ok()?;
    }
    drop(stdin); // EOF -> guestfish runs the commands and exits
    use std::io::Read;
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    child.wait().ok()?.success().then_some(out)
}

/// Run guestfish with a list of commands via stdin (LIBGUESTFS_BACKEND=direct).
fn guestfish_run(image: &std::path::Path, cmds: &[&str]) -> bool {
    let mut child = match Command::new("guestfish")
        .env("LIBGUESTFS_BACKEND", "direct")
        .args(["-a", image.to_str().unwrap(), "-i", "--ro"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    {
        let stdin = child.stdin.as_mut().expect("piped stdin");
        for c in cmds {
            let _ = stdin.write_all(c.as_bytes());
            let _ = stdin.write_all(b"\n");
        }
    }
    let status = child.wait().ok();
    status.map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::user_data;

    /// The generated user-data must stay valid YAML: each runcmd entry is
    /// a multi-line plain scalar (one `bash -c '...'` per entry) with no
    /// `: ` sequences, no ` #` sequences (a `#` at line start or after a
    /// space terminates the scalar — comments are NOT allowed in the
    /// block), and exactly one opening + one closing single quote.
    #[test]
    fn user_data_runcmd_blocks_are_valid_yaml() {
        let s = user_data();
        let entries: Vec<&str> = s
            .split("  - ")
            .map(|e| e.trim_start())
            .filter(|e| e.starts_with("bash -c '"))
            .collect();
        assert_eq!(entries.len(), 3, "expected 3 runcmd entries");
        for e in &entries {
            assert!(!e.contains(": "), "runcmd entry contains ': ' (breaks the YAML plain scalar): {}", e.chars().take(80).collect::<String>());
            assert!(!e.contains(" #"), "runcmd entry contains ' #' (starts a comment, breaks the YAML plain scalar): {}", e.chars().take(80).collect::<String>());
            assert_eq!(e.matches('\'').count(), 2, "unbalanced single quotes: {}", e.chars().take(80).collect::<String>());
        }
    }
}


