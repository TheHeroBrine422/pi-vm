#!/bin/bash
# Clean up an ORPHANED pi-vm: the pi-vm supervisor was killed (e.g. kill -9,
# see hang.log) but its children — cloud-hypervisor, virtiofsd (parent +
# forked daemon), and the ssh session — are still running, and the
# bridge/tap/iptables are still up.
#
# Usage (on the HOST, as the user who runs pi-vm):
#   bash docs/cleanup-orphaned-vm.sh [vm-id]
# Default: 08672294cb35 (the VM orphaned by the kill -9 in hang.log).
#
# WARNING: step 3 kills the ssh session into the orphaned VM — any `pi`
# session inside it ends. The VM disk is preserved and can be resumed
# afterwards (pi-vm self-heals the refcount table if the stop was unclean).
set -u
VM="${1:-08672294cb35}"
DIR="$HOME/.local/share/pi-vm/vms/$VM"
BR="br-${VM:0:8}"
TAP="tap-${VM:0:8}"
SUBNET=$(grep -o '"subnet": *"[^"]*"' "$DIR/meta.json" | cut -d'"' -f4)
IP=$(grep -o '"ip": *"[^"]*"' "$DIR/meta.json" | cut -d'"' -f4)

echo "=== VM $VM ($IP, $SUBNET) ==="
echo "--- processes before:"
ps -ef | grep -E "$VM|root@$IP" | grep -v grep || echo "(none found — already clean?)"
ip -o link show "$BR" 2>/dev/null || echo "(bridge $BR already gone)"

echo
echo "=== 1. graceful guest poweroff (the orphaned CH exits on its own) ==="
timeout 30 ssh -i "$DIR/ssh_key" -o StrictHostKeyChecking=no \
  -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=5 \
  "root@$IP" "systemctl poweroff" \
  && echo "(poweroff requested)" \
  || echo "(poweroff request failed — falling back to SIGKILL)"

echo
echo "=== 2. wait for the CH to exit (up to 90 s) ==="
for _ in $(seq 1 45); do
  pgrep -f "vms/$VM/ch.sock" >/dev/null || break
  sleep 2
done
if pgrep -f "vms/$VM/ch.sock" >/dev/null; then
  echo "   CH still running — SIGKILL (the next resume self-heals the disk)"
  pkill -9 -f "vms/$VM/ch.sock"
  sleep 2
fi

echo
echo "=== 3. kill the leftover virtiofsd (parent + forked daemon) and the ssh session ==="
pkill -f "vms/$VM/fs.sock"
pkill -f "vms/$VM/ssh_key"
sleep 1

echo
echo "=== 4. delete the network (bridge removes the enslaved tap) ==="
sudo ip link del "$BR" 2>/dev/null && echo "   deleted $BR" || echo "   ($BR: nothing to delete)"
sudo ip link del "$TAP" 2>/dev/null && echo "   deleted $TAP" || true
sudo iptables -t nat -D POSTROUTING -s "$SUBNET" -j MASQUERADE 2>/dev/null || true
sudo iptables -D FORWARD -i "$BR" -j ACCEPT 2>/dev/null || true
sudo iptables -D FORWARD -o "$BR" -j ACCEPT 2>/dev/null || true

echo
echo "=== 5. verify ==="
LEFT=$(ps -ef | grep -E "$VM|root@$IP" | grep -v grep)
if [ -n "$LEFT" ]; then
  echo "   STILL RUNNING — investigate:"
  echo "$LEFT"
  exit 1
fi
echo "   no processes left"
ip -o link show "$BR" 2>/dev/null || echo "   bridge $BR gone"
echo "   meta.json self-heals: pi-vm reconciles the stale 'running' state on load"
pi-vm ls 2>/dev/null | grep "$VM" || true
echo
echo "done. The VM can now be resumed:  pi-vm resume $VM"
