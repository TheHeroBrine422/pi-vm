//! `pi-vm inspect` — full metadata for one VM.

use clap::Args;

use crate::state::{resolve, VmHome};

#[derive(Args)]
pub struct Inspect {
    /// VM id, unique id prefix, or name
    pub target: String,
}

impl Inspect {
    pub fn run(&self, home: &VmHome) -> i32 {
        match resolve(home, &self.target) {
            Ok(meta) => {
                let json = serde_json::to_string_pretty(&meta).expect("serialize meta");
                println!("{json}");
                println!(
                    "\ndir:     {}\ndisk:    {}\nssh:     {}\nconsole: {}\nbase img: {}",
                    meta.vm_dir(home).display(),
                    meta.disk_path(home).display(),
                    meta.ssh_key_path(home).display(),
                    meta.console_path(home).display(),
                    meta.base_image_sha256
                        .clone()
                        .unwrap_or_else(|| "(unknown — created before this field, or the hash was unavailable)".into())
                );
                0
            }
            Err(e) => {
                eprintln!("error: {e}");
                2
            }
        }
    }
}
