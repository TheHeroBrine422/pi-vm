#!/bin/bash
# pi-vm boot profile — one cold create + one resume; collects the host
# context, the per-boot 'boot timing' line, and the guest-side systemd
# breakdown (after a 60 s settle so docker et al. are done). Output: one
# text file, printed at the end.
#
# Usage (on a pi-vm host, with the binary on PATH or $BIN set):
#   ./profile-boot.sh          (wall ~10-12 min: two 240 s VM sessions)
#
# Timing accuracy: run this on a build that calls util::prime_sudo() at
# command start (main >= the renumbering fix), otherwise a stale sudo
# lease puts a password prompt INSIDE the measured net+tap phase (a bare
# `sudo -v` in your shell right before the run works as a workaround).
#
# The VM (named P) is created in a temp project dir and deleted at the end.
# Safe to run while other VMs exist (its own name/id; the master is
# untouched — P is a default-spec VM, so on a pool-baked host the create
# ADOPTS the master snapshot: that is part of what we want to see).
set -u
BIN=${BIN:-$HOME/.cargo/bin/pi-vm}
cd "$(dirname "$0")"
OUT=/tmp/boot-profile-$(date +%Y%m%d-%H%M%S).txt
S=$(mktemp -d /tmp/pi-vm-profile.XXXXXX)
mkdir -p "$S/proj"
sshg() { ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=10 "root@$1" "$2" 2>/dev/null; }
ip_of() { $BIN ls | awk -v n="$1" '$NF == n { print $3; exit }'; }
state_of() { $BIN ls | awk -v n="$1" '$NF == n { print $2; exit }'; }

$BIN rm --force P > /dev/null 2>&1

{
  echo "=== host context ==="
  date -u
  uname -a
  echo "cores: $(nproc)"
  grep -m1 "model name" /proc/cpuinfo
  free -h | head -2
  grep -m1 "hypervisor" /proc/cpuinfo || echo "hypervisor: none (bare metal)"
  for m in kvm_amd kvm_intel; do [ -f /sys/module/$m/nested ] && echo "$m nested: $(cat /sys/module/$m/nested)"; done
  lsblk -f 2>/dev/null | grep -Ev "loop|sr[0-9]" | head -6
  df -hT / | tail -1
  $BIN version 2>/dev/null | head -6
} > "$OUT"

profile_guest() { # $1 = label, $2 = vm name
  local ip; ip=$(ip_of "$2")
  echo "=== guest profile ($1) $(date -u) ===" >> "$OUT"
  sshg "$ip" '
    echo "guest: kernel $(uname -r), cores $(nproc)"
    free -h | head -2
    echo "--- systemd-analyze ---"
    systemd-analyze
    echo "--- critical-chain multi-user ---"
    systemd-analyze critical-chain multi-user.target 2>/dev/null | head -15
    echo "--- critical-chain docker ---"
    systemd-analyze critical-chain docker.service 2>/dev/null | head -15
    echo "--- blame top 30 ---"
    systemd-analyze blame 2>/dev/null | head -30
    echo "--- journal boot markers ---"
    journalctl -b --no-pager 2>/dev/null | grep -E "Linux version|Freeing initrd|Startup finished|Started docker.service|multi-user" | head -10
  ' >> "$OUT" 2>&1
  echo "   profiled $2 at $ip"
}

run_once() { # $1 = label, $2 = create|resume
  local label=$1 kind=$2 log
  log=$S/$label.log
  if [ "$kind" = create ]; then
    nohup $BIN create "$S/proj" --name P --timeout 240 > "$log" 2>&1 &
  else
    # 0.2.0 takes the VM flock with NO wait, and meta flips to "stopped"
    # while the previous supervisor still holds the lock through teardown
    # -> an immediate resume can exit 5 ("in use"). main waits 120 s;
    # retry here so the profile is valid on either build.
    ( for a in 1 2 3 4 5 6; do
        $BIN resume P --timeout 240 >> "$log" 2>&1 && break
        grep -q "in use by another pi-vm process" "$log" || break
        echo "   resume attempt $a hit the in-flight stop's lock, retrying ..."
        sleep 15
      done ) &
  fi
  local i
  for i in $(seq 1 180); do grep -q "boot timing" "$log" 2>/dev/null && break; sleep 1; done
  if grep -q "boot timing" "$log"; then grep "boot timing" "$log" >> "$OUT"; else
    echo "   WARN: no boot timing line ($label) — supervisor log tail:" >> "$OUT"
    tail -5 "$log" >> "$OUT"
  fi
  grep -E "master adoption|snapshot restore|renumbered|booting" "$log" >> "$OUT"
  echo "   settle 60 s (let docker et al. finish) ..."
  sleep 60
  if [ -n "$(ip_of P)" ] && [ "$(state_of P)" = "running" ]; then
    profile_guest "$label" P
  else
    echo "   SKIPPED guest profile ($label): not running (state=$(state_of P))" >> "$OUT"
  fi
  for i in $(seq 1 300); do [ "$(state_of P)" = "stopped" ] && break; sleep 2; done
  [ "$(state_of P)" = "stopped" ] || echo "   WARN: P not stopped after $label" >> "$OUT"
}

run_once cold-create create
run_once resume resume

for a in 1 2 3 4 5 6; do
  $BIN rm --force P > /dev/null 2>&1 && break
  # the meta flips to "stopped" while the previous supervisor still
  # holds the flock through teardown -> retry until the lock frees
  echo "   P not deletable yet (in-flight stop), retry $a ..."
  sleep 15
done
rm -rf "$S"
echo "profile: $OUT"
