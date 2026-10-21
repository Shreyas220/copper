# Roadmap

Goals for copper, in the order they make sense to build. Everything here is unbuilt; what exists is described in [design.md](design.md).

## Near term, finishing what exists

- Test and fix the Tart backend on the mini, then a launchd-aware macOS section in the docs and the preview tag off the landing page.
- Install on the laptop for real with the systemd units, so kiln and ingot stop being dev processes.
- LAN binding for kiln so the home network works without Tailscale, with TLS or at least a warning since node tokens travel in plain HTTP.
- Images fetched from kiln by hash instead of every node needing a hand-copied `ubuntu-24.04.img`. Kiln holds the catalogue, nodes download and verify once.
- macOS installer parity: `cu` for the Mac in the release, and `prepare.sh` able to update in place.

## Scheduling and capacity

- CPU overcommit ratio, so eight threads can back more than eight vCPUs, which is how every real cloud runs.
- Memory and disk overcommit policy to match, with the reserve and ratio set per node.
- Instance resize: change vCPUs, memory and disk on a stopped instance.
- Placement hints: pin an instance to a node or spread a group across nodes, admin-only like placement today.
