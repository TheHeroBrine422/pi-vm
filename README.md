# pi-vm

A CLI for creating Pi VM sandboxes. One binary supervises Cloud
Hypervisor + virtiofsd per VM and drops you straight into `pi` in the
project.

**Warning:** This project was written for personal use and is not polished.

## Layout

```
pi/         pi config
src/        the pi-vm CLI (Rust)
docs/       forwarding.md (reaching private/remote services from a VM)
```

## Install / Setup

Requirements:
- a Linux host with KVM (`/dev/kvm`); a dnf-based (Fedora/CachyOS)
  distro is expected for the package list below
- rust toolchain installed
- host packages (install once, with your own package manager):
  - `dnf install -y virtiofsd genisoimage iproute qemu-img iptables libguestfs`
  - (`genisoimage` + `libguestfs` are only needed for `pi-vm bake`)
- a Cloud Hypervisor binary — `pi-vm install` picks one up from `$CH_BIN`,
  an existing assets copy, or `PATH` (e.g. a terrapkg install); otherwise
  it downloads the static v53.0 build

```
cargo install --path .
pi-vm install
pi-vm bake
```

`pi-vm install` checks the requirements (printing the exact install command
for anything missing — it does not run your package manager itself), sets
up nested KVM, and fetches the assets (Cloud Hypervisor if not already
present, and the sha256-verified Fedora 44 cloud image).

## Commands

`pi-vm <path>` is shorthand for `pi-vm create <path>` — `create` is the
default subcommand: any bare first positional that is not a subcommand
name creates a VM (e.g. `pi-vm /tmp/proj`). A path that collides with a
subcommand name (e.g. a dir named `list`) must be given as `./list`.

