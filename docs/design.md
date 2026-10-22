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
- cloud-init seed ISO carries the hostname, SSH keys and, on a copper network, the VM's fixed address; it also installs `qemu-guest-agent`. Older bridged VMs' addresses come from the agent once it's up, with the host ARP table as a fallback before that.
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
- `cu` authenticates with API tokens, stored hashed. `kiln token --user NAME` preserves existing roles; only an explicit `--role` changes one. New users default to role `user`. Tokens have names and can be listed and revoked by their owner, or by an admin.
- The console authenticates with a session cookie (`HttpOnly`, `SameSite=Strict`, 30 days, extended while used). Passwords are Argon2id. Nobody signs up: an admin makes a one-time invite link, valid 24 hours, which sets a password and optionally creates a project; naming an existing user makes it a password reset. The first admin's link comes from `kiln invite` on the kiln machine.
- Cookie-authenticated writes must carry an `x-copper` header. A cross-site page can't add it without a CORS preflight, which kiln never grants. The serial console's WebSocket also checks `Origin`.
- Projects can have caps on vCPUs, memory and disk. Like capacity, a create or resize that would exceed one is refused up front; lowering a cap evicts nothing.
- Removing a user revokes their tokens and sessions and drops their empty projects. Users who still have instances can't be removed: delete is never implied.
- Kiln binds the Tailscale address by default. The tailnet is the trust boundary for v1.
- Kiln listens on the tailnet and loopback (see Networking). `--listen` takes several addresses, and `install.sh kiln --lan` still exists for setups without a tailnet, but that's plain HTTP: bearer tokens cross the LAN unencrypted, and kiln logs a warning for every listener outside loopback and the tailnet. TLS is not built.
- `cu resize <name> --vcpus --memory --disk` works on stopped instances; `cu image ls` lists the catalogue.

## Networking
One architecture wherever a node is: on the home router, or anywhere else on the tailnet.
- The tailnet is copper's network. Kiln listens on its tailnet address and loopback, not the LAN. Nodes, and everyone using copper, reach kiln over the tailnet; at home Tailscale connects directly across the LAN, so nothing is slower.
- Nodes only make outbound connections, to kiln. Kiln never connects to a node or to a VM.
- VMs live on copper's own network, not on the LAN of the site they run at. Copper's address space is `10.42.0.0/16` (`kiln --vm-network`). Each node gets `10.42.N.0/24` when it's approved, numbered from 1 and never reused; its gateway, `10.42.N.1`, is the node itself. Each VM gets a fixed address in its node's /24, from `.10` up, chosen by kiln at create and written into the guest by cloud-init, with the gateway as its router and DNS server. Addresses are known before a VM boots.
- On a Linux node the network is a libvirt network `copper-10.42.N.0-24` on bridge `cu-42-N`, in `open` mode: libvirt runs DNS for it and adds no firewall rules. `copper-net.service`, installed by `install.sh ingot` because it needs root, owns the host side: an nftables table `inet copper` that masquerades VM traffic leaving copper's range, lets VMs reach the internet and each other, and refuses everything else private (the home LAN, the tailnet, link-local) and everything on the node except DNS. On hosts with Docker it also lets `cu-*` traffic past Docker's forward-drop policy through `DOCKER-USER`. It matches nothing that isn't on a `cu-*` bridge or from copper's range. Ingot reports whether it's active, and the console warns on nodes where it isn't, because their VMs have no internet.
- Reaching a VM always goes through its node. `cu ssh`, `cu ssh-config`, `cu forward <vm> <port>` and the serial console all open a session at kiln. Kiln offers it to the VM's node over the long-poll the node keeps open (`GET /nodes/{id}/consoles`); ingot connects to its own VM on that port and dials kiln back with `Upgrade: copper-console`, or reports why it couldn't. Kiln answers 503 while a VM is still booting, and cu retries.
- Direct access over the tailnet: a node can advertise its /24 as a Tailscale subnet route (`sudo tailscale set --advertise-routes=10.42.N.0/24`, then approve it in the Tailscale admin console). Tailnet devices then reach VMs at their copper addresses, and nodes that also run with `--accept-routes` carry VM-to-VM traffic between nodes. Ingot reports whether its route is live, and the Nodes page shows the command until it is. Copper never changes tailscaled's settings itself.
- VMs made before copper networks are bridged onto `br0` with addresses from the home router. They keep running as they were and are reached through their node like the rest.
- macOS nodes use Tart's own NAT for now: VMs get Tart's 192.168.64.x addresses and are reached through ingot the same way. Putting them on the node's copper /24 needs the Mac's vmnet subnet set, which waits for the Mac mini.

