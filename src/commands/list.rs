//! `pi-vm list` — docker ps -a style (alias: `pi-vm ls`).

use clap::Args;

use crate::state::{fmt_ts, VmHome, VmMeta};
use crate::util::human;

#[derive(Args)]
pub struct List {
    /// Full details (all meta fields)
    #[arg(short, long)]
    pub verbose: bool,
}

impl List {
    pub fn run(&self, home: &VmHome) -> i32 {
        let ids = home.list_vm_ids();
        if ids.is_empty() {
            println!("no VMs (run `pi-vm create <path>` or `pi-vm create --no-mount` first — see `pi-vm doctor`)");
            return 0;
        }
        let metas: Vec<VmMeta> = ids
            .iter()
            .filter_map(|id| VmMeta::load(home, id).ok())
            .collect();

        if self.verbose {
            for meta in &metas {
                println!("=== {} {} ===", meta.id, meta.name.as_deref().unwrap_or("(no name)"));
                println!("  state:    {:?}", meta.state);
                println!("  project:  {}", meta.project.as_deref().unwrap_or("(no mount)"));
                println!("  network:  {} / {} / {}", meta.subnet, meta.ip, meta.mac);
                println!(
                    "  resources: {} vcpus, {} MB, {} GB disk, extra_disk={}, kvm={}, squash_uid={}",
                    meta.vcpus, meta.memory_mb, meta.disk_gb, meta.extra_disk, meta.kvm, meta.squash_uid
                );
                println!("  created:  {}", fmt_ts(meta.created));
                println!("  last run: {}", fmt_ts(meta.updated));
                println!(
                    "  disk:     {} ({})",
                    meta.disk_path(home).display(),
                    disk_size_str(&meta.disk_path(home))
                );
                println!();
            }
            return 0;
        }

        // state is converted to a String: this toolchain's width specifiers
        // only pad built-in string types, not custom Display impls — passing
        // the VmState enum directly printed "stopped" unpadded and shifted
        // every column after it one char left of the header. Variable widths
        // ({:name}) are also unsupported, so padding is done explicitly.
        let sep = "  ";
        let proj_w = metas
            .iter()
            .map(|m| m.project.as_deref().unwrap_or("-").len())
            .chain(std::iter::once("PROJECT".len()))
            .max()
            .unwrap();
        let disks: Vec<String> = metas.iter().map(|m| disk_size_str(&m.disk_path(home))).collect();
        let disk_w = disks
            .iter()
            .map(|d| d.len())
            .chain(std::iter::once("DISK".len()))
            .max()
            .unwrap();

        println!(
            "{}{}{}{}{}{}{}{}{}{}{}{}{}{}NAME",
            pad("ID", 12), sep, pad("STATE", 7), sep, pad("SUBNET", 15), sep,
            pad("CREATED", 23), sep, pad("LAST RUN", 23), sep, pad("PROJECT", proj_w), sep,
            pad("DISK", disk_w), sep
        );
        for (i, meta) in metas.iter().enumerate() {
            println!(
                "{}{}{}{}{}{}{}{}{}{}{}{}{}{}{}",
                pad(&meta.id, 12), sep,
                pad(&meta.state.to_string(), 7), sep,
                pad(&meta.subnet, 15), sep,
                pad(&fmt_ts(meta.created), 23), sep,
                pad(&fmt_ts(meta.updated), 23), sep,
                pad(meta.project.as_deref().unwrap_or("-"), proj_w), sep,
                pad(&disks[i], disk_w), sep,
                meta.name.as_deref().unwrap_or("-")
            );
        }
        0
    }
}

/// Pad `s` to at least `w` chars with trailing spaces (this toolchain's fmt
/// doesn't apply width specifiers to custom Display types, and variable
/// widths are unsupported).
fn pad(s: &str, w: usize) -> String {
    if s.len() >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - s.len()))
    }
}

fn disk_size_str(p: &std::path::Path) -> String {
    match std::fs::metadata(p) {
        Ok(m) => human(m.len()),
        Err(_) => "-".into(),
    }
}