```
pi-vm install           one-time host setup: check requirements (prints
                         the install command for anything missing), shared
                         VM network (br-pi-vm bridge + a per-host
                         address pool), CH + cloud image (sha256-verified),
                         nested KVM; prints a hint to run `pi-vm bake`
pi-vm bake [--pi-dir D] [--ssh-key F] [--timeout N]
                         rebuild the pre-baked base image (pi config +
                         full toolchain + your ssh pubkey + direct-boot
                         kernel assets) — the VM equivalent of a
                         container image's build-time COPY. The last
                         step also snapshots a warm template VM and
                         leaves it stopped: the next default-spec
                         create adopts it (see `create`) so its first
                         boot is a sub-second snapshot restore instead
                         of a cold boot
                         --ssh-key F: the host pubkey to bake into the
                         image (the guest's root trusts the host user's
                         ssh key; default: ~/.ssh/*.pub, which must
                         resolve to exactly one key)
pi-vm [create] [path] [--name N] [--vcpus 4] [--memory 8192] [--disk 100]
              [--kvm on|off] [--squash-uid U] [--no-mount]
              [--shell] [--timeout N] [--rm]
                         new VM from the pre-baked base (no path, or
                         --no-mount, = no shared mount: no virtiofsd, no
                         /workspace in the guest); boots and (if the
                         terminal is interactive) attaches an ssh session
                         that starts in pi (cd /workspace; pi).
                         A default-spec create (4 vCPU / 8 G / 100 G)
                         adopts the bake-time warm template when the
                         pool is available: the first boot is a
                         snapshot restore (a few seconds to ssh instead
                         of a cold boot); any other spec falls back to
                         a normal cold boot
                         --timeout N: stop the VM N seconds after launch
                         (boot time counts against it)
                         --rm: when the VM stops normally (the attach
                         session ends, or --timeout fires), delete it
                         (disk + state + network) instead of leaving it
                         stopped; a failed boot or a failed attach does
                         not delete it
pi-vm resume <id|name> [--shell]
           [--timeout N] [--rm]
                         boot an existing VM and attach; --rm deletes
                         the VM when it stops normally (the attach
                         session ends, or --timeout fires) instead of
                         leaving it stopped
pi-vm attach <id|name>
                         attach a plain shell to a running VM (not the
                         pi session); the VM keeps running when you exit
                         (the supervisor that booted it owns the VM)
pi-vm list [-v] [--filter KEY=VALUE]... [--sort created|last-run|state|name|id]
              [--reverse]
                         all VMs (docker ps -a style; alias: pi-vm ls)
                         --filter (repeatable, AND): state=running|stopped,
                         name=<prefix>, project=<prefix>, id=<prefix>
                         --sort: order the rows (state: running first;
                         nameless first by name); --reverse flips the
                         order; no --sort keeps the default order
pi-vm logs <id|name> [-f] [-n N]
                         VM console log (follow with -f)
pi-vm rm <id|name>... [--force]
                         delete one or more VMs (disk + state + network);
                         all targets are processed, a failure on one does
                         not stop the rest; a stale "running" record
                         (the supervisor is gone, e.g. after a host
                         reboot or kill -9 of the supervisor) is
                         reconciled to stopped automatically — only a
                         genuinely running VM needs --force
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
   Networking is a private bridge with NAT — no port overlap with the
   host, and VMs cannot reach each other (inter-VM traffic is dropped;
   internet-only). Never relabel host dirs (no `restorecon` /
   `semanage` on shared paths).

## How it works

- **One-shell model** — the VM lives exactly as long as the `pi-vm
  create`/`resume` process runs. On attach you land straight in `pi` at
  `/workspace`; exiting `pi` ends the ssh session and stops the VM
  (graceful guest poweroff). `--shell` attaches a plain shell instead
  of pi (exiting the shell stops the VM). The disk persists —
  `pi-vm resume` reboots it. With `--rm`, the VM is deleted (disk +
  state + network) when it stops normally instead of left stopped.
  Need the VM to outlive your terminal? Run
  `pi-vm` inside tmux.
- **Terminal tab name** — an interactive attach sets the terminal tab
  name to `pi-vm <first-8-of-id> <project-basename>` (the basename is
  omitted for `--no-mount` VMs) via an OSC escape sequence sent to the
  terminal before the session command runs (only when stdout is a tty —
  piped output is left untouched).
- **Ownership** — files the agent writes in the guest land on the host
  owned by you: virtiofsd squashes every guest uid to one host uid
  (default: the owner of the shared project dir; for `--no-mount` VMs,
  the invoking user).
- **Networking** — all VMs share one Linux bridge (`br-pi-vm`, set up
  once by `pi-vm install`). The VM address space is a single `/24`
  (gateway .1) in the 172.16.2-251 space, **chosen once at install time
  to not overlap any live host interface** (a fixed `172.16.0.0/12` pool
  would route into docker0's `172.17.0.0/16`) and persisted in
  `network.conf`. Each VM gets its own TAP (a random MAC the host
  generates) and a **static IP picked from the pool** at create; that
  config (ip / gateway / mac / instance-id) is written to a small raw
  **config disk** (`config.raw`) the host prepares before every boot, and
  the baked `pi-vm-net` unit reads it off `/dev/vdb` and statically
  configures the NIC. No DHCP, no ISO. Internet is via iptables NAT — no
  host port forwarding. Inter-VM traffic is blocked by **bridge port
  isolation** (each TAP is an isolated port — it can reach the host side
  of the bridge but never other VMs; bridged traffic never touches
  iptables, so this is the real policy) with an iptables FORWARD DROP as
  the fallback. The supervisor knows the IP up front (it's the static
  pick), so it can probe ssh from `t=0`. A nested pi-vm (running inside
  one of these VMs) runs its own bridge in its own chosen `/24`; an
  explicit route makes the pool reachable at each level. The bridge/tap
  is verified before boot; a failure aborts with a clear error instead of
  burning the ssh timeout.
- **Pre-baked base image** — `pi-vm bake` builds the base image: Fedora 44
  cloud + your pi config + the full toolchain (git, ripgrep, fd, node,
  python3, cargo/rust, uv, docker + compose, latest pi +
  pi-web-access + pi-subagents + pi-goal-x) + your ssh pubkey + the
  direct-boot kernel assets. New VMs boot ready — no first-boot
  install. The per-boot `pi-vm-net` unit (config-disk network read,
  virtiofs mount, disk grow, sshd) is baked in too. Git is pre-configured for sandbox use: safe.directory
  is disabled entirely (uid-squashed files don't trigger 'dubious ownership')
  and GPG signing is off (commits don't attempt to sign — the guest has no
  gpg program), with a default user.name/email. Re-run `pi-vm bake` after
  changing the pi config (or the ssh key).
  Each new VM records the base image's sha256 in meta.json (shown by
  `pi-vm ls -v`), so you can tell whether it is based on the most
  recent bake (compare against `pi-vm version`).
- **Warm snapshot pool** — the last step of `pi-vm bake` boots a
  template VM to full readiness, pauses it, and snapshots it
  (Cloud Hypervisor `vm.snapshot`, on-demand memory mode) before
  leaving it stopped as the pool. The next default-spec create
  (4 vCPU / 8 G / 100 G) adopts the pool: instead of a cold boot it
  restores the snapshot into a fresh CH instance — ssh-ready in a
  few seconds (the on-demand page-in of the guest's working set
  dominates; the raw restore itself is sub-second). The template's
  IP/MAC are fixed (the snapshot bakes them in), so the adopter
  reuses the pool slot's pair. The pool is single-use per bake:
  the adopter's meta overwrites the template's, and any other spec
  (or a missing pool) falls back to a normal cold boot. A resumed
  adopted VM boots cold from the template's disk (the snapshot is
  one-shot). The shared mount is re-provisioned on the host side —
  virtiofs survives the virtiofsd rebind, so no guest-side agent
  is needed.
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
- **What does Ctrl-C do?** Ctrl-C works while booting (exit 130: the VM is
  torn down the graceful way, the disk is left intact for `pi-vm resume`).
  Once attached it goes to the remote session first (docker-attach
  semantics) — the ssh client then exits on the signal (130, propagated
  like any remote status), the VM stops the same graceful way, and the
  disk persists for `pi-vm resume` (a `--rm` VM is deleted — a stop is a
  stop). In short: while attached, Ctrl-C = interrupt the guest, then
  stop the VM — reconnect with `pi-vm resume` to continue. For
  non-interactive runs, Ctrl-C/SIGTERM interrupts (exit 130) and tears the
  VM down the same graceful way. The disk is left intact for `pi-vm
  resume`. `kill <pi-vm-pid>` (SIGTERM) works in every state — while
  attached it stops the ssh session and tears the VM down the same
  graceful way; a SIGTERM stop of an attached --rm VM counts as a normal
  stop (the VM is deleted). `kill -9` no longer orphans the VM either:
  every child carries PR_SET_PDEATHSIG, so a dead supervisor SIGTERMs CH
  (immediate host-side poweroff) + virtiofsd + the ssh client.
- **What does Ctrl-Z do?** It is caught and ignored (you get a one-line
  hint): pi-vm does not suspend, so the VM and the attached ssh session
  stay up (the VM daemons ignore SIGTSTP too). Stop the VM by exiting the
  guest session — Ctrl-C while attached interrupts the guest session and
  then stops the VM (see above); while booting or in a non-interactive
  run, Ctrl-C stops the VM (exit 130).
- **Why does ssh take a while to come up?** The baked `pi-vm-net` unit
  runs a convergence loop (up to 60 s) that reads the per-VM config disk,
  statically configures the NIC, mounts virtiofs, and keeps sshd running,
  self-healing (e.g. interface renames). If ssh never becomes reachable
  within the deadline, `create`/`resume` exits 4 (check the bridge +
  `network.conf` with `pi-vm install`).
- **Can I run two pi-vm processes on one VM?** No — each VM dir is
  flock-protected (exit 5).
- **Why do `create`/`resume` refuse to boot?** Missing or stale
  direct-boot assets (e.g. a bake from before this feature) — re-run
  `pi-vm bake`.

## Gotchas

- SIGKILL of CH mid-write can tear the qcow2 disk (L2 table update is not
  atomic with the data write) — the disk may need recreating. Stop
  gracefully (guest poweroff / SIGTERM). `kill -9 pi-vm` is safe-ish by
  contrast: the children carry PR_SET_PDEATHSIG, so a dead supervisor
  SIGTERMs CH (immediate poweroff — unflushed guest data may be lost, same
  as the SIGTERM fallback), and the next resume self-heals the refcount
  table if the poweroff landed mid-write.
- `vmm.shutdown` API = `vm_delete()` in CH — abrupt, NOT graceful. Never
  use it for VMs whose disk must be re-opened.
- **Nested virtiofs**: when the shared folder itself lives on a virtiofs
  mount (e.g. running pi-vm inside one of its own VMs), guest file
  creation in the mount can fail with `Unknown system error -116`
  (ENOKEY) — observed on pi session-file creation. Workaround: put the
  shared folder on the VM's local disk (e.g. /tmp) for nested use.
- virtiofsd lives in `/usr/libexec` on Fedora (not in PATH).
- The guest's root partition/fs grow to `--disk` on first boot via the
  cloud image's growpart/resizefs (the base is 5G virtual).

## TODO

- [ ] packaging: an RPM spec for pi-vm (Requires: the seven host packages;
  in that world `pi-vm install` shrinks to: check requirements, CH from
  PATH/$CH_BIN, nested KVM, cloud image) — then publish to terrapkg
- [x] `install`: stop running the package manager — list the dnf
  packages in the README and just check + report what's missing
- [x] `install`: find a `cloud-hypervisor` already on `PATH` (e.g. a
  terrapkg install) before downloading the static binary ($CH_BIN >
  existing assets copy > PATH > download)
- [ ] give the agent more info about its own environment: write a small
  `vminfo.json` (subset of the host's meta.json: id, vcpus/memory/disk,
  bridge/ip/mac, project semantics — shared path + uid squash, host os +
  CH version, the create flags) into `<project>/.pi-vm/` before boot;
  referenced from the baked AGENTS.md. `--no-mount` VMs: via the ISO for
  now, via the config disk (see the performance item) later. (Deliberately
  NOT a copy of the pi-vm source in the image: it would go stale at bake
  time, and "what am I?" is a data file, not a codebase to grep.)
- [x] reuse between `bake.rs` and `vm.rs` — resolved by the NoCloud ISO
  removal: the per-boot net/ssh/mount/grow setup is now single-sourced in
  the baked `pi-vm-net` unit (reads the per-VM config disk; the ssh key
  is baked in)
- [x] performance: avoid generating the cloud-init NoCloud ISO at all —
  done: shared-bridge + per-VM config disk + baked ssh key (no per-boot
  ISO, no per-VM ssh-keygen, no DHCP; the bake's prep ISO stays —
  build-time, not per-boot)
- [ ] performance (remaining): set up the network and start CH in parallel
  (the TAP setup is now pre-boot), and a slimmer kernel

- overlayfs to make the individual vhds smaller
- gh cli/creds because api rate limits are a bitch, but also not letting it have write access to my accounts
- Separate VM work trees via CoW for a single project directory (tbh maybe unnecessary)
- a better solution for syncing pi directory
  - virtiofs share? would be cool for syncing sessions between VMs too
- better ways to deal with long lasting commands. we likely need to see if pi can force commands to have a max timeout of lets say 10 minutes and never let the LLM do a timeout higher then that. maybe even 5 minutes tbh
- maybe a shortcut for --no-mount for just asking quick questions
- possibly add forwarding commands as a separate sub command that inserts the info about it into an agent's session and allows me to just run one extra command rather then dealing with all of it manually
- ability to set extensions to be baked into the image as a setting for pi-vm rather then it being hard coded
- have install check for cloud-hypervisor binaries on PATH and other assets before downloading to have better compatibility with other install methods (ex: rpm spec)
  - related: make install not reliant on dnf.
- possibly keep older versions of the baked image and add a `pi-vm images` command?
- codemode
- maybe change this where you need to run it with sudo or similar to like the root access thing docker does since having sudo in the script is annoying cause if it times out you have to type in the password at the start and end of the session
- /tmp getting full is a pain and it keeps randomly happening cause tmpfs is only 4gb by default
- better tailscale aperture support. need to auto include the /etc/hosts file and figure out a better networking solution
- Later goals (not required for v1):
  - Simplify subagents (fewer options for subagent types)
    - might also be a good idea to add a note to the AGENTS.md file to use subagents in cases where it is doing multiple large tasks that can be done in parallel
    - it would be cool for there to be a good way for it to spin up subagents in a separate VM
  - A review fanout mechanism using the worktrees feature: spawn N (default
    4) agents with separate worktrees, have them thoroughly review and test
    the project and place the output at ISSUES.md, then have another agent
    dedup the file and place it back in the original project path
  - A way to exclude files (credentials)



Pi vm <path> being create or resume with it telling you if it will resume.

Resume maybe does pi -r


- possible performance improvements:
  - do network setup and ch start in parallel
  - slim kernel for faster CH boot
  - sudo improvements to not pause part way through boot (mentioned earlier)
  - fix the qcow refcount thing that might already be fixed

- rm on stopped vm sometimes records as running
```
caleb@ultramarine ~/Code via 🐍 v3.14.7 
❯ pi-vm rm d2b616fc6188 4ac8eac726a5 2ff126d9bd8a d913929010b8 e087b7bf254b
error: VM d2b616fc6188 is recorded as running (pid Some(684860)) — use --force to kill it first
[sudo] password for caleb: 
deleted VM 4ac8eac726a5
error: VM 2ff126d9bd8a is recorded as running (pid Some(2579999)) — use --force to kill it first
deleted VM d913929010b8
error: VM e087b7bf254b is recorded as running (pid Some(1049000)) — use --force to kill it first


