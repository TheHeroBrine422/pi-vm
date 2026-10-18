# Boot improvements — living log

Goal: drive time-to-ssh-ready (create + resume) down, every experiment gated by
`e2e.sh` (exit 0) + 3-sample medians, measurable improvements committed
immediately.

## Current baseline (build `fcb98e7` = config-disk networking + virtiofsd poll +
0.5 s probe + docker detached from multi-user)

**Final overnight medians (same-window gate runs, 2026-10-17):**
**adopt (warm pool, default spec) 9.7 s** (11.7 / 9.0 / 9.7) · **cold
create 22.9 s** (20.9–24.2 window) · resume (cold) 22.3–23.6 s. The
`fcb98e7` baseline below (16.6 / 15.1 / 15.9 s) is from its own
less-loaded window — the host drifted ±30% over the night, so only
same-window comparisons are meaningful. The user-facing win:
a default-spec create adopts the bake-time snapshot pool and is ~13 s
faster than a cold boot in the same window (the raw restore is
sub-second; 9.7 s = the 8 G ondemand page-in).

| metric | value |
|---|---|
| create, total to ssh-ready | 13.5–16.1 s (samples: 13.5, 15.2, 16.1, 17.4, 13.7 — variance ±2 s) |
| create parallel (2nd VM) | 17.0–18.5 s |
| resume | 13.5–18.2 s |
| supervisor (pre-CH) | ~0.05 s (lock 0.00, net+tap 0.03, virtiofsd 0.02, ch-spawn 0.00) |
| disk copy + resize | 0.08–0.09 s + 0.03 s (sparse qcow2) |
| guest kernel | 3.9–4.3 s |
| guest initrd | 2.8 s |
| guest userspace → sshd listening | ~2.2 s |
| pi-vm-net (first boot) | ~2.1–3.3 s (growpart + btrfs dominate) |
| multi-user.target | 4.2–5.3 s userspace (docker detached 2026-10-17) |
| systemd "Startup finished" userspace number | 8.9–9.9 s — LATER than multi-user (see open question) |

## Open question — RESOLVED (2026-10-17, uspace2 journal, offline read)

