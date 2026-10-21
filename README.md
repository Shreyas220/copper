<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/copper-logo-dark.png">
    <img alt="copper" src=".github/assets/copper-logo-light.png" width="280">
  </picture>
</p>

An Oxide inspired mini cloud for a couple of home machines. Rust, plain systemd daemons.

- `kiln/`   the control plane: SQLite source of truth, placement, user API
- `ingot/`  the node agent: runs on every compute node, reconciles libvirt toward desired state
- `cu/`     the command line client
- `api/`    wire types shared by all three

See `docs/design.md` for the decisions so far and `docs/roadmap.md` for what comes next.

## Install

One machine runs kiln. Name it `kiln` on your tailnet so every node finds it by default.

```sh
curl -fsSL https://raw.githubusercontent.com/Shreyas220/copper/main/install.sh | sudo sh -s -- kiln
sudo -u copper kiln --db /var/lib/copper/kiln.db token --user you --role admin
```

Every compute node runs ingot. It needs libvirt, `qemu-utils` and `cloud-image-utils`.

```sh
curl -fsSL https://raw.githubusercontent.com/Shreyas220/copper/main/install.sh | sudo sh -s -- ingot
```

An Apple silicon Mac becomes a node with Tart instead of libvirt. Run this as the user that stays logged in; it installs Homebrew, Tart, QEMU and ingot as a LaunchAgent:

```sh
curl -fsSL https://raw.githubusercontent.com/Shreyas220/copper/main/dist/macos/prepare.sh | sh -s -- --kiln http://kiln:8400
```

Then approve the node and make a VM:

```sh
export CU_URL=http://kiln:8400 CU_TOKEN=cu_...
cu node ls && cu node approve <id>
cu project create default
cu create web --vcpus 2 --memory 2048
cu ls
```

`install.sh --help` lists the options, including `--binaries target/release` to install a local build.
