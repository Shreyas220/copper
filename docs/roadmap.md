# Roadmap

Goals for copper, in the order they make sense to build. [design.md](design.md) describes how the finished parts work.

## Near term, finishing what exists

- [ ] Test and fix the Tart backend on the mini. Waiting for the hardware. CI now compiles it and runs the unit tests on macOS; it has never driven a real Tart VM. Then take the preview tag off the landing page.
- [x] A launchd-aware macOS section in the docs.
- [ ] Install on the laptop for real with the systemd units, so kiln and ingot stop being dev processes. Needs the owner's sudo; the installer is ready.
- [x] LAN binding for kiln: several listen addresses and `install.sh kiln --lan`, with a warning for plain HTTP outside loopback and the tailnet. TLS itself is still to do.
- [x] Images fetched from kiln by hash. Kiln holds the catalogue, nodes download and verify once.
- [x] macOS installer parity: kiln, ingot and cu for Apple silicon in the release, and `prepare.sh --update` to update in place.

## Scheduling and capacity

- [x] CPU overcommit ratio, so eight threads can back more than eight vCPUs.
- [x] Memory and disk overcommit ratios to match. Ratios are set per node by the admin; the reserve is set per node in ingot.
- [x] Instance resize: change vCPUs, memory and disk on a stopped instance.
- [x] Placement hints: pin an instance to a node or spread a group across nodes, admin-only like placement today.

## Later

- TLS for kiln, so LAN binding doesn't expose tokens.
- Containers: a `kind` field and a podman backend.
- GPU passthrough of the whole card to one VM.
- Data volumes and snapshots.
- CKS: clusters of k3s VMs with a copper cloud controller and CSI driver.
- Serial console through kiln, metrics and an event log.
- A web UI, per-project quotas, and backups of kiln's database.
- Private networks per project, stable addresses and port forwarding.
