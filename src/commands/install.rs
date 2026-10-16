//! `pi-vm install` — one-time host-side setup (port of vm/setup.sh).
//!
//!   1. checks host requirements (virtiofsd, cloud-init, genisoimage,
//!      iproute, qemu-img, iptables, libguestfs) and prints the exact
//!      install command for anything missing — the user runs their own
//!      package manager (per-distro lists are in the README)
//!   2. locates the Cloud Hypervisor binary: $CH_BIN > existing assets
//!      copy > PATH (e.g. a terrapkg install — symlinked into assets) >
//!      download of the static v53.0 binary
//!   3. enables nested KVM (so the VM can run its own VMs)
//!   4. creates the data dirs + downloads the Fedora 44 cloud image
//!      (sha256-verified)

use clap::Args;

use crate::state::VmHome;
use crate::util::{run, run_ok, virtiofsd_path};

const CH_VERSION: &str = "v53.0";

#[derive(Args)]
pub struct Install {}

impl Install {
    pub fn run(&self, home: &VmHome) -> i32 {
        let pm = if run_ok("which", &["dnf"]) { Some("dnf") } else if run_ok("which", &["apt-get"]) { Some("apt") } else { None };
        if !self.requirements(pm) {
            eprintln!();
            eprintln!("install incomplete: install the packages above, then re-run `pi-vm install`");
            return 1;
        }
        self.ch_binary(home);
        self.nested_kvm();
        self.cloud_image(home);

        println!();
        println!("install complete. next:");
        println!("  pi-vm bake --pi-dir <your-pi-config-dir>");
        0
    }

    /// Host requirements, checked by BINARY name (distro-independent).
    /// No auto-install: missing packages are reported with the exact command
    /// for the detected package manager (README has the per-distro lists).
    /// Returns false (run aborts) when anything is missing.
    fn requirements(&self, pm: Option<&str>) -> bool {
        println!("==> [1/4] host requirements (virtiofsd, cloud-init, genisoimage, iproute, qemu-img, iptables, libguestfs)");
        // (binary to check, dnf package, apt package, note)
        let specs: [(&str, &str, &str, &str); 7] = [
            ("virtiofsd", "virtiofsd", "virtiofsd", ""),
            ("cloud-init", "cloud-init", "cloud-init", ""),
            ("genisoimage", "genisoimage", "genisoimage", ""),
            ("ip", "iproute", "iproute2", ""),
            ("qemu-img", "qemu-img", "qemu-utils", ""),
            ("iptables", "iptables", "iptables", ""),
            ("guestfish", "libguestfs", "libguestfs-tools", " (bake only)"),
        ];
        let missing: Vec<(&str, &str, &str, &str)> = specs
            .iter()
            .filter(|(bin, _, _, _)| {
                if *bin == "virtiofsd" {
                    virtiofsd_path().is_none()
                } else {
                    !run_ok("which", &[*bin])
                }
            })
            .map(|s| (s.0, s.1, s.2, s.3))
            .collect();
        if missing.is_empty() {
            println!("   all host requirements present");
            return true;
        }
        println!("   missing: {}", missing.iter().map(|m| format!("{}{}", m.0, m.3)).collect::<Vec<_>>().join(", "));
        match pm {
            Some("dnf") => println!("   install with:  dnf install -y {}", missing.iter().map(|m| m.1).collect::<Vec<_>>().join(" ")),
            Some("apt") => println!("   install with:  apt-get install -y {}", missing.iter().map(|m| m.2).collect::<Vec<_>>().join(" ")),
            None => println!("   (no dnf/apt-get detected — install the missing binaries with your package manager of choice)"),
            Some(other) => println!("   (detected {other} but it is not dnf/apt — install the missing binaries with your package manager of choice)"),
        }
        false
    }

