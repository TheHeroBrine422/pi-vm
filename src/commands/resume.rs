//! `pi-vm resume` — boot an existing VM and attach ssh.

use std::io::IsTerminal;

use clap::Args;

use crate::state::VmHome;
use crate::vm::{boot, resume_vm};

#[derive(Args)]
pub struct Resume {
    /// VM id, unique id prefix, or name
    pub target: String,
    /// Show the VM console live while waiting for boot (instead of a spinner)
    #[arg(long)]
    pub console: bool,
    /// Attach to a plain shell in /workspace instead of starting the agent
    #[arg(long)]
    pub shell: bool,
}

impl Resume {
    pub fn run(&self, home: &VmHome) -> i32 {
        let attach = std::io::stdin().is_terminal();
        let meta = match resume_vm(home, &self.target) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        };
        match boot(home, meta, attach, self.console, self.shell) {
            Ok(code) => code,
            Err(f) => {
                eprintln!("error: {}", f.msg());
                f.code()
            }
        }
    }
}
