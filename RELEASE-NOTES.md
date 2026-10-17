# Release notes — v0.2.0 (tag at `410232c` = 83524ea + version-embed)

Everything from the last few weeks **except** the snapshot-restore work
(that ships in 0.2.1/0.3.0 once the reusable master is verified). One-line
per change:

## What changed (user-visible)

1. **Networking: per-VM config disks, no more DHCP/NoCloud** (the big one).
   Each VM gets a static IP from a per-host `/24` pool (chosen at
   `install` time, persisted in `network.conf`); a 1 MB config disk
   carries `vm-ip/vm-gw/vm-mac/instance-id`, read by the baked
   `pi-vm-net` unit — no ISO, no DHCP, no lease polling. The supervisor
   probes the known IP from t=0. Inter-VM traffic is blocked by bridge
   port isolation (the real policy) with iptables FORWARD DROP as
   fallback; internet via NAT.
   **Clean break: VMs created by older versions fast-fail with a recreate
   message (see Upgrade below).**
2. **`bake --ssh-key`**: defaults to `~/.ssh/*.pub` (must resolve to
   exactly one key); bakes the host pubkey + `PermitRootLogin yes` into
   the image. Bake now *verifies* the key landed and retries the
   libguestfs writeback (a "successful" guestfish session can lose writes
   while the previous one's qcow2 writeback settles); an unverified
   image fails the bake (exit 3) instead of shipping broken ssh.
3. **Base image pre-grown to 100 G** + guest-side fs sizing: a
   `--disk 40` VM still works (the virtual disk is never shrunk — the
   fs is shrunk online by `pi-vm-net`). Default `--disk` is now 100.
4. **Boot timing line**: every boot prints a per-phase breakdown
   (supervisor: lock / net+tap / virtiofsd / ch-spawn; guest:
   ch-spawn → ssh-ready; plus the disk copy/resize times) — the single
   place to see where the seconds go.
5. **`ls --filter KEY=VALUE` + `--sort` + `--reverse`**; `create` is the
   default subcommand; `--rm` on create/resume (delete on normal stop, no
   longer triggered by a failed attach); Ctrl-C while attached no longer
   hard-kills the VM; `rm` reconciles stale "running" records.
6. **Removed**: the autoresearch/agent-VM machinery (send/fetch,
   --prompt-file, tmux agent, LAUNCH_SCRIPT), apt host support, the
   NoCloud-ISO + dhclient/dnsmasq machinery. dnf is the expected host
   distro. (The loop lives in its own project now.)
7. **Bake also pre-configures git** (safe.directory off, GPG off, default
   identity) and the image ships the full toolchain + pi + docker
   (autostart, not a multi-user requirement).
8. **`pi-vm version` now includes the git commit**
   (`pi-vm 0.2.0+<sha>`) so old/new builds are distinguishable.

## Upgrade procedure (from a pre-release version)

1. **Back up data in existing VMs first** (they can't be migrated — see 3).
   `pi-vm attach <name>` + rsync/scp out whatever you need, with the OLD
   binary still installed.
2. Install the new binary from the tag
   (`git fetch --tags && git checkout v0.2.0 && cargo install --path .`).
   `pi-vm version` should print `pi-vm 0.2.0+410232c`.
3. `pi-vm install` (re-checks host requirements; keeps the existing
   `network.conf` pool if present).
4. `pi-vm bake` (rebuilds the base image; ~15–20 min without the
   snapshot pool). Requires exactly one `~/.ssh/*.pub` or pass
   `--ssh-key`.
5. Recreate VMs: `pi-vm create ...`. Booting/resuming a VM created by an
   older version fast-fails with a recreate message (by design — the
   config-disk + base-image generation changed).

## Verifying the release (your e2e pass)

- **Full (one command, 10–15 min):** `bash /workspace/e2e.sh` — creates 3
  VMs (default / parallel / 40 G shrink check), checks ssh + pty +
  internet (NAT) + inter-VM isolation both ways, resumes two, deletes
  all, runs `cargo test`. Exits 0 = green.
- **Quick manual (3 min):**
  ```
  pi-vm create /tmp/proj --name t --timeout 60   # ssh in when ready
  pi-vm resume t --timeout 60
  pi-vm rm --force t
  ```

## Known limitations (this tag)

- No snapshot-restore yet (next release): every create is a cold boot
  (~15–23 s on a 4-vCPU host, host-load dependent).
- On a small host (≤ 8 G RAM), don't run several 8 G VMs concurrently.
- Timing: cold create ~15–23 s, resume ~16–23 s (4-vCPU dev box; the
  host's load matters a lot — measure same-window if comparing).
