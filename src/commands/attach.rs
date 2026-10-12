//! `pi-vm attach` — attach a plain shell to a RUNNING VM (not the pi
//! session). The VM keeps running after you exit: the supervisor (the
//! pi-vm process that booted it) owns the VM's lifetime. Ctrl-C goes to
//! the remote session (docker-attach semantics); SIGTERM to this process
//! ends the ssh session.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use clap::Args;

use crate::state::{resolve, VmHome, VmState};

#[derive(Args)]
pub struct Attach {
    /// VM id, unique id prefix, or name
    pub target: String,
}

impl Attach {
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
        if let Some(pid) = meta.ch_pid
            && !std::path::Path::new(&format!("/proc/{pid}")).exists()
        {
            eprintln!("error: VM {} is not running (supervisor pid {pid} is gone)", meta.id);
            return 1;
        }

        println!(
            "==> attached to {} ({}) — the VM keeps running when you exit",
            meta.id,
            meta.name.as_deref().unwrap_or("(no name)")
        );

        // Same signal model as the boot attach in vm.rs: Ctrl-C (SIGINT)
        // is registered but deliberately NOT checked — the terminal
        // delivers it to the foreground group and ssh forwards it to the
        // remote session. SIGTERM ends the ssh session. Ctrl-Z (SIGTSTP)
        // is registered (hint only) so this process does not suspend;
        // the ssh child ignores SIGTSTP itself (with_pdeathsig).
        let interrupted = Arc::new(AtomicBool::new(false));
        let terminated = Arc::new(AtomicBool::new(false));
        let tstp = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, interrupted.clone());
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, terminated.clone());
        let _ = signal_hook::flag::register(signal_hook::consts::SIGTSTP, tstp.clone());

        let key = meta.ssh_key_path(home);
        let mut ssh = std::process::Command::new("ssh");
        ssh.args(crate::vm::ssh_opts(&key))
            .arg("-t") // interactive shell needs a pty
            .arg(format!("root@{}", meta.ip))
            .arg("cd /workspace; bash");
        crate::vm::with_pdeathsig(&mut ssh);
        match ssh
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
        {
            Err(e) => {
                eprintln!("error: ssh: {e}");
                1
            }
            // Poll rather than block on status(): a SIGTERM must end the
            // ssh session even while attached (a blocking status() would
            // make `kill <attach-pid>` a no-op).
            Ok(mut child) => loop {
                if tstp.swap(false, Ordering::Relaxed) {
                    eprintln!("Ctrl-Z ignored while attached — the VM stays up; exit the shell (or Ctrl-C) to end the attach");
                }
                if terminated.load(Ordering::Relaxed) {
                    crate::vm::libc_kill(child.id(), 15);
                }
                match child.try_wait() {
                    Ok(Some(s)) => {
                        let code = s.code().unwrap_or(1);
                        break (code != 0) as i32; // 0 = clean shell exit, 1 = any error
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(200)),
                    Err(e) => {
                        eprintln!("error: ssh: {e}");
                        break 1;
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{new_id, VmMeta};

    fn test_meta(id: &str, state: VmState, ch_pid: Option<i32>) -> VmMeta {
        VmMeta {
            id: id.into(),
            name: Some("t".into()),
            created: 0,
            updated: 0,
            project: None,
            subnet: "172.16.55.0/24".into(),
            ip: "172.16.55.2".into(),
            mac: "52:54:00:00:00:55".into(),
            vcpus: 1,
            memory_mb: 1024,
            disk_gb: 80,
            kvm: false,
            squash_uid: 1000,
            state,
            ch_pid,
            base_image_sha256: None,
        }
    }

    #[test]
    fn attach_unknown_target_exits_2() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let a = Attach { target: "nope".into() };
        assert_eq!(a.run(&home), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn attach_stopped_vm_exits_1() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let id = new_id();
        test_meta(&id, VmState::Stopped, None).save(&home).unwrap();
        let a = Attach { target: id };
        assert_eq!(a.run(&home), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn attach_stale_running_vm_exits_1() {
        // A "running" record whose supervisor pid is gone is reconciled to
        // stopped on load (and the /proc check is the belt-and-suspenders
        // second guard) — either way, exit 1.
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let id = new_id();
        test_meta(&id, VmState::Running, Some(99999)).save(&home).unwrap();
        let a = Attach { target: id };
        assert_eq!(a.run(&home), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
