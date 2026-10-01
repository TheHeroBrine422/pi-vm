//! `pi-vm doctor` — "why won't it start" in one command.

use std::path::Path;
use std::process::Command;

use clap::Args;

use crate::state::VmHome;
use crate::util::virtiofsd_path;

#[derive(Args)]
pub struct Doctor {
    /// Include asset hashes and CH version
    #[arg(long)]
    pub detailed: bool,
}

pub struct Check<'a> {
    pub name: &'a str,
    pub ok: bool,
    pub detail: String,
}

impl Doctor {
    pub fn run(&self, home: &VmHome) -> i32 {
        self.print(&self.checks(home, self.detailed))
    }

    fn checks(&self, home: &VmHome, detailed: bool) -> Vec<Check<'static>> {
        let mut c = Vec::new();

        // KVM
        let kvm_ok = Path::new("/dev/kvm").exists();
        c.push(Check {
            name: "KVM (/dev/kvm)",
            ok: kvm_ok,
            detail: if kvm_ok {
                "present".into()
            } else {
                "missing — CH requires KVM (no TCG mode)".into()
            },
        });

        // required host binaries (virtiofsd is checked separately — see virtiofsd_path)
        for (bin, hint) in [
            ("genisoimage", "dnf install genisoimage"),
            ("guestfish", "dnf install libguestfs"),
            ("ssh", "dnf install openssh-clients"),
            ("ip", "dnf install iproute"),
            ("iptables", "dnf install iptables"),
            ("qemu-img", "dnf install qemu-img"),
            ("ssh-keygen", "dnf install openssh"),
        ] {
            let which = Command::new("which")
                .arg(bin)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            c.push(Check {
                name: bin,
                ok: which,
                detail: if which { "installed".into() } else { format!("missing — {hint}") },
            });
        }

        // virtiofsd (may live in /usr/libexec, not PATH)
        let vf_path = virtiofsd_path();
        c.push(Check {
            name: "virtiofsd",
            ok: vf_path.is_some(),
            detail: match vf_path {
                Some(p) => format!("installed at {p}"),
                None => "missing — dnf install virtiofsd".into(),
            },
        });

        // assets
        let ch = home.ch_binary();
        c.push(Check {
            name: "cloud-hypervisor binary",
            ok: ch.exists(),
            detail: if ch.exists() {
                if detailed {
                    let v = Command::new(&ch)
                        .arg("--version")
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or_default();
                    format!("present — {v}")
                } else {
                    "present".into()
                }
            } else {
                format!("missing at {} — run `pi-vm install`", ch.display())
            },
        });

        // direct-boot assets (required — without them no VM can boot)
        let kb = home.kernel().exists() && home.initramfs().exists() && home.kernel_cmdline().exists();
        c.push(Check {
            name: "direct-boot assets (kernel + initramfs)",
            ok: kb,
            detail: if kb {
                "present".into()
            } else {
                "missing — no VM can boot; re-run `pi-vm bake` to extract them".into()
            },
        });

        // images
        let cloud = home.cloud_image();
        c.push(Check {
            name: "fedora44-cloud.qcow2",
            ok: cloud.exists(),
            detail: if cloud.exists() {
                if detailed {
                    format!("present — sha256 {}", sha256_of(&cloud).unwrap_or_else(|| "?".into()))
                } else {
                    "present".into()
                }
            } else {
                format!("missing at {} — run `pi-vm install`", cloud.display())
            },
        });

        let prepped = home.prepped_image();
        c.push(Check {
            name: "fedora44-cloud-prepped.qcow2 (pre-baked base)",
            ok: prepped.exists(),
            detail: if prepped.exists() {
                "present — `create`/`resume` will work".into()
            } else {
                format!("missing at {} — run `pi-vm bake`", prepped.display())
            },
        });

        // data dir writable
        let writable = std::fs::create_dir_all(&home.root)
            .and_then(|()| std::fs::write(home.root.join(".write-test"), b"ok"))
            .and_then(|()| std::fs::remove_file(home.root.join(".write-test")))
            .is_ok();
        c.push(Check {
            name: "data dir writable",
            ok: writable,
            detail: if writable {
                format!("{} is writable", home.root.display())
            } else {
                format!("{} is NOT writable", home.root.display())
            },
        });

        // virtiofsd sanity (version check)
        let vf = match virtiofsd_path() {
            Some(p) => Command::new(p).arg("--version").output().map(|o| o.status.success()).unwrap_or(false),
            None => false,
        };
        c.push(Check {
            name: "virtiofsd --version",
            ok: vf,
            detail: if vf {
                match virtiofsd_path() {
                    Some(p) => Command::new(p)
                        .arg("--version")
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or_default(),
                    None => String::new(),
                }
            } else {
                "not runnable".into()
            },
        });

        c
    }

    fn print(&self, checks: &[Check<'_>]) -> i32 {
        let mut all_ok = true;
        println!("{:<38} {:<6} DETAIL", "CHECK", "OK");
        for c in checks {
            if !c.ok {
                all_ok = false;
            }
            println!("{:<38} {:<6} {}", c.name, if c.ok { "yes" } else { "NO" }, c.detail);
        }
        if all_ok {
            println!("\nall checks passed — the host is ready");
            0
        } else {
            println!("\nsome checks failed — see hints above");
            1
        }
    }
}

fn sha256_of(p: &Path) -> Option<String> {
    let data = std::fs::read(p).ok()?;
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(&data);
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
