#!/bin/sh
# Test the mesh between nodes that `ingot net` sets up, with three nodes in
# throwaway network namespaces; the host's own network is never touched.
# Needs root, nft, wireguard-tools and a built ingot. CI runs it; elsewhere:
#
#   cargo build -p ingot && sudo sh tests/mesh.sh
#
#   home LAN 192.168.10.0/24: na .1, nb .2, and a decoy at .3 that isn't nc
#   tailnet  100.64.0.0/24 (MTU 1280, like Tailscale): na .1, nb .2, nc .3
#   nc's own LAN is 192.168.20.0/24, which na and nb can't reach
#
#   na: va 10.42.1.10 (lab)   nb: vb 10.42.2.10 (lab)   nc: vc 10.42.3.10 (lab), vh 10.42.3.11 (home)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
ingot=${INGOT:-$here/../target/debug/ingot}
[ -x "$ingot" ] || { echo "no ingot at $ingot; cargo build -p ingot first" >&2; exit 2; }
fails=0
spaces="lan tail na nb nc va vb vc vh decoy"

drop_namespaces() {
  for ns in $spaces; do ip netns del "m-$ns" 2>/dev/null || true; done
}
drop_namespaces
tmp=$(mktemp -d)
helpers="" listeners=""
cleanup() {
  status=$?
  [ -z "$helpers$listeners" ] || kill $helpers $listeners 2>/dev/null || true
  drop_namespaces
  rm -rf "$tmp"
  exit "$status"
}
trap cleanup EXIT

on() { space=$1; shift; ip netns exec "m-$space" "$@"; }
for s in $spaces; do ip netns add "m-$s"; on "$s" ip link set lo up; done
on lan ip link add sw type bridge && on lan ip link set sw up
on tail ip link add sw mtu 1280 type bridge && on tail ip link set sw up

# plug NODE into the switch in namespace SW, as IFACE with ADDRESS
plug() {
  node=$1 sw=$2 iface=$3 address=$4 mtu=${5:-1500}
  ip link add "$iface" netns "m-$node" mtu "$mtu" type veth peer name "$node-$iface" netns "m-$sw" mtu "$mtu"
  on "$sw" ip link set "$node-$iface" master sw up
  on "$node" ip addr add "$address" dev "$iface"
  on "$node" ip link set "$iface" up
}
plug na lan eth0 192.168.10.1/24
plug nb lan eth0 192.168.10.2/24
plug decoy lan eth0 192.168.10.3/24
for n in na:1 nb:2 nc:3; do plug "${n%%:*}" tail ts0 "100.64.0.${n#*:}/24" 1280; done
on nc ip link add eth0 type dummy && on nc ip addr add 192.168.20.3/24 dev eth0 && on nc ip link set eth0 up

# Each node's VMs: /32s behind it, on isolated bridge ports.
vm() {
  node=$1 n=$2 vmspace=$3 vmaddr=$4 port=$5
  on "$node" ip link show "cu-42-$n" >/dev/null 2>&1 || {
    on "$node" ip link add "cu-42-$n" type bridge
    on "$node" ip addr add "10.42.$n.1/24" dev "cu-42-$n"
    on "$node" ip link set "cu-42-$n" mtu 1220 up
  }
  ip link add "$port" netns "m-$node" type veth peer name eth0 netns "m-$vmspace"
  on "$node" ip link set "$port" master "cu-42-$n" up
  on "$node" bridge link set dev "$port" isolated on
  on "$vmspace" ip link set eth0 mtu 1220 up
  on "$vmspace" ip addr add "$vmaddr/32" dev eth0
  on "$vmspace" ip route add default via "10.42.$n.1" dev eth0 onlink
}
vm na 1 va 10.42.1.10 vnet0
vm nb 2 vb 10.42.2.10 vnet0
vm nc 3 vc 10.42.3.10 vnet0
vm nc 3 vh 10.42.3.11 vnet1

