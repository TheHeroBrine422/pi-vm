# HANDOFF — pi-vm, 2026-10-17 evening

You are picking up an autonomous boot-optimization + snapshot-restore effort on
`pi-vm` (Rust, repo at `/workspace`, binary at `/root/.cargo/bin/pi-vm`,
state at `/root/.local/share/pi-vm/`). Read this top-to-bottom once, then
start at **§5 Next actions**.

## 1. Where things stand

**v0.2.0 is CUT and GATE-VERIFIED** (tag `v0.2.0` at `410232c` on branch
`release/0.2.0`; notes in `RELEASE-NOTES.md`, committed there). It contains
everything from the overnight run *except* snapshot-restore: config-disk
networking (static pool IPs, no DHCP/NoCloud), `bake --ssh-key` + bake
verification, pre-grown 100 G base + guest-side fs sizing (default
`--disk 100`), per-phase boot-timing line, `ls --filter/--sort/--reverse`,
autoresearch/dnsmasq removals, `pi-vm version` now prints the commit
(`0.2.0+<sha>`). The user's upgrade path is in `RELEASE-NOTES.md`
(back up data in pre-release VMs — they fast-fail by design → `pi-vm
install` → `pi-vm bake` → recreate VMs). The user is about to restart the
harness with the 0.2.0 binary.

**0.3.0 = the reusable snapshot master**, committed as a marked WIP on
`main` (the WIP commit message says "UNVERIFIED, 0.3.0 candidate"). That is
your job to finish.

## 2. Git state

- `main`: WIP commit (reusable master: per-VM reflink disk + hardlinked
  per-VM snapshot + post-restore ssh renumbering + Copy restore mode +
  flock serialization + resume lock-wait + SIGINT-tty fix). `git log
  --oneline -5` on main shows it.
- `release/0.2.0`: `410232c` (code, gate-green) + `86d59d8` (notes).
- Tags: `v0.2.0` (the release), `v0.1.0` (obsolete marker at `e23361d`,
  the single-use-pool state — ignore for the release).
- `BOOT-IMPROVEMENTS.md` = the living experiment log (baseline medians,
  A/B results, the OOM mode, the created-vs-resumed finding is noted in
  §4 below — update it as you go).

## 3. Live state on this box

- Installed binary: **0.2.0+410232c** (the release). For 0.3.0 work,
  `git checkout main && cargo install --path .` first.
- Base image: pre-grown 100 G (`fd2c6e7b…`), matches the 0.2.0 bake.
- **The snapshot master is READY**: `vms/dead00000001` (meta
  `warm-default`, stopped, ip 172.16.17.155, mac 52:54:00:f7:92:19) +
  `snapshots/default` (config/state patched, memory-ranges pre-warmed).
  It was built with `/tmp/poolbuild6.sh` (a **created** template — see §4).
  Do not `rm --force warm-default` unless you plan to rebuild it.
