//! pi-vm — docker-like CLI for the pi VM sandbox (Cloud Hypervisor + virtiofsd).
//!
//! Thin dispatcher: parse -> VmHome -> command.run(). One file per command in
//! `commands/`.

mod commands;
mod state;
mod util;
mod vm;

use std::process::ExitCode;

use clap::Parser;

use state::VmHome;

#[derive(Parser)]
#[command(name = "pi-vm", version, about = "docker-like CLI for the pi VM sandbox")]
struct Cli {
    #[command(subcommand)]
    cmd: commands::Cmd,
}

fn main() -> ExitCode {
    restore_sigpipe_default(); // `pi-vm version | head` must not panic with BrokenPipe
    let cli = Cli::parse();
    let home = VmHome::new();
    ExitCode::from(cli.cmd.run(&home) as u8)
}

/// Restore the default SIGPIPE handler so writes to a closed pipe end the
/// process with signal 13 (exit 141) instead of a Rust BrokenPipe panic.
#[cfg(unix)]
fn restore_sigpipe_default() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    unsafe {
        signal(13, 0); // SIGPIPE=13, SIG_DFL=0
    }
}
#[cfg(not(unix))]
fn restore_sigpipe_default() {}
