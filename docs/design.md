# Design decisions

## Principles
- An instance owns its boot disk. Delete is final.
- Progress is shown through instance state, not operation IDs.
- Users choose VM properties, including architecture. The cloud chooses the node; only the admin sees placement.
- No capacity means an immediate refusal.
- Kiln and ingot are plain systemd daemons.

## Kiln-ingot protocol
Kiln is the control plane, ingot the node agent, cu the CLI.
- Kiln's DB (SQLite) is the source of truth for desired state. The node is the source of truth for observed state.
- Ingot pulls desired state and pushes observed state. Kiln never pushes commands.
- Desired instance states: running | stopped | absent. Delete means mark absent; the agent removes VM and disk, reports absent, then the controller drops the row.
- Every action is idempotent; the controller may resend anything.
- The agent caches the last desired state on disk and reconciles toward it while the controller is unreachable. Nothing is created or deleted while disconnected.
- `generation` is a per-node counter bumped whenever that node's desired set changes. The agent echoes the generation it acted on.

```
POST /nodes/register        -> { status: pending | approved | retired }
GET  /nodes/{id}/desired    long-poll with ?after=<generation>
POST /nodes/{id}/observed
```

## Nodes
- Identity is a random node ID the agent generates on first start and stores in its state directory. Hostname is a display name only.
- A reinstalled machine is a new node. The old node stays offline with its instances attached until the admin retires it. Retiring marks its instances failed. Nothing is ever re-created automatically.
- Node states: pending, online, offline (derived from last_seen, 30 s), retired. New nodes need admin approval.

## Instance state as the user sees it
Derived from (desired, observed), never stored:

| desired | observed            | user sees |
|---------|---------------------|-----------|
| running | absent, starting    | starting  |
| running | running             | running   |
| running | failed              | failed    |
| stopped | running, stopping   | stopping  |
| stopped | stopped, absent     | stopped   |
| absent  | not absent          | deleting  |
| absent  | absent              | removed   |
| any     | node offline        | unknown, unless observed is failed, which stays failed |

Failed: the agent retries start with backoff for 5 minutes, then reports failed with the error. Stop then start resets the budget. Stop and delete are allowed from any state.

## Node side
- libvirt via `qemu:///system` for v1. Copper domains are named `copper-<instance id>`; everything else on the host is ignored.
- Boot disks are qcow2 overlays on base images. Images live in `<state dir>/images/<name>.img` for now; fetching them from kiln by hash is still to do.
- cloud-init seed ISO carries the hostname, SSH keys, and installs `qemu-guest-agent`. Addresses come from the agent once it's up, with the host ARP table as a fallback before that.
- Stop is ACPI shutdown, then destroy after 60 s if the guest ignores it.
- Failed operations retry every 10 s for 5 min, then the instance reports `failed` until its desired state changes.
- Capacity reported to kiln is host threads and memory minus a configurable reserve (default 2 threads, 4 GB) for the host itself.
- ingot needs a user in the `libvirt` and `kvm` groups, `qemu-img` and `cloud-localds` on the node, and a disks directory that libvirt's qemu user can reach.

## User API
- Projects group instances. Users have a role of admin or user.
- Auth is API tokens, stored hashed, created by the admin.
- Everything binds to the Tailscale address. The tailnet is the trust boundary for v1.
