//! `pi-vm rm` — delete one or more VMs (disk + state + network).
//!
//! All targets are processed in order; a failure on one does not stop the
//! rest. The exit code is the most severe per-target code (0 = all deleted,
//! 1 = generic error, 2 = not-found, 5 = in use / running without --force).

use clap::Args;

use crate::state::{resolve, VmHome, VmState};

#[derive(Args)]
pub struct Rm {
    /// VM id, unique id prefix, or name (one or more)
    #[arg(required = true)]
    pub targets: Vec<String>,
    /// Kill a running instance before deleting
    #[arg(long)]
    pub force: bool,
}

impl Rm {
    pub fn run(&self, home: &VmHome) -> i32 {
        // Pass 1: resolve all targets up front (before any deletion), so
        // `rm abc abc123` (prefix + full id of the same VM) is a duplicate
        // rather than a not-found error, and a prefix that only matches one
        // VM keeps its invocation-time meaning.
        let mut metas: Vec<crate::state::VmMeta> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let mut rc = 0;
        for target in &self.targets {
            match resolve(home, target) {
                Ok(m) if seen.contains(&m.id) => println!(
                    "skipping {}: already specified in this invocation",
                    m.name.as_deref().unwrap_or(&m.id)
                ),
                Ok(m) => {
                    seen.push(m.id.clone());
                    metas.push(m);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    rc = rc.max(2);
                }
            }
        }
        // Pass 2: delete in order; a failure on one does not stop the rest.
        for meta in &metas {
            rc = rc.max(self.delete_one(home, meta));
        }
        rc
    }

    /// Delete one resolved VM. Returns the per-target exit code (0/1/5).
    fn delete_one(&self, home: &VmHome, meta: &crate::state::VmMeta) -> i32 {
        // raw load (no reconcile) to see the recorded state
        let raw: crate::state::VmMeta = serde_json::from_str(
            &std::fs::read_to_string(meta.meta_path(home)).unwrap_or_else(|_| "{}".into()),
        )
        .unwrap_or(meta.clone());
        if raw.state == VmState::Running {
            if !self.force {
                eprintln!(
                    "error: VM {} is recorded as running (pid {:?}) — use --force to kill it first",
                    meta.id, raw.ch_pid
                );
                return 5;
            }
            if let Some(pid) = raw.ch_pid {
                let _ = std::process::Command::new("kill").arg(pid.to_string()).output();
            }
            // also kill any CH supervising this disk (belt and braces)
            if let Ok(o) = std::process::Command::new("pgrep")
                .arg(format!("-f{}", meta.disk_path(home).display()))
                .output()
            {
                for line in String::from_utf8_lossy(&o.stdout).lines() {
                    let _ = std::process::Command::new("kill").arg(line.trim()).output();
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        let _lock = match crate::state::VmLock::acquire(meta, home) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        };
        crate::state::cleanup_network(meta);
        match crate::state::remove_vm(meta, home) {
            Ok(()) => {
                println!("deleted VM {}", meta.name.as_deref().unwrap_or(&meta.id));
                0
            }
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{new_id, VmMeta};

    fn make_vm(home: &VmHome, id: &str, name: Option<&str>) {
        let meta = VmMeta {
            id: id.into(),
            name: name.map(String::from),
            created: 0,
            updated: 0,
            project: Some("/tmp".into()),
            subnet: "172.16.9.0/24".into(),
            ip: "172.16.9.2".into(),
            mac: "52:54:00:00:00:09".into(),
            vcpus: 1,
            memory_mb: 1024,
            disk_gb: 80,
            extra_disk: false,
            kvm: false,
            squash_uid: 1000,
            state: VmState::Stopped,
            ch_pid: None,
        };
        meta.save(home).unwrap();
    }

    fn rm(targets: Vec<String>) -> Rm {
        Rm { targets, force: false }
    }

    #[test]
    fn rm_multiple_ids() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let a = new_id();
        let b = new_id();
        make_vm(&home, &a, Some("alpha"));
        make_vm(&home, &b, Some("beta"));
        assert_eq!(rm(vec![a.clone(), b.clone()]).run(&home), 0);
        assert!(!home.vms_dir().join(&a).exists());
        assert!(!home.vms_dir().join(&b).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rm_multiple_partial_failure_continues() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let a = new_id();
        make_vm(&home, &a, None);
        // "nope" does not exist; the existing VM must still be deleted
        assert_eq!(rm(vec!["nope".into(), a.clone()]).run(&home), 2);
        assert!(!home.vms_dir().join(&a).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rm_dedups_same_vm() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let a = new_id();
        make_vm(&home, &a, None);
        // full id + its own prefix = the same VM: deleted once, no error
        assert_eq!(rm(vec![a.clone(), a[..4].into()]).run(&home), 0);
        assert!(!home.vms_dir().join(&a).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rm_single_target_unchanged() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        // not-found still exits 2
        assert_eq!(rm(vec!["nope".into()]).run(&home), 2);
        let a = new_id();
        make_vm(&home, &a, Some("solo"));
        // delete by name still exits 0
        assert_eq!(rm(vec!["solo".into()]).run(&home), 0);
        assert!(!home.vms_dir().join(&a).exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
