#!/bin/sh
# Test the rules `ingot net` applies, in throwaway network namespaces; the
# host's own network is never touched. Needs root, nft and a built ingot.
# CI runs it; elsewhere:
#
#   cargo build -p ingot && sudo sh tests/copper-net.sh
#
#   cn-vm1  lab   10.42.1.10/32 ─┐
#   cn-vm2  home  10.42.1.11/32 ─┼─ cu-42-1 (isolated ports) ── cn-node ── lan0 192.168.68.109
#   cn-vm3  lab   10.42.1.12/32 ─┘  10.42.1.1                                    │
#   cn-lan  192.168.68.1 (router)  192.168.68.50 (a LAN host)
#           8.8.8.8 (the internet)  100.100.100.100 (the tailnet)
#           10.42.2.10 (lab) and 10.42.2.11 (home): VMs on another node, routed like the mesh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
ingot=${INGOT:-$here/../target/debug/ingot}
[ -x "$ingot" ] || { echo "no ingot at $ingot; cargo build -p ingot first" >&2; exit 2; }
fails=0

drop_namespaces() {
  for ns in cn-vm1 cn-vm2 cn-vm3 cn-node cn-lan; do ip netns del "$ns" 2>/dev/null || true; done
}
drop_namespaces
tmp=$(mktemp -d)
listeners=""
cleanup() {
  status=$?
  [ -z "$listeners" ] || kill $listeners 2>/dev/null || true
  drop_namespaces
  rm -rf "$tmp"
  exit "$status"
}
trap cleanup EXIT

# Run a command inside a namespace.
on() { ns=$1; shift; ip netns exec "$ns" "$@"; }
for ns in cn-vm1 cn-vm2 cn-vm3 cn-node cn-lan; do ip netns add "$ns"; on "$ns" ip link set lo up; done

# the node, on the home LAN
ip link add lan0 netns cn-node type veth peer name eth0 netns cn-lan
on cn-node ip addr add 192.168.68.109/24 dev lan0
on cn-node ip link set lan0 up
on cn-node ip route add default via 192.168.68.1
on cn-lan ip addr add 192.168.68.1/24 dev eth0
on cn-lan ip addr add 192.168.68.50/24 dev eth0
on cn-lan ip link set eth0 up
for a in 8.8.8.8 100.100.100.100 10.42.2.10 10.42.2.11; do on cn-lan ip addr add "$a/32" dev lo; done
on cn-lan ip route add 10.42.1.0/24 via 192.168.68.109
on cn-node ip route add 10.42.2.0/24 via 192.168.68.1

