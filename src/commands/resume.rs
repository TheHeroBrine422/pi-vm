//! `pi-vm resume` — boot an existing VM and attach ssh.

use std::io::IsTerminal;

use clap::Args;

use crate::state::{VmHome, VmMeta};
use crate::vm::{boot, resume_vm};

#[derive(Args)]
pub struct Resume {
    /// VM id, unique id prefix, or name
    pub target: String,
    /// vCPU count for this resume (default: the recorded value — a change
    /// is recorded, so later plain resumes keep it). A resume is a cold
    /// boot from the VM's disk, so a different spec is just a different
    /// CH launch on the same disk.
    #[arg(long)]
    pub vcpus: Option<u32>,
    /// Memory in MB for this resume (default: the recorded value — a
    /// change is recorded, so later plain resumes keep it)
    #[arg(long)]
    pub memory: Option<u32>,
    /// Attach to a plain shell in /workspace instead of starting pi
    #[arg(long)]
    pub shell: bool,
    /// Stop the VM N seconds after launch
    #[arg(long)]
    pub timeout: Option<u64>,
    /// Delete the VM (disk + state + network) when it stops normally (the
    /// attach session ends, or --timeout fires) instead of leaving it
    /// stopped. A failed boot, a failed attach session, or an
    /// interrupted (Ctrl-C/SIGTERM) non-interactive run does NOT delete
    /// it (investigate with `pi-vm logs` / `pi-vm resume`).
    #[arg(long)]
    pub rm: bool,
}

impl Resume {
    pub fn run(&self, home: &VmHome) -> i32 {
        let attach = std::io::stdin().is_terminal();
        let mut meta = match resume_vm(home, &self.target) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        };
        if let Err(e) = apply_spec_override(&mut meta, self.vcpus, self.memory) {
            eprintln!("error: {e}");
            return 2;
        }
        let rm_meta = meta.clone(); // for the --rm auto-delete after boot
        match boot(home, meta, attach, self.shell, self.timeout, self.rm) {
            Ok(code) => {
                if self.rm {
                    // Only a normal stop reaches here (attach session
                    // ended, --timeout fired, SIGTERM to an attached run,
                    // or the guest powered itself off): a failed boot, a
                    // failed attach session, and an interrupted
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

/// Apply the resume spec override (vcpus / memory MB) to the loaded meta.
/// Absent (None) = keep the recorded value; 0 = an error (the flags are
/// Option, so 0 can only be an explicit --vcpus 0 / --memory 0). A
/// change is PRINTED and PERSISTED (boot() records the running state
/// with the new values, so a later plain `pi-vm resume` keeps them —
/// docker-update semantics; the disk carries no spec, so there is
/// nothing else to reconcile).
///
/// The one case that needs a heads-up: a pending adoption (the VM was
/// created from the master snapshot but has never booted — meta.snapshot
/// is still set). Its FIRST boot is a restore, and the restore runs with
/// the snapshot's baked-in topology (the master's spec) — CH's bare-VMM
/// restore path takes no --cpus/--memory at all — so the override applies
/// from the SECOND boot. The meta is still saved with the new values, so
/// no re-asking is needed.
pub fn apply_spec_override(meta: &mut VmMeta, vcpus: Option<u32>, memory_mb: Option<u32>) -> Result<(), String> {
    let pending_adopt = meta.snapshot;
    let mut changed = false;
    if let Some(v) = vcpus {
        if v == 0 {
            return Err("--vcpus must be >= 1".into());
        }
        if v != meta.vcpus {
            println!("   vcpus: {} -> {} (recorded)", meta.vcpus, v);
            meta.vcpus = v;
            changed = true;
        }
    }
    if let Some(m) = memory_mb {
        if m == 0 {
            return Err("--memory must be >= 1 (MB)".into());
        }
        if m != meta.memory_mb {
            println!("   memory: {} MB -> {} MB (recorded)", meta.memory_mb, m);
            meta.memory_mb = m;
            changed = true;
        }
    }
    if changed && pending_adopt {
        println!(
            "   note: this VM has not booted yet — its first boot is the master's restore ({} vCPU / {} MB, baked into the snapshot); the new spec applies from the second boot",
            meta.vcpus, meta.memory_mb
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_meta(snapshot: bool) -> VmMeta {
        VmMeta {
            id: "resumetest01".into(),
            name: Some("res".into()),
            created: 0,
            updated: 0,
            project: Some("/tmp".into()),
            subnet: "172.16.9.0/24".into(),
            ip: "172.16.9.2".into(),
            mac: "52:54:00:00:00:09".into(),
            vcpus: 4,
            memory_mb: 8192,
            disk_gb: 100,
            kvm: true,
            squash_uid: 0,
            base_image_sha256: None,
            state: crate::state::VmState::Stopped,
            ch_pid: None,
            net_gen: Some(3),
            snapshot,
        }
    }

    #[test]
    fn absent_flags_keep_recorded_spec() {
        let mut m = test_meta(false);
        apply_spec_override(&mut m, None, None).unwrap();
        assert_eq!((m.vcpus, m.memory_mb), (4, 8192));
    }

    #[test]
    fn explicit_override_changes_and_persists_in_meta() {
        let mut m = test_meta(false);
        apply_spec_override(&mut m, Some(8), Some(16384)).unwrap();
        assert_eq!((m.vcpus, m.memory_mb), (8, 16384));
        // a second application with the new values is a no-op
        apply_spec_override(&mut m, Some(8), Some(16384)).unwrap();
        assert_eq!((m.vcpus, m.memory_mb), (8, 16384));
    }

    #[test]
    fn one_flag_leaves_the_other_alone() {
        let mut m = test_meta(false);
        apply_spec_override(&mut m, Some(2), None).unwrap();
        assert_eq!((m.vcpus, m.memory_mb), (2, 8192));
    }

    #[test]
    fn zero_is_an_error() {
        let mut m = test_meta(false);
        assert!(apply_spec_override(&mut m, Some(0), None).is_err());
        assert!(apply_spec_override(&mut m, None, Some(0)).is_err());
        assert_eq!((m.vcpus, m.memory_mb), (4, 8192)); // untouched
    }

    #[test]
    fn pending_adoption_override_still_applies_to_meta() {
        // the override lands in the meta even though the first (restore)
        // boot runs the master's spec — it applies from boot two
        let mut m = test_meta(true);
        apply_spec_override(&mut m, Some(1), Some(2048)).unwrap();
        assert_eq!((m.vcpus, m.memory_mb), (1, 2048));
    }
}