# Start each node's helper; each makes its own key.
for n in na nb nc; do
  mkdir -p "$tmp/$n"
  COPPER_NET_RUN="$tmp/$n" COPPER_NET_KEYS="$tmp/$n/keys" on "$n" "$ingot" net --state-dir "$tmp/$n" >"$tmp/$n/log" 2>&1 &
  helpers="$helpers $!"
done
key() {
  for _ in $(seq 50); do
    [ -f "$tmp/$1/net-status.json" ] && { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["public_key"])' "$tmp/$1/net-status.json"; return; }
    sleep 0.2
  done
  echo "no key from $1" >&2; cat "$tmp/$1/log" >&2; exit 1
}
ka=$(key na) kb=$(key nb) kc=$(key nc)

# What kiln would send each node: its /24, its projects' VMs, the other two.
peer() { # NAME KEY N LAN...: endpoints LAN first, then the tailnet
  name=$1 k=$2 n=$3; shift 3
  eps=""
  for lan in "$@"; do eps="$eps{\"address\": \"$lan:51820\", \"via\": \"lan\"}, "; done
  printf '{"node": "00000000-0000-0000-0000-00000000000%s", "hostname": "%s", "public_key": "%s", "subnet": "10.42.%s.0/24",
    "endpoints": [%s{"address": "100.64.0.%s:51820", "via": "tailnet"}]}' "$n" "$name" "$k" "$n" "$eps" "$n"
}
lab='{"id": "00000000-0000-0000-0000-0000000000a1", "name": "lab", "vms": [
  {"name": "va", "address": "10.42.1.10"}, {"name": "vb", "address": "10.42.2.10"}, {"name": "vc", "address": "10.42.3.10"}]}'
home='{"id": "00000000-0000-0000-0000-0000000000a2", "name": "home", "vms": [{"name": "vh", "address": "10.42.3.11"}]}'
write() { # NODE N PROJECTS PEERS
  cat >"$tmp/$1/next.json" <<EOF
{"generation": 1, "network": {"subnet": "10.42.$2.0/24", "gateway": "10.42.$2.1", "space": "10.42.0.0/16"},
 "projects": [$3], "peers": [$4]}
EOF
  mv "$tmp/$1/next.json" "$tmp/$1/network.json"
}
# na and nb miss each other on the LAN at first, as after a power cut when
# everything boots at once: they settle on the tailnet, and have to find
# their way back to the LAN once it answers.
on na nft add table inet blocklan
on na nft add chain inet blocklan in '{ type filter hook input priority -10; }'
on na nft add rule inet blocklan in iifname eth0 udp dport 51820 drop
# nc's last known LAN address is the decoy's: a wrong guess, like another
# site using the same range.
write na 1 "$lab" "$(peer nb "$kb" 2 192.168.10.2), $(peer nc "$kc" 3 192.168.10.3)"
write nb 2 "$lab" "$(peer na "$ka" 1 192.168.10.1), $(peer nc "$kc" 3 192.168.10.3)"
write nc 3 "$lab, $home" "$(peer na "$ka" 1 192.168.10.1), $(peer nb "$kb" 2 192.168.10.2)"

# via NODE PEERKEY: the path NODE reports for the peer with that key
via() {
  python3 -c '
import json, sys
for p in json.load(open(sys.argv[1])).get("peers", []):
    if p["public_key"] == sys.argv[2]: print(p.get("via", "none"))
' "$tmp/$1/net-status.json" "$2"
}
path_is() { [ "$(via "$1" "$2")" = "$3" ]; }
for _ in $(seq 30); do path_is na "$kb" tailnet && path_is nb "$ka" tailnet && break; sleep 1; done
expect_early=$(path_is na "$kb" tailnet && path_is nb "$ka" tailnet && echo yes || echo no)
on na nft delete table inet blocklan
settled() { path_is na "$kb" lan && path_is nb "$ka" lan && path_is na "$kc" tailnet && path_is nc "$ka" tailnet && path_is nb "$kc" tailnet; }
for _ in $(seq 120); do settled && break; sleep 1; done

