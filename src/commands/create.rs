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
    /// guest's timer starts pi in a tmux session 'agent' with that file
    /// as its first message (pi stays interactive, so `pi-vm send` can
    /// steer it). Requires a shared project (a --no-mount VM has no
    /// /workspace for the prompt).
    #[arg(long)]
    pub prompt_file: Option<String>,
    /// Stop the VM N seconds after launch (boot time counts against it —
    /// pass a budget with a boot buffer)
    #[arg(long)]
    pub timeout: Option<u64>,
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
        // makes the VM an agent VM (the guest timer triggers on presence).
        let agent_mode = match &self.prompt_file {
            Some(pf) => match &project {
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
                return 2;
            }
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
