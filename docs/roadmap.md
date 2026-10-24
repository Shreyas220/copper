# Roadmap

Goals for copper, in the order they make sense to build. [design.md](design.md) describes how the finished parts work.

## Near term, finishing what exists

- [ ] Test and fix the Tart backend on the mini. Waiting for the hardware. CI now compiles it and runs the unit tests on macOS; it has never driven a real Tart VM. Then take the preview tag off the landing page.
- [x] A launchd-aware macOS section in the docs.
- [x] Install on the laptop for real with the systemd units, so kiln and ingot stop being dev processes.
- [x] LAN binding for kiln: several listen addresses and `install.sh kiln --lan`, with a warning for plain HTTP outside loopback and the tailnet. Kept for odd setups; copper itself now runs on the tailnet.
- [x] Images fetched from kiln by hash. Kiln holds the catalogue, nodes download and verify once.
- [x] macOS installer parity: kiln, ingot and cu for Apple silicon in the release, and `prepare.sh --update` to update in place.
- [x] SSH into your own VMs from anywhere: `cu ssh` and `cu ssh-config`, tunnelled through kiln.
- [x] SSH keys saved in kiln, made in the browser or with `cu key new`, and a fix for VMs made without one.

## Networking

One architecture wherever a node is: on the home router or anywhere on the tailnet.

- [x] Copper networks: `10.42.0.0/16`, a /24 per node, a fixed address per VM written in by cloud-init.
- [x] `copper-net.service`: NAT to the internet, nothing private, nothing on the node but DNS. Tested in network namespaces in CI.
- [x] Reach VMs through their node: `cu ssh`, `cu forward <vm> <port>` and the serial console, with kiln never connecting to a node or a VM.
- [x] Kiln on the tailnet and loopback only.
- [x] Every VM packet goes through its host: `/32` addresses with an on-link gateway, isolated bridge ports, MTU 1220.
- [x] Projects are walls: VMs only reach VMs in their own project.
- [x] Names: `<vm>.<project>.copper` from ingot's DNS on each gateway, per project.
- [x] A root helper (`ingot net`) applies each node's network from a file ingot writes, replacing the `copper-net` script.
- [x] The mesh: WireGuard between every pair of nodes, LAN first, the tailnet when the LAN doesn't answer; the Nodes page shows each peer's path. Tested with three nodes in namespaces in CI; on real nodes once a second Linux node or the Mac mini joins.
- [ ] Tart nodes on copper networks: the Mac's VM network on the node's /24, pf rules, private ports, `wireguard-go`. Tested by hand on the Mac mini first.
- [ ] A jump host in kiln, so plain `ssh` reaches a VM by name without cu.

## Scheduling and capacity

- [x] CPU overcommit ratio, so eight threads can back more than eight vCPUs.
- [x] Memory and disk overcommit ratios to match. Ratios are set per node by the admin; the reserve is set per node in ingot.
- [x] Instance resize: change vCPUs, memory and disk on a stopped instance.
- [x] Placement hints: pin an instance to a node or spread a group across nodes, admin-only like placement today.

## The console

- [x] A web console served by kiln: overview, instances, a new-VM flow with a dry run, nodes, images, quotas, users and tokens.
- [x] Browser sign-in with invite links and password resets.
- [x] An event log behind history and "needs a look".
- [x] Live metrics for VMs and nodes, the last hour.
- [x] A serial console in the browser, on libvirt nodes.
- [x] Per-project quotas, and named, revocable tokens.

## Later

- TLS for kiln, for anyone binding it outside the tailnet.
- Serial consoles on Tart nodes.
- Containers: a `kind` field and a podman backend.
- GPU passthrough of the whole card to one VM.
- Data volumes and snapshots.
- CKS: clusters of k3s VMs with a copper cloud controller and CSI driver.
- Backups of kiln's database, with a restore that can't resurrect deleted VMs.
- Load-balancer addresses from `10.42.0.0/24` for k3s, and ingress through kiln.
- Containers on the same network: a veth into the node's bridge, an address, walls and a name, like a VM.
