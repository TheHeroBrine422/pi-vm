# pi-vm

A CLI for creating Pi VM sandboxes. One binary supervises Cloud
Hypervisor + virtiofsd per VM and drops you straight into `pi` in the
project.

**Warning:** This code was written for personal use and has not been polished. Expect rough edges. 

## Layout

```
pi/         pi config
src/        the pi-vm CLI (Rust)
docs/       forwarding.md (reaching private/remote services from a VM)
```

## Install / Setup

Requirements:
- dnf based system (fedora)
- rust toolchain installed

```
cargo install --path .
pi-vm install
pi-vm bake
```

## Commands

```
pi-vm install [--check]   one-time host setup: packages, CH +
                         cloud image (sha256-verified), nested KVM;
                         prints a hint to run `pi-vm bake`
pi-vm bake [--pi-dir D] [--timeout N]
                         rebuild the pre-baked base image (pi config +
                         full toolchain + direct-boot kernel assets) —
                         the VM equivalent of a container image's
                         build-time COPY
pi-vm create [path] [--name N] [--vcpus 4] [--memory 8192] [--disk 80]
              [--extra-disk N] [--kvm on|off] [--squash-uid U] [--no-mount]
              [--console] [--shell]
                         new VM from the pre-baked base (no path, or
                         --no-mount, = no shared mount: no virtiofsd, no
                         /workspace in the guest); boots and (if the
                         terminal is interactive) attaches an ssh session
                         that starts in pi (cd /workspace; pi)
pi-vm resume <id|name> [--console] [--shell]
                         boot an existing VM and attach
pi-vm list [-v]         all VMs (docker ps -a style; alias: pi-vm ls)
pi-vm inspect <id|name> full metadata (meta.json + paths)
pi-vm logs <id|name> [-f] [-n N]
                         VM console log (follow with -f)
pi-vm rm <id|name>... [--force]
                         delete one or more VMs (disk + state + network);
                         all targets are processed, a failure on one does
                         not stop the rest
pi-vm doctor [--detailed]
                         host diagnostics: KVM, packages, assets, images
pi-vm version           CLI + CH versions, image hashes
```

`<id|name>` resolves by exact id, unique id prefix, or name (docker-style).

## Guarantees

