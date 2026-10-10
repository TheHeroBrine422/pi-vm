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
            rc = rc.max(delete_vm(home, meta, self.force));
        }
        rc
    }
}

/// Delete one VM: (if running and `force`) kill the recorded pid + any CH
/// supervising the disk, then lock, clean up the network, remove the VM
/// dir. Returns the per-target exit code (0 = deleted, 1 = error,
/// 5 = running without force).
///
/// A "running" record whose supervisor pid is dead (host reboot, kill -9
/// of the supervisor) is reconciled to stopped first — the stale record
/// does not block the deletion.
///
/// Also the `--rm` auto-delete path for `create`/`resume` (which call it
/// after a normal stop, with `force` true — the teardown already killed
/// the children; the running-check is then just belt-and-braces).
pub fn delete_vm(home: &VmHome, meta: &crate::state::VmMeta, force: bool) -> i32 {
    // raw load (no reconcile) to see the recorded state
    let raw: crate::state::VmMeta = serde_json::from_str(
        &std::fs::read_to_string(meta.meta_path(home)).unwrap_or_else(|_| "{}".into()),
    )
    .unwrap_or(meta.clone());
    // Pre-acquired lock from the force-kill polling loop below.
    let mut pre_lock: Option<crate::state::VmLock> = None;
    if raw.state == VmState::Running {
        // Reconcile: a "running" record whose supervisor pid is dead
        // (host reboot, kill -9 of the supervisor) is just a record of
        // the last session — rewrite meta.json to stopped and delete.
        let alive = raw.ch_pid.map(crate::state::pid_alive).unwrap_or(false);
        if !alive {
            let mut fixed = raw.clone();
            fixed.state = VmState::Stopped;
            fixed.ch_pid = None;
            fixed.updated = crate::state::now();
            if fixed.save(home).is_ok() {
                println!(
                    "note: VM {} was recorded as running (pid {}) but the supervisor is gone — treating as stopped",
                    meta.id,
                    raw.ch_pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
                );
            }
        } else if !force {
            eprintln!(
                "error: VM {} is running (pid {}) — use --force to kill it first",
                meta.id, raw.ch_pid.unwrap()
            );
            return 5;
        } else {
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
            // The flock is held by the SUPERVISOR process (not the killed
            // CH); its own teardown (guest poweroff attempt, network
            // cleanup, meta save) takes a few seconds. Poll for the lock
            // instead of guessing with a fixed sleep (P1: the old 2 s
            // sleep raced the supervisor and made --force flakily fail
            // with 'in use').
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                match crate::state::VmLock::acquire(meta, home) {
                    Ok(l) => {
                        pre_lock = Some(l);
                        break;
                    }
                    Err(_) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        return 1;
                    }
                }
            }
        }
    }
    let _lock = match pre_lock.take() {
        Some(l) => l,
        None => match crate::state::VmLock::acquire(meta, home) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{new_id, VmMeta};

    fn make_vm(home: &VmHome, id: &str, name: Option<&str>, state: VmState, ch_pid: Option<i32>) {
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
            base_image_sha256: None,
            state,
            ch_pid,
        };
        meta.save(home).unwrap();
    }

    fn rm(targets: Vec<String>) -> Rm {
        Rm { targets, force: false }
    }

    /// Spawn a short-lived process, kill + reap it, and return its (now
    /// dead) pid.
    fn dead_pid() -> i32 {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        let _ = child.kill();
        let _ = child.wait();
        pid
    }

    #[test]
    fn rm_multiple_ids() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let a = new_id();
        let b = new_id();
        make_vm(&home, &a, Some("alpha"), VmState::Stopped, None);
        make_vm(&home, &b, Some("beta"), VmState::Stopped, None);
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
        make_vm(&home, &a, None, VmState::Stopped, None);
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
        make_vm(&home, &a, None, VmState::Stopped, None);
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
        make_vm(&home, &a, Some("solo"), VmState::Stopped, None);
        // delete by name still exits 0
        assert_eq!(rm(vec!["solo".into()]).run(&home), 0);
        assert!(!home.vms_dir().join(&a).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_vm_direct_stopped_vm() {
        // The --rm auto-delete path (create/resume): a normally-stopped
        // VM is deleted via delete_vm with force=true — no running check
        // branch, just lock + network cleanup + dir removal.
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let a = new_id();
        make_vm(&home, &a, Some("autorm"), VmState::Stopped, None);
        let meta = resolve(&home, &a).unwrap();
        assert_eq!(delete_vm(&home, &meta, true), 0);
        assert!(!home.vms_dir().join(&a).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]

    fn rm_deletes_stale_running_without_force() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let a = new_id();
        // A stale "running" record: the supervisor pid is dead (host
        // reboot, kill -9 of the supervisor) — rm must not block on it.
        make_vm(&home, &a, None, VmState::Running, Some(dead_pid()));
        assert_eq!(rm(vec![a.clone()]).run(&home), 0);
        assert!(!home.vms_dir().join(&a).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rm_refuses_genuinely_running_without_force() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let a = new_id();
        // A genuinely running VM: the recorded pid is alive (our own test
        // process) — rm must still refuse without --force.
        make_vm(&home, &a, None, VmState::Running, Some(std::process::id() as i32));
        assert_eq!(rm(vec![a.clone()]).run(&home), 5);
        assert!(home.vms_dir().join(&a).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rm_force_deletes_genuinely_running() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let a = new_id();
        // A live stand-in for the supervisor (NOT our own pid — --force
        // kills it).
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        make_vm(&home, &a, None, VmState::Running, Some(child.id() as i32));
        assert_eq!(Rm { targets: vec![a.clone()], force: true }.run(&home), 0);
        assert!(!home.vms_dir().join(&a).exists());
        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_dir_all(&dir).ok();
    }
}
