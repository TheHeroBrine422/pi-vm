//! `pi-vm version` — CLI + asset versions and image hashes.

use crate::state::VmHome;
use crate::util::sha256_file;

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
        match sha256_file(&p) {
            Some(h) => println!("{label} sha256 {h}"),
            None => {
                if p.exists() {
                    println!("{label} sha256 (unreadable)");
                } else {
                    println!("{label} (missing)");
                }
            }
        }
    }
    0
}
