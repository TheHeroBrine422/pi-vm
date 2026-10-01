//! `pi-vm version` — CLI + asset versions and image hashes.

use crate::state::VmHome;

pub fn run(home: &VmHome) -> i32 {
    println!("pi-vm {}", env!("CARGO_PKG_VERSION"));
    let ch = home.ch_binary();
    if ch.exists() {
        if let Ok(o) = std::process::Command::new(&ch).arg("--version").output() {
            println!("cloud-hypervisor {}", String::from_utf8_lossy(&o.stdout).trim());
        }
    } else {
        println!("cloud-hypervisor (not installed — run `pi-vm install`)");
    }
    for (label, p) in [
        ("cloud image", home.cloud_image()),
        ("pre-baked image", home.prepped_image()),
    ] {
        if p.exists() {
            let h = std::fs::read(&p)
                .ok()
                .map(|d| {
                    use sha2::{Digest, Sha256};
                    let mut s = Sha256::new();
                    s.update(&d);
                    s.finalize()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                })
                .unwrap_or_default();
            println!("{label} sha256 {h}");
        } else {
            println!("{label} (missing)");
        }
    }
    0
}
