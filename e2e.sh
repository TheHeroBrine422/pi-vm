#!/bin/bash
# pi-vm e2e gate — one full functional cycle, unattended. Exit 0 = green.
#
#   1. create A (timeout 40 s)             -> ssh probe, pty attach, internet (NAT)
#   2. create B (parallel, timeout 40 s)   -> ssh probe; isolation A->B (expect
#      block), A->internet (expect OK)
#   3. resume A (timeout 30 s)             -> ssh probe, pty attach, internet
#   4. resume B (timeout 30 s)             -> ssh probe; isolation B->A (expect block)
#   5. rm --force A + B                   -> list must be empty
#   6. cargo test                         -> 0 failures
#
# Every 'boot timing' line is echoed for the timing sampler. Wall ~6-8 min.
set -u
BIN=${BIN:-/root/.cargo/bin/pi-vm}
cd "$(dirname "$0")"
S=$(mktemp -d /tmp/pi-vm-e2e.XXXXXX)
fail() { echo "GATE-FAIL: $*"; for n in A B C; do $BIN rm --force "$n" > /dev/null 2>&1; done; exit 1; }

ip_of() { $BIN ls | awk -v n="$1" '$NF == n { print $3; exit }'; }
state_of() { $BIN ls | awk -v n="$1" '$NF == n { print $2; exit }'; }
sshg() { ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=10 "root@$1" "$2" 2>/dev/null; }
wait_timing() { local f=$1 t=${2:-240} i=0; while [ $i -lt $t ]; do grep -q "boot timing" "$f" 2>/dev/null && return 0; sleep 3; i=$((i + 3)); done; return 1; }
save_console() { # $1 = VM name — preserve its console before fail()'s cleanup
  local id; id=$($BIN ls | awk -v n="$1" '$NF == n { print $1; exit }')
  [ -n "$id" ] && [ -f "/root/.local/share/pi-vm/vms/$id/console.log" ] && cp "/root/.local/share/pi-vm/vms/$id/console.log" "$S/$1-console.log"
}
wait_state() { local id=$1 want=$2 t=${3:-300} i=0; while [ $i -lt $t ]; do [ "$(state_of "$id")" = "$want" ] && return 0; sleep 3; i=$((i + 3)); done; return 1; }
pty() { timeout 20 ssh -tt -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes "root@$1" "echo $2" </dev/null 2>/dev/null | grep -q "$2"; }

# --- 1. create A (timeout 120 s: contention budget — on a 4-vCPU host a
# boot can take ~2 min when another VM is up + stopping at the same time)
# Pre-clean any leftovers from a previously interrupted/failed run.
$BIN rm --force A > /dev/null 2>&1
$BIN rm --force B > /dev/null 2>&1
$BIN rm --force C > /dev/null 2>&1
mkdir -p "$S/a" "$S/b" "$S/c"
nohup $BIN create "$S/a" --name A --timeout 120 > "$S/a.log" 2>&1 &
echo "master before A: $( [ -f /root/.local/share/pi-vm/vms/dead00000001/meta.json ] && [ -f /root/.local/share/pi-vm/snapshots/default/state.json ] && echo present || echo MISSING )"
wait_timing "$S/a.log" || { save_console A; fail "A create: $(grep -E 'error' "$S/a.log" | head -1)"; }
grep "boot timing" "$S/a.log"
IPA=$(ip_of A); [ -n "$IPA" ] || fail "A has no IP"
[ "$(state_of A)" = "running" ] || fail "A not running after ready (state=$(state_of A))"
[ "$(sshg "$IPA" 'echo P1')" = "P1" ] || fail "A ssh probe"
pty "$IPA" P1-PTY || fail "A pty attach"
sshg "$IPA" 'curl -sI -m 8 https://example.com | head -1' | grep -q "HTTP/2 200" || fail "A internet (NAT)"