- The 0.2.0 e2e gate is what's in `release/0.2.0` (cold-only). **`main`'s
  `e2e.sh` is the WIP gate** (master-presence check, plain cleanup — the
  master is never named A/B/C so cleanup can't delete it).
- `network.conf` pool = 172.16.17.0/24 (gw .1). Legacy VMs are gone.

## 4. What is known about snapshot-restore (the hard-won facts)

**CH 53 mechanics (validated):**
- Snapshot: `PUT /api/v1/vm.snapshot` works on a RUNNING VM (a pause
  attempt first; its result is ignored). Destination dir must **pre-exist**
  and be fresh (stale files → "File exists"). Writes config.json +
  memory-ranges (sparse: 8 GiB virtual / ~0.5–1 G real) + state.json.
- Restore: **bare VMM only** (`--api-socket path=…` — passing `--kernel`
  diverts to the boot branch) + `PUT /api/v1/vm.restore`
  `{"source_url":"file:///…","memory_restore_mode":"…","resume":true}`.
  Mode enum is **`Copy` or `OnDemand`** (nothing else; "Eager" is 400).
  The 204 returns quickly; treat restore as async — verify the VM is
  actually alive (tcp probe) before declaring success.
- config.json/state.json bake the VM-dir paths (disk, config.raw,
  console.log, fs.sock) + the tap name; the restored VM must run with
  those paths satisfied (a fresh virtiofsd on the same fs.sock path is
  enough — virtiofs survives the rebind, no guest agent needed).
- config.json's `fs` socket is a plain string (not an object) — the
  restore 500 "invalid type: null, expected path string" came from
  patching it wrong.

**A/B results (same window, same master):** OnDemand cold-file: 204 → tcp
0.07 s → ssh 2.71 s. OnDemand + pre-warm dd: 3.56 s (the dd *competes* —
don't pre-warm per VM; warm once at pool build). **Copy + pre-warm:
1.45 s to ssh** — Copy won.

**The critical finding (why the last ~2 days went slow):** snapshots of
**resumed** templates deadlock the restore — 204 returns, vCPUs run
(KVM_RUN) but on **zeroed memory** (RSS flat ~571 M, CH never opens
memory-ranges, no error in the log; guest L3 dead). Snapshots of
**created** templates work (R3 1.45 s; single-use-pool gate runs N5/N7/N8:
adopt 9.0–11.7 s vs cold 20.9–24.2 s same window). **Pool templates must
be created, not resumed.** (The WIP's bake warm step + poolbuild6 both
use a created template.)

**Other failure modes found (all documented in the code comments/gate):**
- Mid-boot snapshot (ssh-ready fires before systemd settles) → restore
  re-runs the rest of the boot → 8 G page-in storm → **host OOM kills CH**
  (7.7 G host). Fix: quiesce (`systemctl is-system-running` →
  running/degraded, then 90 s idle) before the pause.
- CH 53's qcow2 parser **rejects backing chains**
  (MaxNestingDepthExceeded even for a single backing) — per-VM disks must
  be full copies; `cp --reflink=auto` is ~0.08 s on this btrfs host
  (falls back to a plain copy elsewhere).
- The single-use pool's gate cycle silently deleted the master (the
  adopter's meta named "A" lived in the warm dir → next run's `rm A`
  deleted it). The WIP's reusable master fixes this structurally (the
  master's meta is never touched by adopters).
- The WIP's boot() waits for an in-flight stop's flock up to 120 s for
  Stopped metas (resume race: meta says stopped while the previous stop
  still holds the lock → EAGAIN). SIGINT is only honored when stdin is a
  tty (stray group signals used to tear down background creates).

## 5. Next actions (in order)

1. `git checkout main && cargo build && cargo test && cargo install
   --path .` (the WIP binary).
2. Run `setsid nohup bash /workspace/e2e.sh > /tmp/gateN16.log 2>&1 </dev/null
   &` and poll. Expect: `master before A: present`, A + B both "master
   adoption" (serialized by `master-adopt.lock`), "renumbered X -> Y"
   lines, C cold (40 G fallback), GATE-OK. **This is the first end-to-end
   run of the reusable master** — the pieces are validated individually
   (created-template snapshots restore in 1.45 s; reflink/hardlink per-VM
   files are instant; the renumbering command is simple) but the composed
   path has not been green.
3. If green: 3 gate samples (same-window medians: adopt vs cold), then
   `git commit --amend` is NOT wanted — make a clean commit of the WIP
   (reword/replace the WIP commit message to the real feature message),
   update `BOOT-IMPROVEMENTS.md` + `README.md` (the pool sections still
   describe the single-use pool — rewrite for: reusable master, per-VM
   reflink disk, post-restore renumbering, concurrent adopters serialized,
   bake builds the quiesced created-template master), and cut the 0.3.0
   tag when the user says so.
4. If red: the saved console + per-run logs are in the gate workdir
   (`/tmp/pi-vm-e2e.*/`); `save_console` preserves the console on failure.
   Most likely suspects, in order: (a) the renumbering ssh (interface
   detection via the pre-IP, the mac down/up sequence) — test it manually
   with a bare restore (see §6 recipe); (b) the state.json tap-name patch
   (state.json may not contain the tap name at all — the N15-era manual
   tests patched config+state with the master tap name and it still
   worked, so the tap patch is probably a no-op there — verify what
   state.json actually contains for the net device before "fixing" it);
   (c) a restored-guest quirk specific to this snapshot (check the
   master's console.log after a manual restore).
5. The bake's warm step (WIP `bake.rs`) is updated (quiesce + 240 s
   budget + pre-warm) but **unverified** — after the gate work is green,
   run a full `pi-vm bake` once and confirm the pool comes up
   ("system settled — 90 s quiesce" + "snapshot master ready" lines).

## 6. Manual test recipe (bare restore of the current master)

```
V=/root/.local/share/pi-vm/vms; S=/root/.local/share/pi-vm/snapshots/default
# master's tap (name from the meta: tap-<master id's first 8 chars>)
ip link del tap-dead0000 2>/dev/null; ip tuntap add dev tap-dead0000 mode tap
ip link set dev tap-dead0000 master br-pi-vm; ip link set dev tap-dead0000 up
rm -f $V/dead00000001/fs.sock
/usr/libexec/virtiofsd --socket-path=$V/dead00000001/fs.sock \
  --shared-dir=/tmp/warm-proj --cache=auto \
  --translate-uid=squash-guest:0:0:65536 --translate-gid=squash-guest:0:0:65536 &
rm -f /tmp/t.sock
/root/.local/share/pi-vm/assets/cloud-hypervisor --api-socket path=/tmp/t.sock &
sleep 2
curl -s -m 120 -w "%{http_code}\n" --unix-socket /tmp/t.sock -X PUT \
  http://localhost/api/v1/vm.restore -H 'Content-Type: application/json' \
  -d "{\"source_url\":\"file://$S\",\"memory_restore_mode\":\"Copy\",\"resume\":true}"
# then: timeout 3 bash -c "echo > /dev/tcp/172.16.17.155/22"  (master IP)
# and ssh root@172.16.17.155. Kill the CH + virtiofsd + delete the tap after.
```
(virtiofsd is `/usr/libexec/virtiofsd` — it is NOT on PATH and NOT in
`assets/`.)

## 7. Environment quirks (cost real time before)

- **pkill/pgrep self-match**: a pattern string that also appears later in
  the same shell command (e.g. the virtiofsd launch text) kills your own
  shell (exit 143). Use bracket tricks (`pkill -f "foo.soc[k]"`) AND keep
  cleanup in a separate call from any command containing the plain text.
- **`/tmp` is a 3.9 G tmpfs** — 2.3 G disk copies + 8 G snapshot copies
  fill it ("No space left"). Put test VM dirs under
  `/root/.local/share/pi-vm/vms/` (btrfs, reflink works there).
- **Background supervisors**: `setsid nohup … &` survives tool-call
  interruptions; a plain `(nohup … &)` child of your tool's shell dies
  with it (pdeathsig → CH torn down → "CH exited (signalled)" with an
  otherwise-healthy guest).
- **Long commands**: launch in the background with a log, poll separately.
- **Host drift**: outer CPU/IO load varies ±30 % over hours; ALL timing
  comparisons must be same-window (the gate does this: A/B/C in one run).
- **Guest poweroff is slow** (~2–4 min, dockerd); "guest did not power
  off in ~2 min — SIGTERM" is normal.
- The gate's budgets are 120 s per boot (a resume under 3-way load once
  took 118.7 s).
- `pi-vm` is NOT on PATH: `/root/.cargo/bin/pi-vm`. After code changes:
  `cargo install --path .` (a stale installed binary bit the e2e once).
- The `version` subcommand (not `--version`) prints the full state (CH
  version + image shas).

## 8. Baseline / reference numbers

- 0.2.0 gate (this box, clean window): cold create **14.8–15.3 s**,
  resume same.
- Baseline medians (build `fcb98e7`, its own window): create 16.6 /
  parallel 15.1 / resume 15.9 s.
- Single-use pool (N5/N7/N8, its windows): adopt **9.0–11.7 s** (median
  9.7) vs cold 20.9–24.2 (median 22.9).
- Raw restore A/B: OnDemand 2.71 s / **Copy 1.45 s** to ssh (warm file).
- Expected 0.3.0 adopt: ~3–6 s to ssh (Copy restore + renumbering window),
  vs cold ~15–23 s.
- Supervisor overhead: ~0.05 s (lock 0.00, net+tap 0.03, virtiofsd 0.02,
  ch-spawn 0.00). Disk prep: reflink copy ~0.08 s (was 0.09 s plain cp
  + 0.03 s resize).
