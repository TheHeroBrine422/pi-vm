//! `pi-vm fetch` — copy a file or directory from a running VM to the host
//! (scp -r over the VM's ssh). Used for host-side snapshots of guest-local
//! artifacts (e.g. the agent's pi session dir, which the agent can tamper
//! with in-guest — the host copy is the audit trail).

use clap::Args;

use crate::state::{resolve, VmHome, VmState};

#[derive(Args)]
pub struct Fetch {
    /// VM id, unique id prefix, or name
    pub target: String,
    /// Path inside the guest (file or directory)
    pub guest_path: String,
    /// Destination on the host (file, or directory to copy into)
    pub host_path: String,
}

impl Fetch {
    pub fn run(&self, home: &VmHome) -> i32 {
        let meta = match resolve(home, &self.target) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        };
        if meta.state != VmState::Running {
            eprintln!("error: VM {} is not running (state: {})", meta.id, meta.state);
            return 1;
        }
        let key = meta.ssh_key_path(home);
        let src = format!("root@{}:{}", meta.ip, self.guest_path);
        let o = std::process::Command::new("scp")
            .args([
                "-i", &key.to_string_lossy(),
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "BatchMode=yes",
                "-o", "ConnectTimeout=10",
                "-r",
                &src,
                &self.host_path,
            ])
            .output();
        match o {
            Ok(o) if o.status.success() => {
                println!("fetched {} -> {}", self.guest_path, self.host_path);
                0
            }
            Ok(o) => {
                eprintln!("error: scp failed: {}", String::from_utf8_lossy(&o.stderr).trim());
                1
            }
            Err(e) => {
                eprintln!("error: scp: {e}");
                1
            }
        }
    }
}