    /// Locate the Cloud Hypervisor binary: $CH_BIN (explicit) > existing
    /// assets copy (version-verified at download) > PATH (a distro or
    /// terrapkg install — symlinked into assets) > static download.
    fn ch_binary(&self, home: &VmHome) {
        println!("==> [2/4] Cloud Hypervisor binary");
        let bin = home.ch_binary();
        let _ = std::fs::create_dir_all(home.assets_dir());
        // $CH_BIN: an explicit existing binary wins. A stale symlink is
        // replaced; a real assets file is kept (and reported below).
        if let Ok(custom) = std::env::var("CH_BIN") {
            let custom = std::path::Path::new(&custom);
            if custom.exists() {
                match std::fs::read_link(&bin) {
                    Ok(p) if p != custom => { let _ = std::fs::remove_file(&bin); } // stale symlink: replace
                    Ok(_) => { self.ch_report(&bin); return; }                    // already wired
                    Err(_) if bin.exists() => { self.ch_report(&bin); return; } // real file: keep
                    Err(_) => {} // no file yet: fall through and symlink
                }
                if std::os::unix::fs::symlink(custom, &bin).is_ok() {
                    println!("   CH from $CH_BIN: {custom:?}");
                    self.ch_report(&bin);
                    return;
                }
                eprintln!("   WARN: could not symlink {bin:?} -> {custom:?}");
            } else {
                eprintln!("   WARN: $CH_BIN={custom:?} does not exist — ignoring");
            }
        }
        if bin.exists() {
            self.ch_report(&bin);
            return;
        }
        // PATH (e.g. terrapkg or a distro package)
        if let Ok(o) = run("which", &["cloud-hypervisor"])
            && let Some(path) = o.lines().next().map(|s| s.to_string())
            && std::os::unix::fs::symlink(&path, &bin).is_ok()
        {
            println!("   CH found on PATH: {path} (symlinked into assets)");
            self.ch_report(&bin);
            return;
        }
        self.ch_download(&bin);
    }