`systemd-analyze`'s `Startup finished in … (userspace)` is **not**
multi-user. In this image it fires the instant **docker.service**
finishes (journal: `Started docker.service` at 35.853445, `Startup
finished in … 8.962 s (userspace)` at 35.853929) — the metric tracks
the last heavy unit activated in the startup window (docker is started
by pi-vm-net, a timer-driven unit). multi-user.target (the real
"system ready" point) reached at 4.0–5.3 s in userspace; the 8.5–9.9 s
number = multi-user + docker's remaining startup (~3–4 s). Our metric
is ssh-ready, which is unaffected (sshd listens ~2.2 s into userspace;
ssh-ready = kernel 4.2 + initrd 2.8 + userspace-to-ssh ~7). Note:
anything that watches "boot complete" (systemd's own accounting) sees
the docker number.

## Committed so far (2026-10-17)

| commit | change | effect |
|---|---|---|
| 43f50d3 | config-disk networking (static pool IPs, no DHCP/dnsmasq) | removes lease wait + 2 s poll |
| df27945 | per-phase boot timing line | diagnostics |
| c4a3bdb | virtiofsd socket poll (−1.00 s); 0.5 s probe cadence; docker autostart but not a multi-user requirement (multi-user 9.07→4.21 s) | −~1.8 s typical create |
| fcb98e7 | gitignore .pi/ | housekeeping |

## Experiment log

(format: date, change, 3-sample before→after medians, kept/reverted + commit)

- 2026-10-17 **0.3.0 reusable snapshot master — first green on a fresh box, 3 gate samples**: full chain rebuilt from scratch (deps, `cargo install` main, `install`, `bake`, `e2e`): the bake's unverified warm step worked first try ("system settled — 90 s quiesce" then snapshot 204 then "snapshot master ready"). 3 consecutive GATE-OK; same-window totals:
  | metric | s1 | s2 | s3 | median |
  |---|---|---|---|---|
  | adopt A (1st create) | 9.3 | 9.1 | 9.0 | **9.1 s** |
  | adopt B (parallel, serialized) | 9.1 | 9.5 | 9.2 | **9.2 s** |
  | resume A (cold) | 15.6 | 14.6 | 15.6 | **15.6 s** |
  | resume B (cold) | 15.7 | 15.1 | 16.3 | **15.7 s** |
  | cold C (--disk 40 fallback) | 17.2 | 15.5 | 24.8 | **17.2 s** |
  Adopt is 6-8 s faster than cold in the same window (~45 %). **The restore-phase breakdown** (new `[restore]` line) explains the rest: a single adopt = adopt-lock 0.02 + **restore 8.82** + master-ssh 1.17 + renumber 1.38 s. The `vm.restore` 204 call itself is 8.8 s here: Copy reads the 8 G memory file, and it is no longer page-cache-resident (a 7.7 G host cannot hold it; the bake's one-time pre-warm is gone). On a 61 G host the file stays cached and the same call is ~1-2 s — adoption there should land ~3-4 s total (the section-8 expectation was calibrated to the warm-file A/B). **New experiment candidate: OnDemand on a cold cache** — the warm-file A/B (Copy 1.45 vs OnDemand 2.71 to ssh) does not transfer to a cold cache: OnDemand pages only the working set (~1-2 G) while Copy reads all 8 G; A/B the two modes here before re-baking.
  Two renumbering bugs found + fixed on the first runs (details in the section below): the link-down flushes the default route (no internet for adopters), and deleting the baked-in address inside the attached ssh black-holes the session (renumbering hung forever). Fix: detached `setsid` renumbering + explicit old-address delete + default-route re-add; `renumber_cmd()` is unit-tested for the single-quote invariant. virtiofs verified post-restore by hand.

- 2026-10-17 **BASELINE (build fcb98e7 = c4a3bdb + config-disk networking), 3 e2e-gate samples**:
  | metric | s1 | s2 | s3 | median |
  |---|---|---|---|---|
  | create (1st VM) | 15.1 | 16.9 | 16.6 | **16.6 s** |
  | parallel (2nd VM, 1st still up) | 16.1 | 14.0 | 15.1 | **15.1 s** |
  | resume A | 16.2 | 15.9 | 14.1 | **15.9 s** |
  | resume B | 16.3 | 15.7 | 15.5 | **15.7 s** |
  Run-to-run variance ±2 s; supervisor consistently ~0.05 s; guest 13.9–16.8 s.
  Guest split (per-boot): kernel 3.9–4.3 s, initrd 2.7–2.8 s, userspace→sshd
  ~2.2 s, pi-vm-net (first boot, incl. grow) 2.1–3.3 s.
- 2026-10-17 **contention finding (gateA)**: a resume while another VM is
  running *and* a third is stopping took **118.7 s** to ssh-ready on this
  4-vCPU host (vs ~15 s solo). Triple load: 2×4 vCPU guests + stop I/O on
  one shared disk. The e2e gate now budgets 120 s per boot (the "budget
  with a boot buffer" semantics); solo numbers are unaffected. Faster guest
  boots (the known-opts below) directly improve the contention case too.
- 2026-10-17 **host-performance drift caveat**: this 4-vCPU host's boot
  times drift ±30% over hours (kernel 4.2→5.5 s, zram 0.5→3.7 s, all
  units scaling together = outer CPU/IO contention, not a code change).
  All before/after comparisons below use same-window (ideally
  interleaved) medians, never across-hour absolutes.
- 2026-10-17 **pre-grow the base to 100 G + guest-side fs sizing (user
  idea, KEPT)**: bake pre-grows the base (resize + short 2nd prep boot
  running the existing grow, verified over ssh); default `--disk` 80→100.
  A create smaller than the baked size does NOT shrink the virtual disk
  (a truncated qcow2 makes the 100 G btrfs unmountable — the initramfs
  hangs on the root UUID; `qemu-img resize --shrink` surfaced this latent
  bug — before it, the failed resize aborted create loudly): the VM keeps
  the base's virtual size (sparse — no storage cost) and pi-vm-net shrinks
  the fs to the requested size (vm-disk-gb on the config disk) while it
  is still mountable. Verified: 40 G create → `df /` exactly 40 GiB; 100 G
  create → no grow work. Interleaved A/B (same window, 3 each): 100 G VM
  old image (grows 5.26→100 G on first boot) 20.6 s vs new image (no
  grow) 18.4 s → **−2.2 s on the default create**. The 80 G base itself
  was neutral (the grow block's btrfs work is ~3 ms — the 108 MB
  partition-start offset always trips the 100 MB grow threshold, on every
  base; only the first grow of a real 5.26 G→X G fs costs seconds).
- 2026-10-17 **250 ms probe cadence (REVERTED — not measurable)**: 3 gate
  samples (create 19.0, parallel 18.3, resume 21.0/18.2) vs the 0.5 s
  build (19.3 / 18.0 / 18.8–19.6) — the 0.25 s quantization saving is
  inside the ±2 s run-to-run noise. Kept the 500 ms cadence.
- 2026-10-17 **systemd graph trim (b) — no candidates**: the bake's
  runcmd already detached every heavy multi-user want (docker, chronyd,
  auditd, sssd, lvm2-monitor, qemu-guest-agent); the remaining on-path
  units (udev, journal, sysctl, tmpfiles, hwdb) are standard and
  required — the critical chain to sshd is basic.target itself. zram +
  lvm2-monitor appear in `blame` but are off the critical path; masking
  anything else would break functionality (udev/journal are needed by
  the pi-vm-net diagnostics).
- 2026-10-17 **pi-vm-net: pidof-guarded NM/resolved disable (c, KEPT —
  not measurable but a strict reduction)**: 3 gate samples (create 20.6,
  parallel 17.5, resume 21.3/19.7) vs the previous build (19.0 / 18.3 /
  21.0/18.7) — inside the host-drift noise (one 74.4 s resume outlier =
  the documented 3-way contention case). Kept because the change only
  removes work (two full `systemctl disable --now` ops → two ~ms `pidof`
  checks; the disable still fires on image drift) — nothing to lose.
- 2026-10-17 **dracut slim initramfs (d, ATTEMPTED — not viable
  tonight)**: the stock initramfs is 49 MB; the 300 cloud kernel has
  virtio-*/btrfs/ext4 BUILT IN, so a module-less initramfs should
  boot. Findings: (1) `--no-kmod` is not a flag in dracut 108; (2)
  `dracut --force img ver` (default) still ships 49 MB (the default
  feature set includes the full driver scan); (3) `-m base` = 11.4 MB
  but drops to the dracut emergency shell (no udev → /dev/vda3 never
  appears → root=UUID not found); (4) `-m base -m systemd-udevd [-m
  systemd]` = 14.0 MB but the initramfs's systemd freezes: "Failed to
  isolate default target: Unit rescue.service not found" (a dracut
  initramfs-systemd integration issue not worth debugging tonight). The
  image is safe (the runcmd dracut is best-effort; the stock initramfs
  persists). Revisit with a custom kernel (then the initramfs module
  set is under our control) or via the snapshot-restore prototype (which
  skips the initramfs entirely). The 2.8 s initrd is partly the
  1.95 s switch-root disk I/O (the host RAM is 7.7 GiB vs 8 G VMs —
  the base never stays in the page cache), which the snapshot idea
  also removes.
- 2026-10-17 **kernel cmdline (a, verified — already tuned)**: the
  direct-boot cmdline is `no_timer_check console=tty1
  systemd.firstboot=off root=UUID=... rootflags=subvol=root console=hvc0`.
  `no_timer_check` (the main KVM win) is already set; `console=tty1` is
  a no-op under CH (no VGA — hvc0 carries everything); remaining
  cmdline options are negligible. **Custom kernel (b, deferred)**: a
  distro kernel build is 60-90 min on this 4-vCPU host for an
  estimated 0.5-1 s gain, and the slim-initramfs wall (above) would
  partially apply to a custom kernel too; deferred behind the
  snapshot-restore work (which skips kernel + initrd entirely).
- 2026-10-17 **SNAPSHOT-RESTORE PROTOTYPE (user idea — VALIDATED, the
  big win)**: on CH 53, `PUT /api/v1/vm.snapshot` works on a running VM
  (no pre-pause needed) and writes a directory (config.json +
  memory-ranges + state.json; 952 M for an 8 G VM). Restore = **bare
  VMM** (`cloud-hypervisor --api-socket ...` only — passing --kernel
  diverts to the VmBoot branch; the doc's bare-VMM variant is the only
  way in 53) + `PUT /api/v1/vm.restore` with `{"source_url":"file:///…",
  "memory_restore_mode":"OnDemand","resume":true}` (case matters).
  Result: **restore 204 → TCP open 0.26 s → ssh OK in the same second**
  (vs 15-20 s cold). Verified: ondemand paging (guest RAM 467 MiB
  resident post-restore); **virtiofs survives the virtiofsd rebind**
  (fresh virtiofsd on the same socket path before the restore is
  enough — `ls /workspace` worked, no guest agent needed for the
  mount); TAP recreated with the snapshot MAC before restore. Caveats:
  config.json bakes the VM-dir paths (disk/config.raw/console/fs.sock —
  the restored VM must run from the same directory); the restored VM
  keeps the template's (IP, MAC) — a pool slot is a fixed (IP, MAC)
  pair; the snapshot is single-use per VM dir (the adopted VM's disk
  diverges → no re-restore, which is fine). **Implemented + e2e-gated
  (commit e23361d)**: bake's final step boots a default-spec template
  to full readiness, quiesces it (`systemctl is-system-running` + 90 s
  idle — a mid-boot snapshot makes the restore re-run the rest of the
  boot: an 8 G ondemand page-in storm that OOM-killed CH on this 7.7 G
  host), pauses it, snapshots it (soft-fail), and leaves it stopped as
  the pool. A default-spec create (4 vCPU / 8 G / 100 G) adopts it:
  fresh CH (bare VMM + `vm.restore`), TAP recreated with the
  snapshot's name from config.json, virtiofsd re-bound on the same
  socket. Same-window gate results: **adopt 9.0–11.7 s to ssh vs cold
  20.9–24.2 s (−9 to −14 s; 3 gate runs: 11.7/9.0/9.7 vs cold median
  22.9)**; the raw restore is sub-second — the
  remainder is the ondemand page-in of the guest's working set + the
  adopter's virtiofsd serving a different project dir. The e2e gate is
  pool-preserving (restores the template meta after the adopter's
  final stop, so consecutive runs re-adopt). Non-default specs fall
  back to the cold path.

## Paired boot profiles — nested box vs the user's bare-metal host (2026-10-17)

`profile-boot.sh` (repo root) emits the host context + per-boot timing +
the guest's systemd breakdown for one cold create and one resume; run it
on both machines and compare **phase shapes**, not absolutes.

**Cold create (user's host, 0.2.0) vs resume cold boot (this box, 0.3.0) — same
boot class (kernel + initrd + userspace all run):**

| phase | user host (7800X3D, 16 c, 61 G, NVMe btrfs, bare metal) | this box (nested, 4 c, 7.7 G) | ratio |
|---|---|---|---|
| supervisor (lock/net+tap/virtiofsd/ch) | 0.00 / **0.52** / 0.02 / 0.00 | 0.00 / 0.04 / 0.02 / 0.00 | — |
| guest (ch-spawn → ssh-ready) | **6.9 s** | 18.2 s (resume, slow window; creates 15.0) | ~2.3× |
| total | **7.5 s** | 18.2 s (slow window; 15.1 typical) | ~2.3× |
| systemd kernel | 0.455 s | **5.557 s** | **12×** |
| systemd initrd | **2.282 s** | 3.371 s | 1.5× |
| systemd userspace (all) | 4.363 s | 11.748 s | 2.7× |
| multi-user.target | 3.263 s | 5.974 s | 1.8× |
| udev device wall (blame top) | ~2.5 s | ~5.8 s | 2.3× |

Shape conclusions, both samples:

- **The guest dominates (6.9 of 7.5 s on the host; ~18 s here)** —
  supervisor-side work is noise on both; there is nothing to win there.
- **The kernel phase is the nested box's defining cost (5.6 s = 27 % of
  the 20.7 s systemd total, 12× the host's 0.455 s)**: kernel + 49 MB
  initramfs load + early init over the nested virtio-blk on a loaded
  4-vCPU host. On the user's host the kernel phase is 6 % — negligible.
  Implication: cold-boot tuning for *this* class of box is disk/CPU-
  bound, not unit-bound; the host's tuning levers (below) transfer,
  but their absolute value is smaller than the kernel phase.
- **On the user's host, the initrd is the single biggest guest item
  (2.28 s = 33 %)** and it is pure overhead for a VM: virtio + btrfs
  are built into the cloud kernel, so the dracut initramfs mostly runs
  udev to discover a device that was never going to be missing
  (`blame`: every device unit appears at ~2.5 s — the udev wall — then
  switch-root). Confirms the slim-initramfs idea (queued, previously
  attempted) as the top cold-boot target on fast hosts; its value is
  ~2 s on the *cold fallback and every resume*, not on the adoption
  path.
- **NEW concrete win (both samples): `systemd-binfmt` sits ON the
  critical chain** — on the host `sysinit.target ← systemd-binfmt
  +1.014s ← … ← multi-user @3.263s`, here `… ← binfmt_misc.automount
  @1.917s`. Masking `systemd-binfmt` (and `systemd-modules-load` if it
  shows up) is a ~1 s cold-boot saving with no functional cost for
  this image (no foreign-architecture binfmt handling in a sandbox).
- Critical-chain tail differs: host goes through `logind` (no serial
  console getty in the chain), this box through
  `getty.target ← serial-getty@hvc0` (5.974 s) — the console getty is
  on the chain here; a `console=hvc0`-driven getty could be masked for
  ~0.2-0.5 s, but the console is the only debug path in these VMs —
  not worth it.
- `docker.service +951ms` (host) starts after basic.target but is
  deliberately not a multi-user requirement — ssh-ready (our metric) is
  unaffected; docker is ~1 s there, ~3-4 s here (consistent with the
  2.7× userspace scale).
- The user's first run showed `net+tap 4.83 s` — that was the **sudo
  password prompt inside the measured window** (0.2.0 has no priming).
  Fixed on main: `util::prime_sudo()` at the start of
  create/resume/bake/install asks once *before* any timing starts (no
  NOPASSWD — the user declined that for security reasons). Real net+tap
  on the host is 0.52 s vs 0.04 s here (the tap/bridge ioctls are not
  free on a busy host, but 10× slower than nested is odd — recheck after
  priming lands).
- Caveats for reading the nested numbers: the outer load on this box
  drifts ±30 % over hours (all units scale together), and the small-RAM
  host is what makes the mid-boot-snapshot OOM mode possible at all — on
  a 61 G host that restore is not a memory hazard, so a couple of this
  box's guardrails (quiesce before snapshot, Copy vs OnDemand choice)
  may be tunable per-host later.
- The user's **resume** profile came back empty: that is the documented
  resume/lock race (0.2.0 takes the flock with no wait and meta flips to
  `stopped` while the old supervisor still holds it → `resume` exits 5),
  already fixed on main by the 120 s lock-wait. `profile-boot.sh` now
  retries the resume so the profile is valid on either build.

## Snapshot master: first end-to-end run + the two renumbering bugs (2026-10-17)

Fresh VM (no state survived), so the whole chain was rebuilt from
scratch on `main`: deps → `cargo install` → `install` → `bake` → gate.

- **The bake's unverified warm step worked** on the first try: "system
  settled — 90 s quiesce" → snapshot 204 → "snapshot master ready" (the
  created-template rule held; §4's created-vs-resumed finding is now
  verified in the automated bake path, not just by hand).
- **Adoption works**: A/B both adopted, reflink disk + hardlinked
  snapshot, `renumbered 172.16.239.114 -> <own ip>`, **9.1–9.5 s** to
  ssh vs 15.0–15.1 s cold in the same window, master untouched after
  several adopters. virtiofs verified post-restore by hand (the gate
  does not cover /workspace).
- **Bug 1 — adopted VMs had no internet.** The renumbering's `ip link set
  dev $IF down` FLUSHES the default route the snapshot carried (the
  kernel drops routes via a downed link). ssh worked (on-link route is
  enough), so only the gate's NAT check caught it. Fix: re-add
  `ip route replace default via <gw>` after the link is up.
- **Bug 2 — the renumbering ssh hung forever.** `ip addr replace` ADDS,
  it does not replace, so the master's baked-in IP stayed configured
  (pinning the connected route's `src` to the stale address, and leaving
  two restored guests able to answer the master's IP). Deleting it is
  correct — but doing it inside the attached ssh deletes that session's
  OWN source address, so the session black-holes and `ssh` hangs
  (ConnectTimeout only bounds connect-setup). Fix: run the renumbering
  DETACHED (`setsid bash -c … >/tmp/pi-vm-renumber.log 2>&1 </dev/null &`)
  and verify success by waiting for the VM's own IP; keepalives
  (ServerAliveInterval=2 ×2) as a backstop. The command builder is now a
  separate `renumber_cmd()` with a unit test asserting the single-quote
  invariant (the inner script is passed inside a single-quoted `bash -c`).

## Idea queue (user + self; test when reached)

- **Pre-provisioned / double-buffered VM disks (user, 2026-10-17)**: keep a
  pre-made disk ready; when a VM starts, generate the *next* VM's disk in
  the background while CH runs — same total work, hidden from
  time-to-ssh-ready. **Measured context: the disk work is only 0.08–0.09 s
  (cp of a sparse qcow2) + 0.03 s (resize) on the current build — not a
  bottleneck yet**, so expected gain ≈ 0.12 s. Revisit if the pre-baked
  image ever becomes dense/large, or if per-VM disk prep grows (e.g.
  per-VM seeding). Implementation sketch: a background worker in the
  supervisor that keeps `home/disk-pool/<slot>.qcow2` warm (cp + resize to
  the next VM's size); create_vm picks the warm slot instead of cp'ing.
- **pi-vm-net first-boot grow (self)**: the 2.1–2.7 s growpart + btrfs
  resize on first boot after a resize is real create-path cost. Option:
  grow the *partition table + fs* at bake time to a default 8 GB (the
  common create size) so typical creates hit the "NOCHANGE" path;
  non-default sizes still grow on boot. Test: bake with an 8 GB root,
  create 8 GB (expect no grow) vs 16 GB (expect grow).
- **Pre-provision fs grow at 100 GB during bake (user, 2026-10-17)**:
  user never uses > 100 GB → bake the base image's root partition +
  btrfs fs at 100 GB; a 100 GB create (the default) then hits the
  "NOCHANGE" grow path on first boot — saves ~2.1–2.7 s of pi-vm-net.
  Mechanism: after the prep phase, host `qemu-img resize` the prep
  disk to 100 G, one short CH boot runs the existing grow (growpart +
  btrfs resize max — both online-safe), the grown disk becomes the
  prepped base. Edge: a create with disk_gb < 100 leaves an oversized
  partition/fs — handle in pi-vm-net: if fs size > disk size, online
  `btrfs filesystem resize <disk-size>` (btrfs supports online shrink
  when usage allows); grows above 100 GB still work as today.
  Also: default `--disk-gb` becomes 100 (check current default).
- **Snapshot-restore "warm boot" (user, 2026-10-17 — from production VPS notes)**:
  the big one. Pre-boot a template VM to ready state (sshd up, userspace
  done), `vm.snapshot` it, and restore per create:
  `--restore "source_url=file:///…/snapshot,memory_restore_mode=ondemand,resume=true"`
  → time-to-ssh ~1–3 s instead of ~15 s. Key points:
  - **`ondemand` (lazy, userfaultfd) is the make-or-break** — `copy`
    (eager) loads all memory up front and may not beat a cold boot.
    Prototype step 1: confirm userfaultfd restore works on this host
    kernel (6.19 — likely yes; test via the CH API on a test VM).
  - **pi-vm adaptation** (no machine-type pools like production): hard-lock
    the DEFAULT cpu/ram (4 vCPU / 8 G) for the snapshot path — a create
    with off-default `--vcpus/--memory` falls back to cold boot (or a
    per-size snapshot matrix later). Disk: the snapshot VM runs on the
    prepped base; a created VM's disk is a copy of that base, so the
    snapshot's page cache stays valid while the disk is unmodified — once
    it diverges (user data) we never re-restore (the VM is already
    running; a stopped diverged VM boots cold).
  - **Config injection moves post-restore** (the user's own note: the
    config-disk read can't happen at normal boot anymore): a small agent
    in the template, whose first action after restore is read
    vm-ip/vm-gw/vm-mac off the still-attached config disk and re-apply
    the NIC config (the snapshot carries the template's IP/MAC). The ssh
    key is the shared baked key — no injection needed (pi-vm's single-key
    design is easier than the customer-key case). Virtiofs is the tricky
    part: the snapshot's fs.sock state is stale — the agent re-mounts
    /workspace (reuse the pi-vm-net mount logic, decoupled).
  - **Template hygiene**: authorized_keys = just the shared key; no
    template IP baked anywhere the agent doesn't overwrite; pi-vm-net
    disabled in the template (replaced by the agent).
  - Prototype order: (1) userfaultfd ondemand restore works + measure
    restore→ssh on a test VM, (2) post-restore agent (config disk +
    virtiofs remount), (3) pi-vm plumbing (snapshot file in the home dir,
    create path branch, cold fallback). Storage: snapshot file ≈ memory
    size (8 G default) — fine.
  - This subsumes most other guest-side wins (kernel/initrd/userspace are
  skipped) — the remaining relevant ones: the supervisor pre-CH phases +
  disk prep (the double-buffered disk idea becomes "next VM's disk +
  snapshot pool").
