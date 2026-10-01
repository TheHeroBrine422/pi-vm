//! `pi-vm logs` — VM console log (boot/runcmd debugging).
//!
//! The console file only captures the agetty (kernel/runcmd output goes to
//! the on-disk logs inside the guest — see the bake flow); still, this is
//! the first thing to look at when a boot misbehaves.

use std::io::Read;
use std::path::Path;

use clap::Args;

use crate::state::{resolve, VmHome};

#[derive(Args)]
pub struct Logs {
    /// VM id, unique id prefix, or name
    pub target: String,
    /// Follow the log (like tail -f)
    #[arg(short, long)]
    pub follow: bool,
    /// Show only the last N lines (default 40 without --follow)
    #[arg(short = 'n', long, default_value_t = 40)]
    pub lines: usize,
}

impl Logs {
    pub fn run(&self, home: &VmHome) -> i32 {
        let meta = match resolve(home, &self.target) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        };
        let path = meta.console_path(home);
        if !path.exists() {
            eprintln!("error: no console log at {} (has this VM ever booted?)", path.display());
            return 1;
        }
        if self.follow {
            return self.follow(&path);
        }
        match std::fs::read(&path) {
            Ok(data) => {
                let text = String::from_utf8_lossy(&data);
                let lines: Vec<&str> = text.lines().collect();
                for line in lines.iter().rev().take(self.lines) {
                    println!("{line}");
                }
                0
            }
            Err(e) => {
                eprintln!("error: read {}: {e}", path.display());
                1
            }
        }
    }

    /// tail -f: seek to the end, then poll for new data.
    fn follow(&self, path: &Path) -> i32 {
        use std::fs::File;
        use std::io::Seek;
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("error: open {path:?}: {e}");
                return 1;
            }
        };
        let _ = file.seek(std::io::SeekFrom::End(0));
        let mut buf = [0u8; 4096];
        loop {
            match file.read(&mut buf) {
                Ok(0) => std::thread::sleep(std::time::Duration::from_millis(500)),
                Ok(n) => print!("{}", String::from_utf8_lossy(&buf[..n])),
                Err(_) => return 1,
            }
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    }
}
