//! One file per command; each exposes an `Args` struct (clap) with a `run`
//! method. `Cmd` is the subcommand enum and the single dispatch point.

pub mod create;
pub mod doctor;
pub mod inspect;
pub mod install;
pub mod list;
pub mod logs;
pub mod rm;
pub mod resume;
pub mod version;
pub mod bake;

use clap::Subcommand;

use crate::state::VmHome;

#[derive(Subcommand)]
pub enum Cmd {
    /// Show CLI + asset versions and image hashes
    Version,
    /// Diagnose the host (KVM, packages, assets, images)
    Doctor(doctor::Doctor),
    /// List VMs (docker ps -a style; alias: ls)
    #[command(alias = "ls")]
    List(list::List),
    /// Show full metadata for one VM
    Inspect(inspect::Inspect),
    /// Delete one or more VMs (disk + state + network). --force kills a running instance first.
    Rm(rm::Rm),
    /// Create a new VM from the pre-baked base and attach an ssh session
    Create(create::Create),
    /// Boot an existing VM and attach an ssh session
    Resume(resume::Resume),
    /// Install host prerequisites (packages, assets) and bake the base image
    Install(install::Install),
    /// Rebuild the pre-baked base image (pi config + toolchain)
    Bake(bake::Bake),
    /// Show the VM console log (boot/runcmd debugging)
    Logs(logs::Logs),
}

impl Cmd {
    pub fn run(self, home: &VmHome) -> i32 {
        match self {
            Cmd::Version => version::run(home),
            Cmd::Doctor(args) => args.run(home),
            Cmd::List(args) => args.run(home),
            Cmd::Inspect(args) => args.run(home),
            Cmd::Rm(args) => args.run(home),
            Cmd::Create(args) => args.run(home),
            Cmd::Resume(args) => args.run(home),
            Cmd::Install(args) => args.run(home),
            Cmd::Bake(args) => args.run(home),
            Cmd::Logs(args) => args.run(home),
        }
    }
}
