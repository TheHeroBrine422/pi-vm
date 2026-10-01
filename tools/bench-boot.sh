#!/usr/bin/env bash
# bench-boot.sh — boot-time benchmark for pi-vm (direct kernel boot).
#
# usage: tools/bench-boot.sh [--runs N] [--warmup] [--vcpus N] [--memory MB]
#                            [--disk GB] [--pi-vm PATH]
#
# Boots N fresh --no-mount VMs (default 5) and measures, with a bounded
# 0.25 s-cadence TCP probe (250 ms resolution):
#   - first_console  : first guest content in the console log (~2.7 s in —
#                     CH's console is a virtio-console, probed after PCI
#                     enumeration; the kernel banner itself predates it)
#   - login prompt   : first "login:" (the hvc0 getty; appears a few
#                     seconds after sshd, once cloud-init finishes)
#   - ssh ready      : first successful TCP connect to the VM's :22
# All times are wall seconds from the `pi-vm create` process start.
#
# Every VM is stopped gracefully (SIGINT -> guest poweroff, up to 120 s)
# and deleted afterwards. Run it from a checkout with the pi-vm binary, or
# pass --pi-vm / set $PIVM.

set -u

VMHOME="${PI_VM_HOME:-$HOME/.local/share/pi-vm}"
PIVM="${PIVM:-$(command -v pi-vm || true)}"
[ -z "$PIVM" ] && PIVM="./target/release/pi-vm"
RUNS=5
WARMUP=0
VCPUS=2
MEM=4096
DISK=80

while [ $# -gt 0 ]; do
  case "$1" in
    --runs) RUNS=$2; shift 2 ;;
    --warmup) WARMUP=1; shift ;;
    --vcpus) VCPUS=$2; shift 2 ;;
    --memory) MEM=$2; shift 2 ;;
    --disk) DISK=$2; shift 2 ;;
    --pi-vm) PIVM=$2; shift 2 ;;
    -h|--help) grep '^# ' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

A="$VMHOME/assets"
S="$(mktemp -d)"
trap 'rm -rf "$S"' EXIT

# --- preflight ---
[ -x "$PIVM" ] || { echo "error: pi-vm binary not found: $PIVM (pass --pi-vm)" >&2; exit 2; }
[ -f "$VMHOME/images/fedora44-cloud-prepped.qcow2" ] || { echo "error: pre-baked base image missing — run \`pi-vm install\` / \`pi-vm bake\` first" >&2; exit 2; }
if [ ! -f "$A/kernel" ] || [ ! -f "$A/initramfs" ] || [ ! -f "$A/kernel-cmdline" ]; then
  echo "error: direct-boot assets missing in $A — run \`pi-vm bake\` to extract them" >&2
  exit 2
fi
[ -e /dev/kvm ] || echo "WARN: /dev/kvm missing — CH requires KVM" >&2

now() { date +%s.%N; }
el() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.1f", a-b}'; }