listen() {
  space=$1 addr=$2 port=$3
  on "$space" python3 -c '
import socket, sys
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind((sys.argv[1], int(sys.argv[2]))); s.listen()
while True:
    c, peer = s.accept(); open(sys.argv[3], "a").write(peer[0] + "\n"); c.close()
' "$addr" "$port" "$tmp/$space-$port" &
  listeners="$listeners $!"
}
reaches() {
  on "$1" python3 -c '
import socket, sys
try:
    socket.create_connection((sys.argv[1], int(sys.argv[2])), timeout=2,
                             source_address=(sys.argv[3], 0) if len(sys.argv) > 3 else None).close()
except OSError: sys.exit(1)
' "$2" "$3" ${4:+"$4"}
}
expect() {
  want=$1 what=$2; shift 2
  if "$@"; then got=yes; else got=no; fi
  if [ "$got" = "$want" ]; then echo "ok    $what"; else echo "FAIL  $what (wanted $want)"; fails=$((fails + 1)); fi
}
for v in va:10.42.1.10 vb:10.42.2.10 vc:10.42.3.10 vh:10.42.3.11; do listen "${v%%:*}" "${v#*:}" 22; done
sleep 1

expect yes "nodes that miss each other on the LAN fall back to the tailnet" test "$expect_early" = yes
expect yes "  and find the LAN once it answers" path_is na "$kb" lan
expect yes "  both ways" path_is nb "$ka" lan
expect yes "a node elsewhere is reached over the tailnet" path_is na "$kc" tailnet
expect yes "  both ways" path_is nc "$ka" tailnet
expect yes "  after the wrong LAN guess goes unanswered" path_is nb "$kc" tailnet
expect yes "a VM reaches its project on a node on the same LAN" reaches va 10.42.2.10 22
expect yes "  under its own address" grep -qx 10.42.1.10 "$tmp/vb-22"
expect yes "a VM reaches its project on a node elsewhere" reaches va 10.42.3.10 22
expect yes "  and back" reaches vc 10.42.1.10 22
expect no  "a VM can't reach another project on another node" reaches va 10.42.3.11 22
expect no  "  nor the other way" reaches vh 10.42.1.10 22
# Three tries: one lost ping isn't an MTU problem, and a path too small drops them all.
expect yes "1220-byte packets fit over the tailnet" on va ping -c 3 -W 2 -M do -s 1192 10.42.3.10
# nb borrows the address of a lab VM on nc; the walls would let it in, but
# na only takes nb's own /24 from nb's key.
on nb ip addr add 10.42.3.10/32 dev lo
expect no  "a node can't pass itself off as another node's VMs" reaches nb 10.42.1.10 22 10.42.3.10
on nb ip addr del 10.42.3.10/32 dev lo
# A device on the LAN that routes copper's range to a node still gets nowhere.
on decoy ip route add 10.42.0.0/16 via 192.168.10.1
expect no  "nothing on the LAN gets into a VM" reaches decoy 10.42.1.10 22
# kiln drops nb from na's peers: its route and peer go.
write na 1 "$lab" "$(peer nc "$kc" 3 192.168.10.3)"
sleep 3
expect no  "a node that leaves the mesh can't be reached" reaches va 10.42.2.10 22
expect yes "  and the others still can" reaches va 10.42.3.10 22

kill $helpers && wait $helpers 2>/dev/null || true
helpers=""
for n in na nb nc; do COPPER_NET_RUN="$tmp/$n" on "$n" "$ingot" net --stop; done
expect no "stop removes the mesh" on na ip link show copper-wg

if [ "$fails" = 0 ]; then echo "mesh: all good"; else
  for n in na nb nc; do echo "--- $n"; tail -20 "$tmp/$n/log"; done
  echo "mesh: $fails failed"; exit 1
fi
