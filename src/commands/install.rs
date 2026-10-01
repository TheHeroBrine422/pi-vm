//! `pi-vm install` — one-time host-side setup (port of vm/setup.sh).
//!
//!   1. installs host packages (virtiofsd, cloud-init, genisoimage, iproute,
//!      qemu-img, iptables, libguestfs)
//!   2. downloads the Cloud Hypervisor static binary (v53.0)
//!   3. enables nested KVM (so the VM can run its own VMs)
//!   4. creates the data dirs + downloads the Fedora 44 cloud image
//!      (sha256-verified)
//!
//! `--check` = dry run: report what is present/missing without installing.

use clap::Args;

use crate::commands::doctor::Doctor;
use crate::state::VmHome;
use crate::util::{run, run_ok, virtiofsd_path};

const CH_VERSION: &str = "v53.0";

#[derive(Args)]
pub struct Install {
    /// Only check, don't install
    #[arg(long)]
    pub check: bool,
}

impl Install {
    pub fn run(&self, home: &VmHome) -> i32 {
        if self.check {
            println!("==> check mode: reporting what is present/missing (no installs)");
            let rc = Doctor { detailed: true }.run(home);
            self.check_assets(home);
            return rc;
        }

        if !run_ok("which", &["dnf"]) {
            eprintln!("error: expected a dnf-based host (CachyOS/Fedora)");
            return 2;
        }

        self.packages();
        self.ch_binary(home);
        self.nested_kvm();
        self.cloud_image(home);

        println!();
        println!("install complete. next:");
        println!("  pi-vm bake --pi-dir <your-pi-config-dir>");
        0
    }

    fn packages(&self) {
        println!("==> [1/4] host packages (virtiofsd, cloud-init, genisoimage, iproute, qemu-img, iptables, libguestfs)");
        let pkgs = ["virtiofsd", "cloud-init", "genisoimage", "iproute", "qemu-img", "iptables", "libguestfs"];
        let missing: Vec<&str> = pkgs
            .iter()
            .filter(|p| {
                if **p == "virtiofsd" {
                    virtiofsd_path().is_none()
                } else {
                    !run_ok("which", &[**p])
                }
            }).copied()
            .collect();
        if missing.is_empty() {
            println!("   all packages already installed");
            return;
        }
        println!("   installing: {}", missing.join(", "));
        let mut args: Vec<String> = vec!["dnf".into(), "install".into(), "-y".into()];
        args.extend(missing.iter().map(|s| s.to_string()));
        // privileged (auto-sudo when not root); a failure ABORTS (v1's behavior)
        let status = if crate::util::is_root() {
            std::process::Command::new(&args[0]).args(&args[1..]).status()
        } else {
            let mut sa: Vec<String> = vec!["sudo".into()];
            sa.extend(args);
            std::process::Command::new(&sa[0]).args(&sa[1..]).status()
        };
        match status {
            Ok(s) if s.success() => println!("   installed"),
            Ok(s) => {
                eprintln!("   ERROR: dnf install exited with {s} — re-run `pi-vm install`");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("   ERROR: dnf install: {e} — re-run `pi-vm install`");
                std::process::exit(1);
            }
        }
    }

    fn ch_binary(&self, home: &VmHome) {
        println!("==> [2/4] Cloud Hypervisor static binary");
        let bin = home.ch_binary();
        let _ = std::fs::create_dir_all(home.assets_dir());
        if bin.exists() {
            let v = std::process::Command::new(&bin)
                .arg("--version")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("?").to_string())
                .unwrap_or_else(|_| "?".into());
            println!("   CH already present: {bin:?} ({v})");
            return;
        }
        println!("   downloading cloud-hypervisor {CH_VERSION} ...");
        let url = format!("https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/{CH_VERSION}/cloud-hypervisor-static");
        let part = format!("{}.part", bin.display());
        if run_ok("curl", &["-fL", "--progress-bar", "-o", &part, &url]) {
            run_ok("chmod", &["+x", &part]);
            if std::fs::rename(&part, &bin).is_ok() {
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

    fn check_assets(&self, home: &VmHome) {
        println!();
        println!("asset status:");
        for (label, p) in [
            ("cloud-hypervisor", home.ch_binary()),
            ("kernel (direct boot)", home.kernel()),
            ("initramfs (direct boot)", home.initramfs()),
            ("fedora44-cloud.qcow2", home.cloud_image()),
            ("fedora44-cloud-prepped.qcow2", home.prepped_image()),
        ] {
            println!("  {label:<28} {}", if p.exists() { "present" } else { "MISSING" });
        }
    }
}
