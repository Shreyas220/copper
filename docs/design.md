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
- Boot disks are qcow2 overlays on base images. Base images come from kiln's catalogue by hash (see Images); a node-local `<state dir>/images/<name>.img` is used only when kiln has no catalogue entry for the name and arch.
- cloud-init seed ISO carries the hostname, SSH keys, and installs `qemu-guest-agent`. Addresses come from the agent once it's up, with the host ARP table as a fallback before that.
- Stop is ACPI shutdown, then destroy after 60 s if the guest ignores it.
- A reconciliation timer runs every 10 s independently of long-poll responses. Failed starts and stops retry for 5 min, then the instance reports `failed` until its desired state changes. Deletes retry every 10 s indefinitely while connected; exhausting the failure budget makes the error visible but does not stop cleanup retries.
- Capacity reported to kiln is host threads and memory minus a configurable reserve (default 2 threads, 4 GB) for the host itself. The reserve is node configuration; overcommit is kiln policy (see Placement).
- Resize: a VM that is off is brought up to its spec before it starts and while it stays stopped. vCPUs and memory are redefined only when they differ; disks only grow, and cloud-init grows the guest's root filesystem on the next boot. Applied sizes are cached per process, so an unchanged spec costs nothing per tick.
- The disk budget is current filesystem free space plus physical blocks allocated to boot disks in the reconciled desired snapshot. Kiln subtracts the full virtual disk reservations once. Unrelated host usage reduces the budget, and orphaned disks are not added back. `INGOT_RESERVE_DISK_GB` optionally reserves additional host headroom (default 0 GiB). Registration preserves the last observed capacity of known nodes; only a report for the current generation refreshes it. All resources remain reserved until confirmed deletion. This admission budget is not a filesystem quota; external disk growth can still exhaust the filesystem.
- Image names are lowercase filename stems starting with a letter or digit, followed by letters, digits, `.`, `_`, or `-` (maximum 128 bytes). Resolved base-image paths must remain inside the configured images directory. Libvirt lookup failures block disk deletion unless the domain is explicitly confirmed missing.
- ingot needs a user in the `libvirt` and `kvm` groups, `qemu-img` and `cloud-localds` on the node, and a disks directory that libvirt's qemu user can reach.

## Images
- Kiln keeps a content-addressed catalogue: blobs at `<db dir>/catalog/<sha256>.img`, and a table mapping a platform-neutral name and an arch to a blob. `kiln image import --name ubuntu-24.04 --arch x86_64 FILE` adds one; only qcow2 files are accepted. `install.sh kiln` imports Ubuntu 24.04 for its own arch, or for `--image-arch` arches.
- Creating an instance binds the name to the blob's hash in its spec. Re-importing a name later never changes existing instances.
- Nodes fetch a missing blob with `GET /nodes/{id}/images/{sha256}` using their node credential, verify the hash while writing, and publish it to `<images dir>/sha256/<hash>.img` only if it matched. Kiln serves only blobs that the catalogue or an instance still refers to.
- Downloads run in their own task, one at a time, so reconciliation and reports continue meanwhile. An instance waiting for its image reports `starting` with a note and does not spend its failure budget; a failed download is retried on the next reconcile.

## Placement
- Each node admits its reported capacity times a per-node overcommit ratio for CPU, memory and disk (default 1.0 each, at most 64). The admin sets them with `cu node set <id> --cpu-ratio 4`. Lowering a ratio below what is allocated evicts nothing; it only stops new placements.
- Among nodes that fit, kiln prefers the most free memory.
- Admin-only hints: `--node <id>` places on exactly that node or refuses; `--spread <group>` prefers nodes holding the fewest instances of that group in the project. Spread is soft and never refuses on its own.
- Resize changes a stopped instance in place. It must fit on its own node once its current allocation is released, or it is refused; instances never move.