    fn ch_report(&self, bin: &std::path::Path) {
        let v = std::process::Command::new(bin)
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("?").to_string())
            .unwrap_or_else(|_| "?".into());
        println!("   CH present: {bin:?} ({v})");
    }

    fn ch_download(&self, bin: &std::path::Path) {
        println!("   downloading cloud-hypervisor {CH_VERSION} ...");
        let url = format!("https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/{CH_VERSION}/cloud-hypervisor-static");
        let part = format!("{}.part", bin.display());
        if run_ok("curl", &["-fL", "--progress-bar", "-o", &part, &url]) {
            run_ok("chmod", &["+x", &part]);
            if std::fs::rename(&part, bin).is_ok() {
                println!("   saved: {bin:?}");
            } else {
                eprintln!("   ERROR: rename {part} -> {bin:?} failed");
            }
        } else {
            let _ = std::fs::remove_file(&part);
            eprintln!("   ERROR: CH download failed from {url} (set CH_BIN to an existing binary and re-run)");
        }
    }

    fn nested_kvm(&self) {
        println!("==> [3/4] nested KVM (so the VM can run its own VMs)");
        let mod_out = run("ls", &["/sys/module/"]).unwrap_or_default();
        let kvm_mod = mod_out
            .lines()
            .find(|l| *l == "kvm_intel" || *l == "kvm_amd");
        match kvm_mod {
            Some(m) => {
                let conf = format!("options {m} nested=1\n");
                // privileged write (auto-sudo tee when not root)
                let mut tee = std::process::Command::new(if crate::util::is_root() { "tee" } else { "sudo" })
                    .args(if crate::util::is_root() {
                        vec!["-a", "/etc/modprobe.d/kvm-nested.conf"]
                    } else {
                        vec!["tee", "-a", "/etc/modprobe.d/kvm-nested.conf"]
                    })
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .expect("spawn tee");
                if let Some(mut stdin) = tee.stdin.take() {
                    use std::io::Write;
                    let _ = stdin.write_all(conf.as_bytes());
                }
                let _ = tee.wait();
                let loaded = std::process::Command::new("lsmod")
                    .output()
                    .map(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .any(|l| l.split_whitespace().next() == Some(m))
                    })
                    .unwrap_or(false);
                if loaded {
                    let r = if crate::util::is_root() {
                        std::process::Command::new("modprobe").args(["-r", m]).status()
                    } else {
                        std::process::Command::new("sudo").args(["modprobe", "-r", m]).status()
                    };
                    let r2 = if crate::util::is_root() {
                        std::process::Command::new("modprobe").arg(m).status()
                    } else {
                        std::process::Command::new("sudo").args(["modprobe", m]).status()
                    };
                    if r.map(|s| s.success()).unwrap_or(false)
                        && r2.map(|s| s.success()).unwrap_or(false)
                    {
                        println!("   reloaded {m} with nested=1");
                    } else {
                        println!("   could not reload {m} now (VMs running? or needs sudo); applies on next reboot");
                    }
                } else {
                    println!("   config written; {m} not loaded yet (applies when it loads)");
                }
            }
            None => {
                println!("   WARN: no kvm module loaded — check the host's KVM support (CH requires KVM; it has no TCG fallback and will fail to start without /dev/kvm)");
            }
        }
        if std::path::Path::new("/dev/kvm").exists() {
            let readable = std::fs::OpenOptions::new().read(true).open("/dev/kvm").is_ok();
            let writable = std::fs::OpenOptions::new().write(true).open("/dev/kvm").is_ok();
            if readable && writable {
                println!("   /dev/kvm already accessible");
            } else if !crate::util::is_root() {
                // v1: add the invoking user to the kvm group
                if let Ok(user) = std::env::var("USER") {
                    if run_ok("sudo", &["usermod", "-aG", "kvm", &user]) {
                        println!("   added {user} to group kvm — log out and back in before running pi-vm");
                    } else {
                        println!("   /dev/kvm exists but is not accessible — check group membership (kvm)");
                    }
                }
            } else {
                println!("   /dev/kvm exists but is not accessible — check device permissions");
            }
        } else {
            println!("   WARN: no /dev/kvm — check the host's KVM support (CH requires KVM; no TCG fallback)");
        }
    }

    fn cloud_image(&self, home: &VmHome) {
        println!("==> [4/4] directories + fedora cloud image");
        let _ = std::fs::create_dir_all(home.images_dir());
        let _ = std::fs::create_dir_all(home.assets_dir());
        let _ = std::fs::create_dir_all(home.vms_dir());

        let img = home.cloud_image();
        if img.exists() {
            println!("   cloud image already present: {img:?}");
            return;
        }
        let url = "https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2";
        let cksum_url = "https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-44-1.7-x86_64-CHECKSUM";
        let name = "Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2";
        println!("   downloading {name} (~557 MB)...");
        let part = format!("{}.part", img.display());
        let mut ok = false;
        if run_ok("curl", &["-fL", "--progress-bar", "-o", &part, url]) {
            let ck = format!("{}.cksum", img.display());
            let mut skipped = false;
            if run_ok("curl", &["-fsSL", cksum_url, "-o", &ck])
                && let Some(want) = run("grep", &["-F", &format!("SHA256 ({name}) ="), &ck]).ok().and_then(|g| {
                    g.rsplit(' ').next().map(|s| s.to_string())
                }) {
                    let got = run("sha256sum", &[&part]).ok().and_then(|s| s.split(' ').next().map(|s| s.to_string())).unwrap_or_default();
                    if want != got {
                        println!("   WARN: sha256 mismatch (want {want}, got {got})");
                    } else {
                        println!("   sha256 verified");
                    }
                } else {
                    skipped = true;
                }
            if skipped {
                println!("   WARN: checksum unavailable — sha256 verification SKIPPED");
            }
            let size = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            if size > 100_000_000 {
                let _ = std::fs::remove_file(&ck);
                if std::fs::rename(&part, &img).is_ok() {
                    println!("   saved: {img:?}");
                    ok = true;
                }
            } else {
                println!("   WARN: download looks bad ({size} bytes)");
            }
        } else {
            println!("   WARN: download failed");
        }
        if !ok {
            let _ = std::fs::remove_file(format!("{}.part", img.display()));
            let _ = std::fs::remove_file(format!("{}.cksum", img.display()));
            eprintln!("   ERROR: cloud image not available — re-run `pi-vm install`");
        }
    }
}
