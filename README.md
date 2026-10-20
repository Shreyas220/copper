# copper

homecloud: an Oxide-style mini cloud for a couple of home machines. Rust, plain systemd daemons.

- `api/`        shared wire types for both the user API and the controller-agent protocol
- `controller/` the control plane: SQLite source of truth, placement, user API
- `agent/`      runs on every compute node, reconciles libvirt toward desired state
- `hc/`         command line client

See `docs/design.md` for the decisions so far.
