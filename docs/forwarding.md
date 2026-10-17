# Reaching private/remote services from a VM

A VM's network is a private bridge with NAT: it reaches the public
internet, but **not** private networks — a Tailscale tailnet, a LAN, or a
colocation server whose service ports are closed. To give a VM access to
one specific private service **without exposing it to the whole private
network**, put a **destination-hardcoded forwarder** on a machine that
*can* reach the service (your host, a relay, the VM itself).

## The pattern (and why it's safe)

1. **Hardcode the destination.** Use a TCP forwarder with a fixed target
   (`socat ... TCP:<dest-ip>:<dest-port>`), never a general proxy
   (SOCKS). The VM can only reach that one endpoint — it can't steer the
   forwarder to other private IPs.
2. **Bind to the VM-facing interface only.** `bind=<bridge-ip>` (the
   pi-vm bridge IP is the gateway of the host's chosen VM `/24` — see
   `network.conf` / the `pi-vm install` output), not `0.0.0.0` or a
   public interface — so the forwarder isn't internet-exposed.
3. **Preserve the hostname (SNI / `Host`).** For TLS services that check
   the SNI or `Host` header, add an `/etc/hosts` entry in the VM mapping
   the real hostname to the forwarder's IP, so the client uses the
   correct name. (For plain HTTP, or a server that ignores `Host`, you
   can point the config at the IP:port directly.)
4. **Firewall.** If the forwarder's port is blocked on the relay
   machine, allow it **only from the VM bridge network** (the host's
   chosen `/24` — a narrow rule; all pi-vm VMs share one bridge, so the
   rule covers every VM), or pick a port that's already reachable from
   the VM.

## Recipe 1 — private-network service (Tailscale / LAN)

A machine on the private network runs a forwarder bound to the interface
facing the VM:

```
sudo socat TCP-LISTEN:443,bind=<bridge-ip>,fork,reuseaddr TCP:100.98.228.35:443
```

Then in the VM, map the hostname to the relay's bridge IP:

```
echo "<bridge-ip> example.ts.net" >> /etc/hosts
```

Point your config at the real hostname (`https://example.ts.net/v1`).
The VM connects to the relay's bridge IP; the relay forwards to the
private IP. SNI + `Host` stay correct, and the VM can reach nothing else
on the tailnet.

## Narrow firewall allow (if the port is blocked)

If the relay machine's firewall rejects the forwarder's port (the VM sees
`No route to host` — an ICMP reject — while other ports give
`Connection refused`), allow the port **only from the VM bridge network**
(the host's chosen `/24`). Scope it to the forwarder's port only, never
all traffic.

**The VM bridge (all pi-vm VMs share one):**

```
sudo firewall-cmd --permanent --add-rich-rule='rule family=ipv4 source address=<vm-net/24> port port=443 protocol=tcp accept'
sudo firewall-cmd --reload
```
