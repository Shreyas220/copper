# Design decisions

## Principles
- An instance owns its boot disk. Delete is final.
- Progress is shown through instance state, not operation IDs.
- Users choose VM properties, including architecture. The cloud chooses the node; only the admin sees placement.
- No capacity means an immediate refusal.
- Controller and agent are plain systemd daemons.

## Controller-agent protocol
- Controller DB (SQLite) is the source of truth for desired state. The node is the source of truth for observed state.
- The agent pulls desired state and pushes observed state. The controller never pushes commands.
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
| any     | node offline        | unknown   |

Failed: the agent retries start with backoff for 5 minutes, then reports failed with the error. Stop then start resets the budget. Stop and delete are allowed from any state.

## Node side
- libvirt via `qemu:///system` for v1. Domains carry homecloud metadata; everything else on the host is ignored.
- Boot disks are qcow2 overlays on base images in `/var/lib/homecloud/images/`, fetched from the controller by hash.

## User API
- Projects group instances. Users have a role of admin or user.
- Auth is API tokens, stored hashed, created by the admin.
- Everything binds to the Tailscale address. The tailnet is the trust boundary for v1.