## Backends
The reconciler drives a `Hypervisor` trait (`ingot/src/hv.rs`) that owns both storage preparation and the VM lifecycle, so each platform keeps its own disk format. The backend is chosen at compile time by target OS.
- **libvirt** (Linux): the production backend above.
- **tart** (macOS, Apple silicon): Apple's Virtualization.framework through the Tart CLI. The boot disk is the cloud image converted to raw under `~/.tart/vms/copper-<id>/`; the seed ISO is built with `hdiutil`. launchd replaces libvirtd for persistence: a running VM has a LaunchAgent whose job is `tart run`, created on start and removed on stop, so running VMs come back after a reboot and login. Guests are aarch64 only, bridged onto a host interface given by `INGOT_BRIDGE` (default `en0`); kiln's per-NIC bridge and MAC are ignored and the address is read back with `tart ip`. The node needs `tart`, `qemu-img`, an arm64 cloud image under the same image name, and a user session that stays logged in. Type-checked on Linux with `cargo check -p ingot --features tart`; not yet run on a Mac.
- Image names are platform-neutral (`ubuntu-24.04`); the catalogue holds one blob per arch under the same name.

## macOS node
- `dist/macos/prepare.sh` turns an Apple silicon Mac into a node: Remote Login, no sleep, restart after power loss, Homebrew, Tart, QEMU, and ingot plus cu under `~/.copper`.
- ingot runs as the LaunchAgent `com.copper.ingot` in the logged-in user's session, with `KeepAlive` and logs at `~/.copper/ingot.log`. launchd has no `RestartPreventExitStatus`, so a wrapper turns exit status 78 into a clean exit that `KeepAlive` leaves alone.
- Each running VM is its own LaunchAgent, so restarting or updating ingot never stops guests.
- Settings live in `~/.copper/config`. `prepare.sh --update` fetches new binaries, swaps them by rename (overwriting a running signed binary in place gets it killed on Apple silicon), and restarts ingot without sudo.
- Auto-login has to be turned on by hand in System Settings so VMs come back after a reboot.

## User API
- Projects group instances and belong to a user. Users have a role of admin or user. Admins list all projects and address another user's project with `cu --project owner:project ...`; ordinary users can only access their own projects.
- Auth is API tokens, stored hashed, created by the admin. `kiln token --user NAME` preserves existing roles; only an explicit `--role` changes one. New users default to role `user`.
- Kiln binds the Tailscale address by default. The tailnet is the trust boundary for v1.
- `--listen` takes several addresses, so kiln can also serve the home LAN (`install.sh kiln --lan`). That is plain HTTP: user and node bearer tokens cross the LAN unencrypted, and kiln logs a warning for every listener outside loopback and the tailnet. TLS is not built.
- `cu resize <name> --vcpus --memory --disk` works on stopped instances; `cu image ls` lists the catalogue.

## SSH access
- Guests sit on the home LAN, which the tailnet does not route. `cu ssh <name>` reaches them anyway by tunnelling through kiln, so it works wherever cu reaches kiln.
- cu runs `ssh` with itself as the ProxyCommand. That opens `GET /v1/projects/{p}/instances/{name}/ssh` with `Upgrade: copper-tcp`; kiln checks the caller owns the project (or is admin) and that the instance is running, connects to port 22 on one of its addresses, and relays bytes after `101 Switching Protocols`. SSH still authenticates the key injected at creation.
- The tunnel only ever targets port 22. Addresses come from the guest agent, so a guest controls them: kiln only relays to private LAN addresses that are not its own, never to loopback, link-local or tailnet addresses. That reaches nothing a bridged guest couldn't reach itself.
- Host keys are remembered under `copper-<instance id>`, because LAN addresses get reused but instance ids never do.
- `cu ssh-config <name>` prints a `Host <name>.copper` entry for scp, rsync and editors. Its ProxyCommand reads `CU_TOKEN` from the environment; the token is never written to the file or passed as an argument.