# --- 2. create B (parallel) + isolation ---
nohup $BIN create "$S/b" --name B --timeout 120 > "$S/b.log" 2>&1 &
wait_timing "$S/b.log" || { save_console B; fail "B create: $(grep -E 'error' "$S/b.log" | head -1)"; }
grep "boot timing" "$S/b.log"
IPB=$(ip_of B); [ -n "$IPB" ] || fail "B has no IP"
[ "$IPA" != "$IPB" ] || fail "A and B got the same IP"
[ "$(sshg "$IPB" 'echo P2')" = "P2" ] || fail "B ssh probe"
sshg "$IPA" "ping -c3 -W2 $IPB" | grep -q "0 received" || fail "isolation: A reached B"
sshg "$IPA" 'curl -sI -m 8 https://example.com | head -1' | grep -q "HTTP/2 200" || fail "isolation window: A internet"

# --- 3. resume A (after its 40 s stop) ---
wait_state A stopped || fail "A did not stop"
nohup $BIN resume A --timeout 120 > "$S/a2.log" 2>&1 &
wait_timing "$S/a2.log" || { save_console A; fail "A resume: $(grep -E 'error' "$S/a2.log" | head -1)"; }
grep "boot timing" "$S/a2.log"
[ "$(sshg "$IPA" 'echo P3')" = "P3" ] || fail "A resume ssh"
pty "$IPA" P3-PTY || fail "A resume pty"
sshg "$IPA" 'curl -sI -m 8 https://example.com | head -1' | grep -q "HTTP/2 200" || fail "A resume internet"

# --- 4. resume B ---
wait_state B stopped || fail "B did not stop"
nohup $BIN resume B --timeout 120 > "$S/b2.log" 2>&1 &
wait_timing "$S/b2.log" || { save_console B; fail "B resume: $(grep -E 'error' "$S/b2.log" | head -1)"; }
grep "boot timing" "$S/b2.log"
[ "$(sshg "$IPB" 'echo P4')" = "P4" ] || fail "B resume ssh"
sshg "$IPB" "ping -c3 -W2 $IPA" | grep -q "0 received" || fail "isolation(resume): B reached A"

# --- 4b. create C --disk 40: the shrink path (the baked fs is pre-grown to
# 100 G; a 40 G VM must have its fs shrunk to fit by pi-vm-net) ---
nohup $BIN create "$S/c" --name C --disk 40 --timeout 120 > "$S/c.log" 2>&1 &
wait_timing "$S/c.log" || { save_console C; fail "C create: $(grep -E 'error' "$S/c.log" | head -1)"; }
grep "boot timing" "$S/c.log"
IPC=$(ip_of C); [ -n "$IPC" ] || fail "C has no IP"
FS=$(sshg "$IPC" 'df -B1 --output=size / | awk "NR==2{print \$1}"')
[ -n "$FS" ] && [ "$FS" -lt 100000000000 ] 2>/dev/null || fail "C fs not shrunk to 40 G (got ${FS:-?} B)"
[ "$FS" -ge 38000000000 ] 2>/dev/null || fail "C fs shrunk too far (${FS} B < 38 G)"

# --- 5. rm all (the master is never named A/B/C — the reusable master
# stays in place for the next run) ---
$BIN rm --force A > "$S/rmA.log" 2>&1 || fail "rm A: $(grep -E 'error' "$S/rmA.log" | head -1)"
$BIN rm --force B > "$S/rmB.log" 2>&1 || fail "rm B: $(grep -E 'error' "$S/rmB.log" | head -1)"
$BIN rm --force C > "$S/rmC.log" 2>&1 || fail "rm C: $(grep -E 'error' "$S/rmC.log" | head -1)"
sleep 2
for n in A B C; do $BIN ls | awk -v n="$n" '$NF == n { found = 1 } END { exit found ? 0 : 1 }' && fail "VM $n remains after rm"; done

# --- 6. cargo test ---
cargo test > "$S/test.log" 2>&1 || fail "cargo test failed: $(grep 'test result' "$S/test.log" | head -1)"
grep -q " 0 failed" "$S/test.log" || fail "cargo test failures: $(grep 'test result' "$S/test.log" | head -1)"

echo "GATE-OK (workdir $S)"
exit 0
