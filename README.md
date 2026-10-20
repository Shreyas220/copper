<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/copper-logo-dark.png">
    <img alt="copper" src=".github/assets/copper-logo-light.png" width="280">
  </picture>
</p>

An Oxide-style mini cloud for a couple of home machines. Rust, plain systemd daemons.

- `kiln/`   the control plane: SQLite source of truth, placement, user API
- `ingot/`  the node agent: runs on every compute node, reconciles libvirt toward desired state
- `cu/`     the command line client
- `api/`    wire types shared by all three

See `docs/design.md` for the decisions so far.