caleb@ultramarine ~/Code via 🐍 v3.14.7 took 8s 
❯ pi-vm version                                                                    
pi-vm 0.1.0
cloud-hypervisor cloud-hypervisor v53.0
Migration Protocol Versions: 0
cloud image sha256 28680fe5b371a5a82ebf43a31926e086a168e59949d03969c5093e7071f90b7f
pre-baked image sha256 263a01e3fa6fbb4ed9ce92514e1c3b959debeb42f89759f3cc578ef5b0d403de
```
## Data layout

```
~/.local/share/pi-vm/            (override with PI_VM_HOME)
  assets/            cloud-hypervisor, kernel, initramfs, kernel-cmdline
                     (direct-boot assets, from `bake`)
  images/            fedora44-cloud.qcow2, fedora44-cloud-prepped.qcow2
  vms/<12-hex-id>/   meta.json, disk.qcow2, config.raw, console.log, .lock
  network.conf     the chosen VM /24 pool + gateway (persists across runs)
```

Per-VM folder + meta.json (no database): the folder is the source of truth;
`rm -rf` of a VM dir = delete VM; the `.lock` (flock) is the source of
truth for "in use".

## Exit codes

| code | meaning |
|---|---|
| 0 | success |
| 1 | generic error (incl. attach: VM not running) |
| 2 | usage / preflight / not-found (rm/logs bad target; missing image, no KVM, name collision, network; resume bad target) |
| 3 | VM failed to boot (CH exited during boot) / bake verification failed |
| 4 | ssh did not become reachable within the timeout |
| 5 | VM is in use by another pi-vm process |
| 124 | bake prep boot timed out |
| 130 | interrupted (Ctrl-C / SIGTERM) — the disk is intact for resume; --rm does not delete |

Note: Ctrl-C while attached to the ssh session goes to the remote session
(docker-attach semantics); the VM stops when the ssh session ends (for the
default attach, when you `exit` pi).
