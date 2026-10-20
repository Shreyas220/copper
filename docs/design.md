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
- The agent durably caches the last desired state and uses its inventory to account for existing boot-disk blocks at registration. The cache does not authorize VM recovery after a reboot without Kiln. After startup or a failed controller request, reconciliation is suspended until a fresh authenticated desired-state response arrives. Existing VMs keep running; no resources are created, stopped, or deleted while disconnected. A network failure is detected by request failure or timeout. After any current request failure, the next desired-state fetch omits `after` so reconnecting does not wait for a long-poll timeout. Only the polling task publishes connection state; delayed report failures cannot supersede a newer desired response.
- `generation` is a per-node counter bumped whenever that node's desired set changes, including removal after a confirmed deletion. The agent captures observations and the generation it reconciled under the same lock. Every desired instance is reported, including absent instances, until removed from the desired snapshot.

```
POST /nodes/register        -> { status: pending | approved | retired }
GET  /nodes/{id}/desired    long-poll with ?after=<generation>
POST /nodes/{id}/observed
```

## Nodes
- Identity is a random node ID the agent generates on first start and stores in its state directory. Hostname is a display name only.
- A reinstalled machine is a new node. The old node stays offline with its instances attached until the admin retires it. Retiring marks its instances failed. Instances are never automatically relocated to another node.
- Node states: pending, online, offline (derived from last_seen, 30 s), retired. New nodes need admin approval.
- An agent generates a private `node-token` file alongside its node ID. Every node request uses this bearer credential; registration pins its SHA-256 hash before admin approval. Registration cannot replace a pinned credential. The tailnet remains the transport trust boundary.
- Retired agents receive HTTP 410 and stop reconciling, leaving existing VMs running for manual recovery. Invalid credentials (HTTP 401) also stop the agent. Pending approval (HTTP 403) suspends reconciliation and returns the agent to registration until approved again. Ingot exits with status 78 for these cases; the supplied systemd unit does not restart it.
- An admin may run `cu forget <instance-id>` after retiring the node. This removes tracking only; it does not erase VM disks or claim physical deletion. Normal deletion still waits for node confirmation.
- Upgrade: deploy Kiln and Ingot together. The credential migration moves existing approved nodes to pending. Start the updated agents to enroll their credentials, then approve them again. Until approval, their running VMs are left alone. Preserve `node-id` and `node-token` together across normal restarts; losing credentials requires retiring the old identity and enrolling a new node.

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
- A reconciliation timer runs every 10 s independently of long-poll responses. Failed starts and stops retry for 5 min, then the instance reports `failed` until its desired state changes. Deletes retry every 10 s indefinitely while connected; exhausting the failure budget makes the error visible but does not stop cleanup retries.
- Capacity reported to kiln is host threads and memory minus a configurable reserve (default 2 threads, 4 GB) for the host itself.
- The disk budget is current filesystem free space plus physical blocks allocated to boot disks in the reconciled desired snapshot. Kiln subtracts the full virtual disk reservations once. Unrelated host usage reduces the budget, and orphaned disks are not added back. `INGOT_RESERVE_DISK_GB` optionally reserves additional host headroom (default 0 GiB). Registration preserves the last observed capacity of known nodes; only a report for the current generation refreshes it. All resources remain reserved until confirmed deletion. This admission budget is not a filesystem quota; external disk growth can still exhaust the filesystem.
- Image names are lowercase filename stems starting with a letter or digit, followed by letters, digits, `.`, `_`, or `-` (maximum 128 bytes). Resolved base-image paths must remain inside the configured images directory. Libvirt lookup failures block disk deletion unless the domain is explicitly confirmed missing.
- ingot needs a user in the `libvirt` and `kvm` groups, `qemu-img` and `cloud-localds` on the node, and a disks directory that libvirt's qemu user can reach.

## User API
- Projects group instances and belong to a user. Users have a role of admin or user. Admins list all projects and address another user's project with `cu --project owner:project ...`; ordinary users can only access their own projects.
- Auth is API tokens, stored hashed, created by the admin. `kiln token --user NAME` preserves existing roles; only an explicit `--role` changes one. New users default to role `user`.
- Everything binds to the Tailscale address. The tailnet is the trust boundary for v1.
