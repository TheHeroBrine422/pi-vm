//! `pi-vm create` — create a new VM from the pre-baked base and attach ssh.

use std::io::IsTerminal;

use clap::Args;

use crate::state::VmHome;
use crate::vm::{boot, create_vm, VmConfig};

#[derive(Args)]
pub struct Create {
    /// Host path shared into the VM at /workspace (omit, or use --no-mount, for a VM with no shared mount)
    pub path: Option<String>,
    /// No shared mount at all (no virtiofsd, no /workspace in the guest)
    #[arg(long)]
    pub no_mount: bool,
    /// Optional unique VM name
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long, default_value_t = 4)]
    pub vcpus: u32,
    #[arg(long, default_value_t = 8192)]
    pub memory: u32,
    #[arg(long, default_value_t = 80)]
    pub disk: u32,
    /// Extra scratch disk in GB (mounted at /mnt/extra)
    #[arg(long, default_value_t = 0)]
    pub extra_disk: u32,
    /// KVM on|off (CH requires KVM; off = no nested)
    #[arg(long, default_value = "on")]
    pub kvm: String,
    /// Squash all guest uids to this host uid (default: owner of the project dir)
    #[arg(long)]
    pub squash_uid: Option<u32>,
    /// Show the VM console live while waiting for boot (instead of a spinner)
    #[arg(long)]
    pub console: bool,
    /// Attach to a plain shell in /workspace instead of starting the agent
    #[arg(long)]
    pub shell: bool,
    /// Agent prompt (host path): copied into <project>/prompt.txt — the
    /// pi-vm supervisor starts pi in a tmux session 'agent' with that
    /// file as its first message (pi stays interactive, so `pi-vm send`
    /// can steer it). Requires a shared project (a --no-mount VM has no
    /// /workspace for the prompt).
    #[arg(long)]
    pub prompt_file: Option<String>,
    /// Stop the VM N seconds after launch (boot time counts against it —
    /// pass a budget with a boot buffer)
    #[arg(long)]
    pub timeout: Option<u64>,
    /// Delete the VM (disk + state + network) when it stops normally
    /// (the attach session ends, or --timeout fires) instead of leaving
    /// it stopped. A failed boot or a failed attach session does NOT
    /// delete it (investigate with `pi-vm logs` / `pi-vm resume`).
    #[arg(long)]
    pub rm: bool,
}

impl Create {
    pub fn run(&self, home: &VmHome) -> i32 {
        let project = match (&self.path, self.no_mount) {
            (Some(_), true) => {
                eprintln!("error: a path and --no-mount are contradictory — drop one");
                return 2;
            }
            (Some(p), false) => Some(p.clone()),
            _ => None,
        };
        // Agent mode: the prompt must end up in the shared folder (the
        // guest reads /workspace/prompt.txt — no ISO delivery involved).
        // The flag copies it; a hand-placed <project>/prompt.txt also
        // makes the VM an agent VM (the supervisor triggers on presence).
        let mut wrote_prompt = false;
        let agent_mode = match &self.prompt_file {
            Some(pf) => match &project {
                Some(p) => {
                    let content = std::fs::read_to_string(pf).unwrap_or_else(|e| {
                        eprintln!("error: cannot read prompt file {pf}: {e}");
                        std::process::exit(2);
                    });
                    let dest = std::path::Path::new(p).join("prompt.txt");
                    // Remember whether the file pre-existed: a failed
                    // create must not leave our copy behind (prompt.txt
                    // presence is the agent-mode trigger — a leftover
                    // would silently make the *next* create an agent VM),
                    // but it must not delete a user-placed one either.
                    let pre_existing = dest.exists();
                    std::fs::write(&dest, content)
                        .unwrap_or_else(|e| {
                            eprintln!("error: cannot write {p}/prompt.txt: {e}");
                            std::process::exit(2);
                        });
                    if !pre_existing {
                        wrote_prompt = true;
                    }
                    true
                }
                None => {
                    eprintln!("error: --prompt-file requires a shared project (the prompt lands in <project>/prompt.txt; a --no-mount VM has no /workspace)");
                    return 2;
                }
            },
            None => project.as_ref()
                .map(|p| std::path::Path::new(p).join("prompt.txt").exists())
                .unwrap_or(false),
        };
        let kvm = match self.kvm.as_str() {
            "on" => true,
            "off" => false,
            _ => {
                eprintln!("error: --kvm must be 'on' or 'off'");
                return 2;
            }
        };
        let cfg = VmConfig {
            project,
            name: self.name.clone(),
            vcpus: self.vcpus,
            memory_mb: self.memory,
            disk_gb: self.disk,
            extra_disk: self.extra_disk,
            kvm,
            squash_uid: self.squash_uid,
        };
        let attach = std::io::stdin().is_terminal();
        let meta = match create_vm(home, &cfg) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {e}");
                if wrote_prompt {
                    // roll back the prompt we just wrote — a failed
                    // create must not turn the next create into an agent
                    // VM (see above); a user-placed prompt.txt is kept.
                    let _ = std::fs::remove_file(
                        std::path::Path::new(&cfg.project.clone().unwrap()).join("prompt.txt"),
                    );
                    println!("rolled back the written prompt.txt (the create failed)");
                }
                return 2;
            }
        };
        let rm_meta = meta.clone(); // for the --rm auto-delete after boot
        match boot(home, meta, attach, self.console, self.shell, agent_mode, self.timeout, self.rm) {
            Ok(code) => {
                if self.rm {
                    // Only a normal stop reaches here (attach session ended,
                    // or --timeout fired): a failed boot AND a failed attach
                    // session return Err from boot, so the VM is not
                    // deleted. Delete via the rm logic (lock is free — boot
                    // released it). force=true: the teardown already killed
                    // the children; the running-check is belt-and-braces.
                    let name = rm_meta.name.as_deref().unwrap_or(&rm_meta.id);
                    println!("==> --rm: deleting VM {name} (disk + state + network)");
                    if crate::commands::rm::delete_vm(home, &rm_meta, true) != 0 {
                        eprintln!("error: --rm auto-delete failed — VM {} still exists; delete it with `pi-vm rm {}`", rm_meta.id, rm_meta.id);
                        return 1;
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
