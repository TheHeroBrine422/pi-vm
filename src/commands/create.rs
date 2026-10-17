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
    /// KVM on|off (CH requires KVM; off = no nested)
    #[arg(long, default_value = "on")]
    pub kvm: String,
    /// Squash all guest uids to this host uid (default: owner of the project dir)
    #[arg(long)]
    pub squash_uid: Option<u32>,
    /// Attach to a plain shell in /workspace instead of starting pi
    #[arg(long)]
    pub shell: bool,
    /// Stop the VM N seconds after launch (boot time counts against it —
    /// pass a budget with a boot buffer)
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
        // Validate --kvm before any side effects (the create_vm error
        // branch only rolls back what create_vm itself created).
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
        let rm_meta = meta.clone(); // for the --rm auto-delete after boot
        match boot(home, meta, attach, self.shell, self.timeout, self.rm) {
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
