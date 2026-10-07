//! `pi-vm resume` — boot an existing VM and attach ssh.

use std::io::IsTerminal;

use clap::Args;

use crate::state::VmHome;
use crate::vm::{boot, resume_vm, AgentLaunch};

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
    /// Re-deliver an initial prompt to the agent (host path; the seed ISO is
    /// regenerated on every boot, so a resume can re-launch the agent with a
    /// new prompt)
    #[arg(long)]
    pub prompt_file: Option<String>,
    /// Re-deliver pi's session directory (host path inside the shared project
    /// -> /workspace/<rel>, or an absolute guest path)
    #[arg(long)]
    pub session_dir: Option<String>,
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
        // The shared project (for --session-dir mapping) is the VM's project.
        let agent = match (&self.prompt_file, &self.session_dir) {
            (None, None) => None,
            (pf, sd) => {
                let prompt = match pf.as_ref()
                    .map(|p| std::fs::read_to_string(p))
                    .transpose()
                {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("error: cannot read prompt file: {e}");
                        return 2;
                    }
                };
                let session_dir = match sd.as_ref()
                    .map(|p| crate::vm::session_dir_to_guest(meta.project.as_deref(), p))
                    .transpose()
                {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("error: {e}");
                        return 2;
                    }
                };
                Some(AgentLaunch {
                    script: crate::vm::build_launch_script(session_dir.as_deref()),
                    prompt,
                })
            }
        };
        match boot(home, meta, attach, self.console, self.shell, agent, self.timeout) {
            Ok(code) => code,
            Err(f) => {
                eprintln!("error: {}", f.msg());
                f.code()
            }
        }
    }
}
