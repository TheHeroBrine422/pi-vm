//! `pi-vm resume` — boot an existing VM and attach ssh.

use std::io::IsTerminal;

use clap::Args;

use crate::state::VmHome;
use crate::vm::{boot, resume_vm};

#[derive(Args)]
pub struct Resume {
    /// VM id, unique id prefix, or name
    pub target: String,
    /// Attach to a plain shell in /workspace instead of starting the agent
    #[arg(long)]
    pub shell: bool,
    /// Re-copy the agent prompt into <project>/prompt.txt (the seed of the
    /// agent VM — the supervisor starts pi with it; a hand-placed
    /// prompt.txt also makes the VM an agent VM)
    #[arg(long)]
    pub prompt_file: Option<String>,
    /// Stop the VM N seconds after launch
    #[arg(long)]
    pub timeout: Option<u64>,
    /// Delete the VM (disk + state + network) when it stops normally
    /// (the attach session ends, or --timeout fires) instead of leaving
    /// it stopped. A failed boot, a failed attach session, or an
    /// interrupted (Ctrl-C/SIGTERM) non-interactive run does NOT delete
    /// it (investigate with `pi-vm logs` / `pi-vm resume`).
    #[arg(long)]
    pub rm: bool,
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
        let rm_meta = meta.clone(); // for the --rm auto-delete after boot
        match boot(home, meta, attach, self.shell, agent_mode, self.timeout, self.rm) {
            Ok(code) => {
                if self.rm {
                    // Only a normal stop reaches here (attach session
                    // ended, --timeout fired, SIGTERM to an attached run,
                    // or the guest powered itself off): a failed boot,
                    // a failed attach session, and an interrupted
                    // non-interactive run all return Err from boot, so
                    // the VM is not deleted. force=FALSE on purpose: a
                    // "running" record with a LIVE pid here means
                    // someone resumed the VM in the stop→delete window —
                    // its CH must not be killed (a stale record's dead
                    // pid is reconciled by delete_vm, no force needed).
                    let name = rm_meta.name.as_deref().unwrap_or(&rm_meta.id);
                    println!("==> --rm: deleting VM {name} (disk + state + network)");
                    let rc = crate::commands::rm::delete_vm(home, &rm_meta, false);
                    if rc != 0 {
                        eprintln!("error: --rm auto-delete failed (code {rc}) — the VM may still exist; check `pi-vm ls`");
                        return rc;
                    }
                }
                code
            }
            Err(f) => {
                eprintln!("error: {}", f.msg());
                f.code()
            }
        }
    }
}
