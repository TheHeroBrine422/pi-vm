//! `pi-vm create` — create a new VM from the pre-baked base and attach ssh.

use std::io::IsTerminal;

use clap::Args;

use crate::state::VmHome;
use crate::vm::{boot, create_vm, PromptRun, VmConfig};

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
    /// Headless: run `pi --print` with this prompt file, capture the log,
    /// then stop the VM (non-interactive only; needs a shared mount)
    #[arg(long)]
    pub prompt_file: Option<String>,
    /// Kill the prompt run after N seconds (exit code 124)
    #[arg(long)]
    pub timeout: Option<u32>,
    /// Guest pi --session-dir, a path under /workspace (default .pi-vm/sessions)
    #[arg(long)]
    pub session_dir: Option<String>,
    /// Leave the VM running after the prompt run (pi-vm resume <id>)
    #[arg(long)]
    pub keep: bool,
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
        let kvm = match self.kvm.as_str() {
            "on" => true,
            "off" => false,
            _ => {
                eprintln!("error: --kvm must be 'on' or 'off'");
                return 2;
            }
        };
        let prompt = match &self.prompt_file {
            Some(pf) => {
                if self.no_mount || project.is_none() {
                    eprintln!("error: --prompt-file requires a shared mount (a path)");
                    return 2;
                }
                if self.shell {
                    eprintln!("error: --prompt-file and --shell are contradictory");
                    return 2;
                }
                Some(PromptRun {
                    prompt_file: pf.clone(),
                    timeout_s: self.timeout,
                    session_dir: self.session_dir.clone().unwrap_or_else(|| ".pi-vm/sessions".into()),
                    keep: self.keep,
                })
            }
            None => None,
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
        match boot(home, meta, attach, self.console, self.shell, prompt) {
            Ok(code) => code,
            Err(f) => {
                eprintln!("error: {}", f.msg());
                f.code()
            }
        }
    }
}