# --- one benchmark run: boot a fresh VM, time it, stop it, delete it ---
bench_one() {
  local start pid ip="" clog="" vmid="" t_first="" t_login="" t_ssh="" t_ssh_at="" cpos=0
  start=$(now)
  # stdin from /dev/null: forces pi-vm's non-attach mode (attach =
  # stdin.is_terminal()); a backgrounded attach would fight us for the
  # terminal and break the SIGINT stop path.
  "$PIVM" create --no-mount --vcpus "$VCPUS" --memory "$MEM" --disk "$DISK" \
    < /dev/null > "$S/run.log" 2>&1 &
  pid=$!

  # find the new VM dir (created before CH spawns) -> IP + console path.
  # `known` must be space-separated: the case pattern below matches on
  # " <id> " — a newline-separated list only matches its last entry.
  local known
  known=$(ls "$VMHOME/vms" 2>/dev/null | sort | tr '\n' ' ')
  for i in $(seq 1 120); do
    sleep 0.5
    kill -0 "$pid" 2>/dev/null || break
    for d in "$VMHOME/vms"/*/; do
      [ -f "$d/meta.json" ] || continue
      local id; id=$(basename "$d")
      case " $known " in *" $id "*) ;; *)
        vmid=$id
        ip=$(grep -oE '"ip"[[:space:]]*:[[:space:]]*"[^"]+"' "$d/meta.json" | grep -oE '172\.16\.[0-9]+\.[0-9]+' | head -1)
        clog="$d/console.log"
        break 2
      ;; esac
    done
  done
  if [ -z "$vmid" ]; then
    echo "error: no VM dir appeared (see $S/run.log)" >&2
    kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    return 1
  fi

  # poll: bounded sshd probe + console phase markers. After ssh-ready, keep
  # polling up to 20 s more for the login prompt (it lands a few seconds
  # after sshd, once cloud-init finishes). Bounded overall: a VM that never
  # becomes ssh-reachable fails the run instead of hanging forever.
  local poll_deadline; poll_deadline=$(( $(date +%s) + 300 ))
  while true; do
    kill -0 "$pid" 2>/dev/null || break
    [ "$(date +%s)" -ge "$poll_deadline" ] && { echo "   (poll timeout — VM never became ssh-reachable)" >&2; break; }
    local nowt; nowt=$(now)
    if [ -z "$t_ssh" ]; then
      timeout 1 bash -c "echo > /dev/tcp/$ip/22" 2>/dev/null && t_ssh=$nowt
    fi
    if [ -n "$clog" ] && [ -f "$clog" ]; then
      local sz; sz=$(stat -c %s "$clog" 2>/dev/null || echo 0)
      if [ "$sz" -gt "$cpos" ]; then
        # CH's own log lines (stdout+stderr go to the console file) are
        # not guest output — filter them out of the phase markers
        local new; new=$(tail -c +$((cpos + 1)) "$clog" | grep -v '^cloud-hypervisor:')
        cpos=$sz
        if [ -n "$new" ]; then
          [ -z "$t_first" ] && t_first=$nowt
          if [ -z "$t_login" ] && printf '%s' "$new" | grep -q "login:"; then t_login=$nowt; fi
        fi
      fi
    fi
    if [ -n "$t_ssh" ]; then
      [ -z "$t_ssh_at" ] && t_ssh_at=$nowt
      [ -n "$t_login" ] && break
      el "$nowt" "$t_ssh_at" | awk '{ if ($1 > 20) exit 0; else exit 1 }' && break
    fi
    sleep 0.25
  done

  # graceful stop (SIGINT -> guest poweroff, up to ~130 s worst case)
  kill -INT "$pid" 2>/dev/null
  for i in $(seq 1 150); do
    sleep 1
    kill -0 "$pid" 2>/dev/null || break
  done
  kill -0 "$pid" 2>/dev/null && { echo "   (stop timed out — SIGKILL)" >&2; kill -9 "$pid"; }
  wait "$pid" 2>/dev/null
  "$PIVM" rm "$vmid" --force >/dev/null 2>&1 || echo "   WARN: rm $vmid failed — delete it manually" >&2

  # a run that never reached ssh is a failure, not a data-less success
  [ -n "$t_ssh" ] || return 1

  printf '%s|%s|%s|%s\n' \
    "${t_first:+$(el "$t_first" "$start")}" \
    "${t_login:+$(el "$t_login" "$start")}" \
    "${t_ssh:+$(el "$t_ssh" "$start")}" \
    "$vmid"
}

# --- run the matrix ---
declare -a RESULTS
if [ "$WARMUP" -eq 1 ]; then
  echo "==> warmup run (untimed — warms the base image's page cache)"
  bench_one >/dev/null 2>&1 || echo "   (warmup run failed — continuing)"
fi
echo "==> $RUNS run(s) (fresh $VCPUS vcpu / $MEM MB VM, --no-mount)"
i=1
while [ "$i" -le "$RUNS" ]; do
  printf '   run %s ... ' "$i"
  r=$(bench_one) || { echo "FAILED"; i=$((i+1)); continue; }
  echo "ok"
  RESULTS+=("$r")
  i=$((i+1))
done

# --- summary ---
stat3() { # $1=label, rest=values; prints min / median / avg / stddev / max
  local label="$1"; shift
  if [ "$#" -eq 0 ]; then printf '%-14s %s\n' "$label" "(no data)"; return; fi
  printf '%s\n' "$@" | sort -n | awk -v label="$label" '
    {v[++n]=$1; s+=$1; s2+=$1*$1}
    END{
      med = (n%2) ? v[(n+1)/2] : (v[n/2]+v[n/2+1])/2
      mean = s/n
      var = (n>1) ? (s2 - s*s/n)/(n-1) : 0
      if (var<0) var=0
      sd = sqrt(var)
      printf "%-14s min %6.1f s   med %6.1f s   avg %6.1f s   sd %5.2f s   max %6.1f s   (n=%d)\n", label, v[1], med, mean, sd, v[n], n
    }'
}
echo
printf '%-8s %-14s %-13s %s\n' "run" "first_console" "login_prompt" "ssh_ready"
for entry in "${RESULTS[@]}"; do
  IFS='|' read -r b l s v <<< "$entry"
  printf '%-8s %-14s %-13s %s\n' "$v" "${b:-—}" "${l:-—}" "${s:-—}"
done
echo
echo "==> summary (wall seconds from create start)"
stat3 "first_console" $(for entry in "${RESULTS[@]}"; do IFS='|' read -r b l s v <<< "$entry"; [ -n "$b" ] && printf '%s ' "$b"; done)
stat3 "login_prompt"  $(for entry in "${RESULTS[@]}"; do IFS='|' read -r b l s v <<< "$entry"; [ -n "$l" ] && printf '%s ' "$l"; done)
stat3 "ssh_ready"     $(for entry in "${RESULTS[@]}"; do IFS='|' read -r b l s v <<< "$entry"; [ -n "$s" ] && printf '%s ' "$s"; done)
echo
echo "first_console: first guest content in the console log (~2.7 s in — the"
echo "virtio-console device is probed after PCI enumeration). login_prompt:"
echo "the hvc0 getty prompt. ssh_ready: first sshd accept."