# the VMs, each a /32 behind the node on an isolated bridge port, as
# cloud-init and libvirt set them up
on cn-node ip link add cu-42-1 type bridge
on cn-node ip addr add 10.42.1.1/24 dev cu-42-1
on cn-node ip link set cu-42-1 up
n=0
for vm in cn-vm1:10.42.1.10 cn-vm2:10.42.1.11 cn-vm3:10.42.1.12; do
  ns=${vm%%:*} addr=${vm#*:} n=$((n + 1))
  ip link add "vnet$n" netns cn-node type veth peer name eth0 netns "$ns"
  on cn-node ip link set "vnet$n" master cu-42-1 up
  on cn-node bridge link set dev "vnet$n" isolated on
  on "$ns" ip link set eth0 mtu 1220 up
  on "$ns" ip addr add "$addr/32" dev eth0
  on "$ns" ip route add default via 10.42.1.1 dev eth0 onlink
done

cat >"$tmp/network.json" <<'EOF'
{
  "generation": 1,
  "network": { "subnet": "10.42.1.0/24", "gateway": "10.42.1.1", "space": "10.42.0.0/16" },
  "projects": [
    { "id": "00000000-0000-0000-0000-000000000001", "name": "lab",
      "vms": [ { "name": "k3s-1", "address": "10.42.1.10" }, { "name": "k3s-3", "address": "10.42.1.12" },
               { "name": "k3s-2", "address": "10.42.2.10" } ] },
    { "id": "00000000-0000-0000-0000-000000000002", "name": "home",
      "vms": [ { "name": "backup", "address": "10.42.1.11" }, { "name": "photos", "address": "10.42.2.11" } ] }
  ]
}
EOF
export COPPER_NET_READY="$tmp/ready"
on cn-node "$ingot" net --once --state-dir "$tmp"

# A listener that records who connected, and a connect that says yes or no.
listen() {
  ns=$1 addr=$2 port=$3
  on "$ns" python3 -c '
import socket, sys
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind((sys.argv[1], int(sys.argv[2]))); s.listen()
while True:
    c, peer = s.accept(); open(sys.argv[3], "a").write(peer[0] + "\n"); c.close()
' "$addr" "$port" "$tmp/$ns-$port" &
  listeners="$listeners $!"
}
# reaches NS HOST PORT [FROM]: connect from NS, optionally from address FROM.
reaches() {
  on "$1" python3 -c '
import socket, sys
try:
    socket.create_connection((sys.argv[1], int(sys.argv[2])), timeout=1,
                             source_address=(sys.argv[3], 0) if len(sys.argv) > 3 else None).close()
except OSError: sys.exit(1)
' "$2" "$3" ${4:+"$4"}
}
expect() {
  want=$1 what=$2; shift 2
  if "$@"; then got=yes; else got=no; fi
  if [ "$got" = "$want" ]; then echo "ok    $what"; else echo "FAIL  $what (wanted $want)"; fails=$((fails + 1)); fi
}

listen cn-lan 0.0.0.0 80
listen cn-node 0.0.0.0 22
listen cn-node 10.42.1.1 53
for vm in cn-vm1:10.42.1.10 cn-vm2:10.42.1.11 cn-vm3:10.42.1.12; do listen "${vm%%:*}" "${vm#*:}" 22; done
sleep 1

expect yes "ingot net says it's ready" test -f "$tmp/ready"
expect yes "a VM reaches the internet" reaches cn-vm1 8.8.8.8 80
expect yes "  as the node, through NAT" grep -qx 192.168.68.109 "$tmp/cn-lan-80"
expect no  "a VM can't reach the home router" reaches cn-vm1 192.168.68.1 80
expect no  "a VM can't reach a home LAN host" reaches cn-vm1 192.168.68.50 80
expect no  "a VM can't reach the tailnet" reaches cn-vm1 100.100.100.100 80
expect no  "a VM can't reach the node's own services" reaches cn-vm1 10.42.1.1 22
expect no  "  even at its LAN address" reaches cn-vm1 192.168.68.109 22
expect yes "a VM reaches DNS on its gateway" reaches cn-vm1 10.42.1.1 53
expect yes "a VM reaches a VM of its project on the same node" reaches cn-vm1 10.42.1.12 22
expect yes "  under its own address" grep -qx 10.42.1.10 "$tmp/cn-vm3-22"
expect no  "a VM can't reach another project's VM on the same node" reaches cn-vm1 10.42.1.11 22
expect no  "  nor the other way" reaches cn-vm2 10.42.1.10 22
# Straight across the bridge, past the host: the isolated port stops it.
on cn-vm1 ip route add 10.42.1.12/32 dev eth0
on cn-vm1 ip neigh replace 10.42.1.12 lladdr "$(on cn-vm3 cat /sys/class/net/eth0/address)" dev eth0
expect no  "a VM can't reach even its own project directly, only through the host" reaches cn-vm1 10.42.1.12 22
on cn-vm1 ip route del 10.42.1.12/32 dev eth0
expect yes "a VM reaches a VM on another node" reaches cn-vm1 10.42.2.10 80
expect yes "  under its own address" grep -qx 10.42.1.10 "$tmp/cn-lan-80"
expect yes "a VM on another node reaches a VM of its project here" reaches cn-lan 10.42.1.10 22 10.42.2.10
expect no  "  but not one of another project" reaches cn-lan 10.42.1.10 22 10.42.2.11
expect no  "a LAN host with a route still can't get in" reaches cn-lan 10.42.1.10 22 192.168.68.50
expect yes "the node (ingot) reaches its VMs" reaches cn-node 10.42.1.11 22

on cn-node "$ingot" net --stop
expect no "stop removes the rules" sh -c 'ip netns exec cn-node nft list tables | grep -q copper'
expect no "  and the ready file" test -f "$tmp/ready"

if [ "$fails" = 0 ]; then echo "copper-net: all good"; else echo "copper-net: $fails failed"; exit 1; fi
