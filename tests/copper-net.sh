#!/bin/sh
# Test copper-net's rules in throwaway network namespaces; the host's own
# network is never touched. Needs root and nft. CI runs it; elsewhere:
#
#   sudo sh tests/copper-net.sh
#
#   cn-vm   10.42.1.10 ── cu-42-1 bridge ── cn-node ── lan0 192.168.68.109
#                         10.42.1.1                        │
#   cn-lan  192.168.68.1 (router)  192.168.68.50 (a LAN host)
#           8.8.8.8 (the internet)  100.100.100.100 (the tailnet)
#           10.42.2.10 (a VM on another node, routed like a tailnet subnet route)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
net="$here/../dist/systemd/copper-net"
fails=0

drop_namespaces() {
  for ns in cn-vm cn-node cn-lan; do ip netns del "$ns" 2>/dev/null || true; done
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
for ns in cn-vm cn-node cn-lan; do ip netns add "$ns"; on "$ns" ip link set lo up; done

# the node, on the home LAN
ip link add lan0 netns cn-node type veth peer name eth0 netns cn-lan
on cn-node ip addr add 192.168.68.109/24 dev lan0
on cn-node ip link set lan0 up
on cn-node ip route add default via 192.168.68.1
on cn-lan ip addr add 192.168.68.1/24 dev eth0
on cn-lan ip addr add 192.168.68.50/24 dev eth0
on cn-lan ip link set eth0 up
for a in 8.8.8.8 100.100.100.100 10.42.2.10; do on cn-lan ip addr add "$a/32" dev lo; done
on cn-lan ip route add 10.42.1.0/24 via 192.168.68.109
on cn-node ip route add 10.42.2.0/24 via 192.168.68.1

# the VM, on the node's copper bridge
on cn-node ip link add cu-42-1 type bridge
on cn-node ip addr add 10.42.1.1/24 dev cu-42-1
on cn-node ip link set cu-42-1 up
ip link add vnet0 netns cn-node type veth peer name eth0 netns cn-vm
on cn-node ip link set vnet0 master cu-42-1 up
on cn-vm ip addr add 10.42.1.10/24 dev eth0
on cn-vm ip link set eth0 up
on cn-vm ip route add default via 10.42.1.1

COPPER_NET_READY="$tmp/ready" on cn-node sh "$net" start

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
reaches() {
  on "$1" python3 -c '
import socket, sys
try: socket.create_connection((sys.argv[1], int(sys.argv[2])), timeout=1).close()
except OSError: sys.exit(1)
' "$2" "$3"
}
expect() {
  want=$1 what=$2; shift 2
  if "$@"; then got=yes; else got=no; fi
  if [ "$got" = "$want" ]; then echo "ok    $what"; else echo "FAIL  $what (wanted $want)"; fails=$((fails + 1)); fi
}

listen cn-lan 0.0.0.0 80
listen cn-node 0.0.0.0 22
listen cn-node 10.42.1.1 53
listen cn-vm 10.42.1.10 22
sleep 1

expect yes "copper-net says it's ready" test -f "$tmp/ready"
expect yes "a VM reaches the internet" reaches cn-vm 8.8.8.8 80
expect yes "  as the node, through NAT" grep -qx 192.168.68.109 "$tmp/cn-lan-80"
expect no  "a VM can't reach the home router" reaches cn-vm 192.168.68.1 80
expect no  "a VM can't reach a home LAN host" reaches cn-vm 192.168.68.50 80
expect no  "a VM can't reach the tailnet" reaches cn-vm 100.100.100.100 80
expect no  "a VM can't reach the node's own services" reaches cn-vm 10.42.1.1 22
expect no  "  even at its LAN address" reaches cn-vm 192.168.68.109 22
expect yes "a VM reaches DNS on its gateway" reaches cn-vm 10.42.1.1 53
expect yes "a VM reaches a VM on another node" reaches cn-vm 10.42.2.10 80
expect yes "  under its own address" grep -qx 10.42.1.10 "$tmp/cn-lan-80"
expect yes "the node (ingot) reaches its VM" reaches cn-node 10.42.1.10 22

COPPER_NET_READY="$tmp/ready" on cn-node sh "$net" stop
expect no "stop removes the rules" sh -c 'ip netns exec cn-node nft list tables | grep -q copper'
expect no "  and the ready file" test -f "$tmp/ready"

if [ "$fails" = 0 ]; then echo "copper-net: all good"; else echo "copper-net: $fails failed"; exit 1; fi
