# Boot improvements — living log

Goal: drive time-to-ssh-ready (create + resume) down, every experiment gated by
`e2e.sh` (exit 0) + 3-sample medians, measurable improvements committed
immediately.

## Current baseline (build `fcb98e7` = config-disk networking + virtiofsd poll +
0.5 s probe + docker detached from multi-user)

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