1. **Clean shared folder (ownership)** — every file the agent writes to the
   shared project, **including files written by non-root users the agent
   creates**, must be owned by the host user on the host (the host user
   reviews the agent's work in a normal IDE + git). Mechanism: virtiofsd
   `--translate-uid/-gid squash-guest` (all guest uids → the host user).
2. **Full-featured environment** — the agent gets a whole machine: `/dev`,
   nested KVM (`/dev/kvm` in the guest), block devices, kernel modules,
   docker + compose, the full toolchain. Don't trade this away for
   simplicity.
3. **Don't break the host** — the VM is isolated (KVM); the only host
   interactions are the shared folder (controlled by #1) and networking.
   Networking is a private docker-like bridge — no port overlap with the
   host. Never relabel host dirs (no `restorecon` / `semanage` on shared
   paths).

## How it works

- **One-shell model** — the VM lives exactly as long as the `pi-vm
  create`/`resume` process runs. On attach you land straight in `pi` at
  `/workspace`; exiting `pi` ends the ssh session and stops the VM
  (graceful guest poweroff). `--shell` attaches a plain shell instead of
  the agent (exiting the shell stops the VM). The disk persists —
  `pi-vm resume` reboots it. Need the VM to outlive your terminal? Run
  `pi-vm` inside tmux.
- **Ownership** — files the agent writes in the guest land on the host
  owned by you: virtiofsd squashes every guest uid to one host uid
  (default: the owner of the shared project dir; for `--no-mount` VMs,
  the invoking user).
- **Networking** — each VM gets a docker-like bridge (TAP + Linux bridge +
  iptables NAT) on a private `172.16.N.0/24` — no host port forwarding.
  Subnet allocation checks both pi-vm's state **and the host's live
  interfaces**, so a pi-vm running inside one of its own VMs can't
  collide with the parent's networking. The bridge/tap is verified before
  boot; a failure aborts with a clear error instead of burning the ssh
  timeout.
- **Pre-baked base image** — `pi-vm bake` builds the base image: Fedora 44
  cloud + your pi config + the full toolchain (git, ripgrep, fd, node,
  python3, cargo/rust, uv, docker + compose, latest pi + pi-web-access +
  pi-subagents + pi-goal-x) + the direct-boot kernel assets. New VMs boot ready — no
  first-boot install. Re-run `pi-vm bake` after changing the pi config.
- **Direct kernel boot** — no UEFI, no GRUB: CH loads the kernel +
  initramfs straight from the baked assets (~5–8 s faster than a firmware
  boot). The console is a virtio-console (`hvc0`); `pi-vm logs` shows the
  kernel boot messages. Missing/stale assets → `create`/`resume` fail
  preflight (exit 2) with a pointer at `pi-vm bake`.
- **Boot speed** — fresh VM to ssh-ready: median ~25 s (min ~16 s) on a
  2-vCPU nested-KVM host; 5/5 benchmark runs reach ssh, every stop clean.
  On a normal host it lands near the min. Benchmark on your host with
  `tools/bench-boot.sh` (5 runs, min/median/avg/stddev/max; `--warmup` for
  a cold first run).

## FAQ

- **Does my data persist across stop/resume?** Yes — the disk is never
  re-copied on resume, and the stop path is a guest-initiated poweroff
  (30 s cap, then SIGTERM → SIGKILL fallback). The only data-loss paths
  are a forced stop (the guest fails to power off within the 120 s window,
  or a manual kill of the process).
- **What does Ctrl-C do?** Ctrl-C works while booting. Once attached it
  goes to the remote session (docker-attach semantics), and the VM stops
  when the ssh session ends — for the default attach, when you exit `pi`
  — via a graceful guest poweroff. For non-interactive runs, Ctrl-C/SIGTERM
  interrupts (exit 130) and tears the VM down the same graceful way. The
  disk is left intact for `pi-vm resume`.
- **Why does ssh take a while to come up?** The baked `pi-vm-net` unit
  runs a convergence loop (up to 60 s) that brings up the network + ssh as
  soon as the underlying devices are ready, in parallel with sshd, and
  self-heals (e.g. interface renames). If ssh never becomes reachable,
  `create`/`resume` exits 4.
- **Can I run two pi-vm processes on one VM?** No — each VM dir is
  flock-protected (exit 5).
- **Why do `create`/`resume` refuse to boot?** Missing or stale
  direct-boot assets (e.g. a bake from before this feature) — re-run
  `pi-vm bake`.

## Gotchas

- SIGKILL of CH mid-write can tear the qcow2 disk (L2 table update is not
  atomic with the data write) — the disk may need recreating. Stop
  gracefully (guest poweroff / SIGTERM).
- `vmm.shutdown` API = `vm_delete()` in CH — abrupt, NOT graceful. Never
  use it for VMs whose disk must be re-opened.
- virtiofsd lives in `/usr/libexec` on Fedora (not in PATH).
- The guest's root partition/fs grow to `--disk` on first boot via the
  cloud image's growpart/resizefs (the base is 5G virtual).

## TODO

- Separate VM work trees via CoW
- tap/bridge don't appear to be cleaned up properly
- A better solution for long-running commands (the LLM loses control until
  the command finishes, which is bad if a task has a problem halfway
  through)
- pi-vm attach command to attach a shell to a running vm
- --rm flag, and maybe a shortcut for --no-mount for just asking quick questions
- fix bug where all cqow's ref tables are being rebuilt on every create even after a valid build
- possibly add forwarding commands as a separate sub command that inserts the info about it into an agent's session and allows me to just run one extra command rather then dealing with all of it manually
- fix git signatures failing for commits inside the sandbox
- Later goals (not required for v1):
  - Block git write actions (make git read-only somehow)
  - Simplify subagents (fewer options for subagent types)
    - might also be a good idea to add a note to the AGENTS.md file to use subagents in cases where it is doing multiple large tasks that can be done in parallel
  - A review fanout mechanism using the worktrees feature: spawn N (default
    4) agents with separate worktrees, have them thoroughly review and test
    the project and place the output at ISSUES.md, then have another agent
    dedup the file and place it back in the original project path
  - A way to exclude files (credentials)

## Data layout

```
~/.local/share/pi-vm/            (override with PI_VM_HOME)
  assets/            cloud-hypervisor, kernel, initramfs, kernel-cmdline
                     (direct-boot assets, from `bake`)
  images/            fedora44-cloud.qcow2, fedora44-cloud-prepped.qcow2
  vms/<12-hex-id>/   meta.json, disk.qcow2, ssh_key, console.log, .lock
```

Per-VM folder + meta.json (no database): the folder is the source of truth;
`rm -rf` of a VM dir = delete VM; the `.lock` (flock) is the source of
truth for "in use".

## Exit codes

| code | meaning |
|---|---|
| 0 | success |
| 1 | generic error / not-found (rm/inspect/logs bad target) |
| 2 | usage / preflight (missing image, no KVM, name collision, network; resume bad target) |
| 3 | VM failed to boot (CH exited during boot) / bake verification failed |
| 4 | ssh did not become reachable within the timeout |
| 5 | VM is in use by another pi-vm process |
| 124 | bake prep boot timed out |
| 130 | interrupted (Ctrl-C / SIGTERM) |

Note: Ctrl-C while attached to the ssh session goes to the remote session
(docker-attach semantics); the VM stops when the ssh session ends (for the
default attach, when you `exit` pi).