## SSH access
- `cu ssh <name>` works the same from anywhere, through kiln and the VM's node (see Networking).
- cu runs `ssh` with itself as the ProxyCommand. That opens `GET /v1/projects/{p}/instances/{name}/ssh` (port 22; `/ports/{port}` for any other) with `Upgrade: copper-tcp`. Kiln checks the caller owns the project (or is admin) and that the instance is running, hands the session to its node, and relays bytes after `101 Switching Protocols`. SSH still authenticates the key injected at creation.
- Creating a VM needs at least one OpenSSH public key. Keys are only written by cloud-init at first boot and the images have no password, so kiln refuses a create without one rather than make a VM nobody can log into. Anything that isn't a one-line public key, such as a pasted private key, is refused too. The dry run needs no key.
- People save public keys in kiln (`/v1/keys`, `cu key`, the console's people page). Keys are personal: admins see only their own. A create that names no keys gets all of the caller's saved keys. Removing a saved key never touches VMs that have it.
- Kiln never holds a private key. `cu key new` runs `ssh-keygen` into `~/.copper/keys/<name>`, which `cu ssh` and `cu ssh-config` then offer alongside ssh's usual keys. The console makes Ed25519 pairs in the browser with a vendored TweetNaCl, because browsers only give their own crypto to HTTPS pages; it saves the public half and hands the private half to the browser's downloads.
- Ingot only ever connects to its own VMs. On a copper network that's the address kiln assigned. For older bridged VMs it's the address the guest reports, which the guest controls, so ingot only uses private LAN addresses that aren't loopback, link-local, the tailnet's or its own.
- Host keys are remembered under `copper-<instance id>`, because addresses get reused but instance ids never do.
- `cu ssh-config <name>` prints a `Host <name>.copper` entry for scp, rsync and editors. Its ProxyCommand reads `CU_TOKEN` from the environment; the token is never written to the file or passed as an argument.

## Serial console
- For when SSH won't. The browser opens a WebSocket to kiln for a running instance. Kiln records a pending session, and the instance's node learns of it through `GET /nodes/{id}/consoles`, a long-poll ingot keeps open. Ingot dials back with `Upgrade: copper-console` and pumps bytes between that connection and libvirt's console stream for the domain. Kiln never connects to a node.
- Opening a console takes it from any other viewer. Tart nodes poll for sessions, for `cu ssh` and `cu forward`, but have no serial console, so they refuse console sessions and the browser says so.

## Events and metrics
- Kiln logs what happens in an `events` table: creates, start and stop requests, resizes, deletes and removals, what nodes report (running, stopped, failed with the error), node approvals, retirements and nodes going quiet and coming back, invites, tokens and quota changes. Ordinary users see events in their own projects without node names. Events are kept 90 days.
- A sweeper checks every 10 s for approved nodes that stopped reporting, and logs "node quiet" once; the node's next report logs "node back".
- Ingot reports usage with each observation: per VM, CPU as a share of its vCPUs from libvirt's CPU time, memory from the balloon driver (QEMU's resident size until the guest reports), disk as blocks the overlay holds; per host, from /proc and statvfs. Kiln keeps the last hour in memory only.
- `running_since` is set when a node first reports an instance running after anything else, and cleared when it stops.

## Console
- Kiln serves the console from files built into the binary (`kiln/ui`, embedded by `build.rs`). It is plain JavaScript modules with no build step, and a plain client of the same API cu uses.
- Every element shows something kiln returns or does something kiln does. There are no placeholders for unbuilt features.
- The halftone artwork (`kiln/ui/art`) follows the same rule. A node is drawn by role: copper for the machine kiln runs on (its hostname matches `GET /v1/info`), bronze for other online nodes, grey for offline, a dotted outline for waiting. A stack drawing is used only when its pictured roles match the nodes exactly; otherwise each node gets its own slab.
- Pages build the DOM with a helper that only ever sets text, so names and messages from the API can't inject markup. The content security policy allows scripts, styles and fonts only from kiln itself.
