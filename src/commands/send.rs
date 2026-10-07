//! `pi-vm send` — type a message into a running VM's agent (the tmux
//! session 'agent'). The message lands in pi's input box as keystrokes
//! (Enter included); a newline in the message becomes a space (a newline
//! in the pty would be an Enter and submit early). Only works for agent
//! VMs (created with --prompt-file/--session-dir, or resumed with them)
//! while the VM is running.

use clap::Args;

use crate::state::{resolve, VmHome, VmState};

#[derive(Args)]
pub struct Send {
    /// VM id, unique id prefix, or name
    pub target: String,
    /// Message to type into the agent's input box
    #[arg(short, long)]
    pub message: String,
}

impl Send {
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
        // Sanitize: a newline in pty input is an Enter (early submit).
        let msg = self.message.replace('\n', " ").replace('\r', " ");
        // Escape for the remote shell (single-quote context).
        let q = msg.replace('\\', "\\\\").replace('\'', "'\\''");
        let cmd = format!("tmux send-keys -t agent '{q}' Enter");
        let key = meta.ssh_key_path(home);
        let o = std::process::Command::new("ssh")
            .args([
                "-i", &key.to_string_lossy(),
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "BatchMode=yes",
                "-o", "ConnectTimeout=10",
                &format!("root@{}", meta.ip),
                &cmd,
            ])
            .output();
        match o {
            Ok(o) if o.status.success() => {
                println!(
                    "sent to {} ({})",
                    meta.id,
                    meta.name.as_deref().unwrap_or("(no name)")
                );
                0
            }
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                eprintln!("error: send failed: {} (is the VM an agent VM, with the tmux session 'agent' running?)", err.trim());
                1
            }
            Err(e) => {
                eprintln!("error: ssh: {e}");
                1
            }
        }
    }
}
