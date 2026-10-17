//! pi-vm — docker-like CLI for the pi VM sandbox (Cloud Hypervisor + virtiofsd).
//!
//! Thin dispatcher: parse -> VmHome -> command.run(). One file per command in
//! `commands/`.

mod commands;
mod state;
mod util;
mod vm;

use std::process::ExitCode;

use clap::{CommandFactory, Parser};

use state::VmHome;

#[derive(Parser)]
#[command(
    name = "pi-vm",
    version = concat!(env!("CARGO_PKG_VERSION"), "+", env!("GIT_COMMIT")),
    about = "docker-like CLI for the pi VM sandbox",
    long_about = "docker-like CLI for the pi VM sandbox\n\nA bare first positional that is not a subcommand name is treated as `create <path>`: `pi-vm /tmp/proj` = `pi-vm create /tmp/proj`."
)]
struct Cli {
    /// Subcommand (optional: a bare first positional is re-dispatched as
    /// `create <path>` before parsing; no args shows the help)
    #[command(subcommand)]
    cmd: Option<commands::Cmd>,
}

/// Names a bare first positional must not be, or it is re-dispatched as
/// `create <path>`: all subcommand names + aliases, plus clap's built-in
/// `help`. Derived from the command so new subcommands are picked up.
fn known_subcommands() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for sub in Cli::command().get_subcommands() {
        names.push(sub.get_name().to_string());
        names.extend(sub.get_aliases().map(String::from));
        names.extend(sub.get_visible_aliases().map(String::from));
    }
    names.push("help".to_string());
    names
}

fn main() -> ExitCode {
    restore_sigpipe_default(); // `pi-vm version | head` must not panic with BrokenPipe
    let args: Vec<String> = std::env::args().collect();
    // `pi-vm <path>` (bare first positional, not a subcommand) = `pi-vm create <path>`
    let args = if args.len() > 1
        && !args[1].starts_with('-')
        && !known_subcommands().contains(&args[1])
    {
        let mut with_create = args;
        with_create.insert(1, "create".to_string());
        with_create
    } else {
        args
    };
    let cli = Cli::parse_from(&args);
    let home = VmHome::new();
    match cli.cmd {
        Some(cmd) => ExitCode::from(cmd.run(&home) as u8),
        // No args: show the help (stderr + exit 2, as when the subcommand
        // was required).
        None => {
            eprintln!("{}", Cli::command().render_help());
            ExitCode::from(2)
        }
    }
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
