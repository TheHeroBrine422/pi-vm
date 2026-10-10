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
    /// Re-copy the agent prompt into <project>/prompt.txt (the seed of the
    /// agent VM — the guest timer starts pi with it; a hand-placed
    /// prompt.txt also makes the VM an agent VM)
    #[arg(long)]
    pub prompt_file: Option<String>,
    /// Stop the VM N seconds after launch
    #[arg(long)]
    pub timeout: Option<u64>,
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
        let agent_mode = match &self.prompt_file {
            Some(pf) => match &meta.project {
                Some(p) => {
                    let content = std::fs::read_to_string(pf).unwrap_or_else(|e| {
                        eprintln!("error: cannot read prompt file {pf}: {e}");
                        std::process::exit(2);
                    });
                    std::fs::write(std::path::Path::new(p).join("prompt.txt"), content)
                        .unwrap_or_else(|e| {
                            eprintln!("error: cannot write {p}/prompt.txt: {e}");
                            std::process::exit(2);
                        });
                    true
                }
                None => {
                    eprintln!("error: --prompt-file requires a shared project (this VM has no /workspace)");
                    return 2;
                }
            },
            None => meta.project.as_ref()
                .map(|p| std::path::Path::new(p).join("prompt.txt").exists())
                .unwrap_or(false),
        };
        match boot(home, meta, attach, self.console, self.shell, agent_mode, self.timeout) {
            Ok(code) => code,
            Err(f) => {
                eprintln!("error: {}", f.msg());
                f.code()
            }
        }
    }
}
