//! The host side of copper's VM network. The unprivileged ingot writes what
//! the node's network should be to `<state dir>/network.json`; `ingot net`,
//! run as root (copper-net.service on Linux, the com.copper.net launchd
//! daemon on macOS), checks it and turns it into firewall rules, replaced
//! atomically whenever the file changes: one nftables table on Linux, one pf
//! anchor on macOS.
//!
//! - VMs reach the internet, through NAT as the node, and other copper VMs.
//! - Traffic into a VM is let in only from VMs in the same project (the
//!   walls). VMs reach each other only through the host: their addresses are
//!   /32s and their bridge ports are isolated.
//! - Nothing else private: not the home LAN, the tailnet, link-local, or the
//!   node itself except DNS, which is redirected to ingot when it answers
//!   (on a Mac, to `ingot net` itself: see reach.rs).
//!
//! Every rule matches only copper's interfaces (`cu-*` bridges on Linux,
//! macOS's VM bridge), the mesh, or copper's range, so the host's own
//! traffic, ssh and Tailscale included, is never touched. The file comes
//! from a less privileged process, so nothing in it reaches the ruleset
//! except addresses that parse and lie inside copper's range.

use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, ensure, Context};
use copper_api::{Generation, MeshPeer, NetworkMap, NodeNetwork, ProjectNetwork};
use serde::{Deserialize, Serialize};

use crate::mesh::{self, Mesh};
use crate::reach;

pub const FILE: &str = "network.json";
/// Exists while copper's rules are in place; ingot reports it as
/// `network_ready`.
pub const READY_FILE: &str = if cfg!(target_os = "macos") { "/var/run/copper/net-ready" } else { "/run/copper/net-ready" };
/// The mesh's state, for ingot to report: `mesh::Status`.
pub const STATUS_FILE: &str = if cfg!(target_os = "macos") { "/var/run/copper/net-status.json" } else { "/run/copper/net-status.json" };
/// macOS's bridge for Tart's VMs (its shared network), and the pf anchor
/// copper loads its rules into: macOS's own pf.conf evaluates every
/// `com.apple/*` anchor, so no system file is edited.
pub const MAC_BRIDGE: &str = "bridge100";
const PF_ANCHOR: &str = "com.apple/290.copper";
/// Where the node's WireGuard key lives, root's alone. Under /etc/wireguard
/// because Ubuntu's AppArmor profile for `wg` lets it read keys nowhere else.
const KEY_DIR: &str = "/etc/wireguard/copper";
const DEFAULT_SPACE: &str = "10.42.0.0/16";
const PRIVATE: &str = "10.0.0.0/8, 100.64.0.0/10, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16";
const CHECK_EVERY: Duration = Duration::from_secs(2);
/// Re-applied this often even when nothing changed, in case something else
/// flushed the rules.
const REAPPLY_EVERY: Duration = Duration::from_secs(60);

/// What ingot asks for. Written by ingot, read by `ingot net`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetFile {
    pub generation: Generation,
    pub network: NodeNetwork,
    #[serde(default)]
    pub projects: Vec<ProjectNetwork>,
    /// Where ingot answers DNS for its VMs, once it does. Port 53 on the
    /// gateway is redirected there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<SocketAddrV4>,
    /// The other nodes, for the mesh.
    #[serde(default)]
    pub peers: Vec<MeshPeer>,
}

impl NetFile {
    pub fn new(generation: Generation, network: NodeNetwork, map: &NetworkMap, dns: Option<SocketAddrV4>) -> Self {
        Self { generation, network, projects: map.projects.clone(), dns, peers: map.peers.clone() }
    }
}

/// A `/n` range of IPv4 addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    base: Ipv4Addr,
    prefix: u8,
}

impl Range {
    fn parse(s: &str, prefix: u8) -> anyhow::Result<Self> {
        let (addr, p) = s.split_once('/').with_context(|| format!("{s}: not a range"))?;
        let base: Ipv4Addr = addr.parse().with_context(|| format!("{s}: not an IPv4 range"))?;
        ensure!(p.parse::<u8>().ok() == Some(prefix), "{s}: expected a /{prefix}");
        let r = Self { base, prefix };
        ensure!(r.mask() & u32::from(base) == u32::from(base), "{s}: host bits set");
        Ok(r)
    }
    fn mask(&self) -> u32 {
        u32::MAX << (32 - self.prefix as u32)
    }
    fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == u32::from(self.base)
    }
}

impl std::fmt::Display for Range {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.base, self.prefix)
    }
}

/// One project with a VM on this node: those VMs, and every address in the
/// project that may reach them.
#[derive(Debug, Clone, PartialEq)]
struct Wall {
    local: Vec<Ipv4Addr>,
    members: Vec<Ipv4Addr>,
}

/// A `NetFile`, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    space: Range,
    subnet: Option<Range>,
    gateway: Option<Ipv4Addr>,
    walls: Vec<Wall>,
    dns: Option<SocketAddrV4>,
    peers: Vec<mesh::Peer>,
}

impl Plan {
    /// Before ingot has written a file: NAT and nothing private, no walls
    /// yet, so nothing gets into a VM that it didn't ask for, and no peers.
    pub fn base() -> Self {
        Self { space: Range::parse(DEFAULT_SPACE, 16).unwrap(), subnet: None, gateway: None, walls: vec![], dns: None, peers: vec![] }
    }

    pub fn check(file: &NetFile) -> anyhow::Result<Self> {
        let space = Range::parse(&file.network.space, 16)?;
        ensure!(space.base.is_private(), "{space}: not a private range");
        let subnet = Range::parse(&file.network.subnet, 24)?;
        ensure!(space.contains(subnet.base), "{subnet} is outside {space}");
        let gateway: Ipv4Addr = file.network.gateway.parse().context("gateway")?;
        ensure!(u32::from(gateway) == u32::from(subnet.base) + 1, "gateway {gateway} isn't .1 of {subnet}");
        let mut walls = vec![];
        for p in &file.projects {
            let mut members = vec![];
            for vm in &p.vms {
                let ip: Ipv4Addr = vm.address.parse().with_context(|| format!("address of {}", vm.name))?;
                ensure!(space.contains(ip), "{ip} ({}) is outside {space}", vm.name);
                members.push(ip);
            }
            members.sort();
            members.dedup();
            let local: Vec<_> = members.iter().copied().filter(|ip| subnet.contains(*ip) && *ip != gateway).collect();
            if !local.is_empty() {
                walls.push(Wall { local, members });
            }
        }
        // An address can only be in one project.
        let mut seen = std::collections::HashSet::new();
        for ip in walls.iter().flat_map(|w| &w.local) {
            ensure!(seen.insert(*ip), "{ip} is in more than one project");
        }
        if let Some(dns) = file.dns {
            ensure!(*dns.ip() == gateway && dns.port() >= 1024, "DNS must be on the gateway, on a port above 1023");
        }
        let mut peers: Vec<mesh::Peer> = vec![];
        for p in &file.peers {
            ensure!(mesh::valid_key(&p.public_key), "{}: not a WireGuard key", p.hostname);
            let range = Range::parse(&p.subnet, 24)?;
            ensure!(space.contains(range.base) && range != subnet, "{}: {range} isn't another node's part of {space}", p.hostname);
            ensure!(!peers.iter().any(|q| q.public_key == p.public_key || q.subnet == p.subnet), "{}: twice in the mesh", p.hostname);
            let endpoints = p.endpoints.iter()
                .map(|e| e.address.parse::<SocketAddrV4>().map(|a| (a, e.via)).with_context(|| format!("{}: endpoint {}", p.hostname, e.address)))
                .collect::<anyhow::Result<_>>()?;
            peers.push(mesh::Peer { public_key: p.public_key.clone(), subnet: range.to_string(), endpoints });
        }
        Ok(Self { space, subnet: Some(subnet), gateway: Some(gateway), walls, dns: file.dns, peers })
    }

    /// The node's own VMs.
    fn local_vms(&self) -> Vec<Ipv4Addr> {
        self.walls.iter().flat_map(|w| w.local.iter().copied()).collect()
    }

    /// The nftables script: declares the table first so deleting it is safe
    /// on a fresh boot, then replaces it whole, in one transaction.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn render(&self) -> String {
        let space = self.space;
        let mut s = String::from("table inet copper\ndelete table inet copper\ntable inet copper {\n");
        let _ = writeln!(s, "  set private {{\n    type ipv4_addr; flags interval\n    elements = {{ {PRIVATE} }}\n  }}");
        for (i, w) in self.walls.iter().enumerate() {
            let _ = writeln!(s, "  set project_{i} {{\n    type ipv4_addr\n    elements = {{ {} }}\n  }}", join(&w.members));
            let _ = writeln!(s, "  chain project_{i} {{\n    ip saddr @project_{i} accept\n  }}");
        }
        let entries: Vec<String> = self.walls.iter().enumerate()
            .flat_map(|(i, w)| w.local.iter().map(move |ip| format!("{ip} : jump project_{i}")))
            .collect();
        let _ = write!(s, "  map walls {{\n    type ipv4_addr : verdict\n");
        if !entries.is_empty() {
            let _ = write!(s, "    elements = {{ {} }}\n", entries.join(", "));
        }
        s.push_str("  }\n");

        let ports = match self.dns { Some(d) => format!("{{ 53, {} }}", d.port()), None => "53".into() };
        let _ = write!(s, "  # VMs to this host: DNS, and answers to what the host opened.\n  chain input {{\n    type filter hook input priority filter - 5; policy accept;\n    \
            iifname \"cu-*\" ct state established,related accept\n    \
            iifname \"cu-*\" meta l4proto {{ tcp, udp }} th dport {ports} accept\n    \
            iifname \"cu-*\" drop\n  }}\n");
        let _ = write!(s, "  chain forward {{\n    type filter hook forward priority filter - 5; policy accept;\n");
        if let Some(subnet) = self.subnet {
            // Other nodes send only to this node's VMs, never through it.
            let _ = writeln!(s, "    iifname \"{}\" ip daddr != {subnet} drop", mesh::IFACE);
        }
        let _ = write!(s, "    \
            # Into a VM: only from its own project.\n    \
            oifname \"cu-*\" ct state established,related accept\n    \
            oifname \"cu-*\" ip daddr vmap @walls\n    \
            oifname \"cu-*\" drop\n    \
            # Out of a VM: other copper VMs, whose nodes check the walls, and the internet.\n    \
            iifname \"cu-*\" ct state established,related accept\n    \
            iifname \"cu-*\" ip daddr {space} accept\n    \
            iifname \"cu-*\" ip daddr @private drop\n    \
            iifname \"cu-*\" meta nfproto ipv6 drop\n  }}\n");
        // A VM sending to another VM on the same bridge is routed back out
        // the way it came; telling it to go direct would only fail.
        s.push_str("  chain output {\n    type filter hook output priority filter - 5; policy accept;\n    oifname \"cu-*\" icmp type redirect drop\n  }\n");
        if let (Some(dns), Some(gw)) = (self.dns, self.gateway) {
            let _ = write!(s, "  chain prerouting {{\n    type nat hook prerouting priority dstnat; policy accept;\n    \
                iifname \"cu-*\" ip daddr {gw} meta l4proto {{ tcp, udp }} th dport 53 dnat ip to {dns}\n  }}\n");
        }
        let _ = write!(s, "  chain postrouting {{\n    type nat hook postrouting priority srcnat; policy accept;\n    ip saddr {space} ip daddr != {space} masquerade\n  }}\n}}\n");
        s
    }

    /// The same rules for macOS's pf, for the anchor `PF_ANCHOR`. macOS keeps
    /// its own VM network on `bridge` and copper adds its gateway beside it.
    /// What macOS's pf does differently, found on the Mac mini:
    /// - it doesn't count the gateway alias as `(self)`, and never sees the
    ///   Mac's own packets leave through the bridge, so the gateway gets
    ///   explicit rules: DNS, the answers to connections the Mac opened
    ///   (they carry an ACK; a VM's own new connection starts with a bare
    ///   SYN), and nothing else;
    /// - between two VMs on one Mac both directions arrive in on the bridge,
    ///   so a reply never matches its connection's state: walls take any
    ///   flags, and a wall stops both directions;
    /// - Apple's NAT only covers its own range, so copper adds its own.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn render_pf(&self, bridge: &str, uplink: &str, mesh: Option<&str>) -> String {
        let (space, mut s) = (self.space, format!("table <copper_private> const {{ {PRIVATE} }}\n"));
        for (i, w) in self.walls.iter().enumerate() {
            let _ = writeln!(s, "table <project_{i}> const {{ {} }}\ntable <here_{i}> const {{ {} }}", join(&w.members), join(&w.local));
        }
        let (Some(subnet), Some(gw)) = (self.subnet, self.gateway) else {
            // No network yet: nothing to translate, and no VMs to let in.
            return s;
        };
        let _ = writeln!(s, "nat on {uplink} inet from {subnet} to ! {space} -> ({uplink}:0)");
        if let Some(dns) = self.dns {
            let _ = writeln!(s, "rdr pass on {bridge} inet proto {{ tcp, udp }} from {subnet} to {gw} port 53 -> {} port {}", dns.ip(), dns.port());
        }
        let _ = write!(s, "pass in quick on {bridge} inet proto {{ tcp, udp }} from {subnet} to {gw} port 53 keep state\n\
            pass in quick on {bridge} inet proto tcp from {subnet} to {gw} flags A/A no state\n\
            block drop in quick on {bridge} inet from {subnet} to {gw}\n");
        let into = match mesh { Some(m) => format!("{{ {bridge} {m} }}"), None => bridge.to_string() };
        for i in 0..self.walls.len() {
            let _ = writeln!(s, "pass in quick on {into} inet from <project_{i}> to <here_{i}> flags any keep state");
        }
        let _ = writeln!(s, "block drop in quick on {into} inet to {subnet}");
        if let Some(m) = mesh {
            let _ = writeln!(s, "block drop in quick on {m} all");
        }
        let _ = write!(s, "pass in quick on {bridge} inet from {subnet} to {space} flags any keep state\n\
            block drop in quick on {bridge} inet from {subnet} to <copper_private>\n\
            block drop in quick on {bridge} inet6 all\n\
            pass in quick on {bridge} inet from {subnet} to any keep state\n");
        s
    }
}

fn join(ips: &[Ipv4Addr]) -> String {
    ips.iter().map(|ip| ip.to_string()).collect::<Vec<_>>().join(", ")
}

/// ingot's side: write the file only when it changed, so `ingot net` only
/// rebuilds the rules when there's something new.
pub struct Writer {
    path: PathBuf,
    last: Option<NetFile>,
}

impl Writer {
    pub fn new(state_dir: &Path) -> Self {
        Self { path: state_dir.join(FILE), last: None }
    }

    pub fn update(&mut self, file: NetFile) {
        if self.last.as_ref() == Some(&file) {
            return;
        }
        match serde_json::to_vec_pretty(&file).map_err(anyhow::Error::from).and_then(|b| crate::state::write_atomic(&self.path, &b)) {
            Ok(()) => self.last = Some(file),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "writing the network file"),
        }
    }
}

/// `ingot net`: apply the file now and whenever it changes, until stopped.
/// On a Mac, `host` answers the VMs' DNS and ingot's connections to them,
/// from the same file (see reach.rs).
pub fn run(state_dir: &Path, once: bool, host: Option<reach::Host>) -> anyhow::Result<()> {
    let path = state_dir.join(FILE);
    let paths = Paths::from_env();
    let mut mesh = Mesh::open(&paths.keys)?;
    let mut seen: Option<(SystemTime, u64)> = None;
    let mut applied_at = std::time::Instant::now() - REAPPLY_EVERY;
    let mut status = None;
    let mut complained = None;
    let mut gateway = None;
    let mut hosted_dns = None;
    loop {
        // macOS makes its VM bridge afresh whenever a VM starts after none
        // ran, without copper's gateway on it.
        if cfg!(target_os = "macos") {
            if let Some(gw) = gateway {
                if let Err(e) = mac_gateway(gw) {
                    tracing::warn!(error = %format!("{e:#}"), "adding copper's gateway to the VM bridge");
                }
            }
        }
        let stamp = std::fs::metadata(&path).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        // Port 53 is redirected only once the helper's own DNS listens.
        let dns_now = host.as_ref().and_then(|h| h.dns.listening());
        if stamp != seen || dns_now != hosted_dns || applied_at.elapsed() >= REAPPLY_EVERY {
            let plan = match std::fs::read(&path) {
                Ok(bytes) => serde_json::from_slice::<NetFile>(&bytes).map_err(anyhow::Error::from).and_then(|f| {
                    let mut plan = Plan::check(&f)?;
                    if let Some(host) = &host {
                        host.update(&f.network, &f.projects, plan.local_vms());
                        plan.dns = dns_now;
                    }
                    Ok((f.generation, plan))
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((0, Plan::base())),
                Err(e) => Err(e.into()),
            };
            let applied = plan.and_then(|(generation, plan)| {
                gateway = plan.gateway;
                mesh.apply(&plan.peers, plan.gateway)?;
                apply(&plan, &paths.ready, mesh.iface())?;
                Ok(generation)
            });
            match applied {
                Ok(generation) => {
                    if stamp != seen {
                        tracing::info!(generation, "copper network applied");
                    }
                }
                // The old rules stay: nft replaces the table in one transaction or not at all.
                Err(e) if once => return Err(e),
                Err(e) => tracing::error!(error = %format!("{e:#}"), "copper network not applied; keeping the previous rules"),
            }
            seen = stamp;
            hosted_dns = dns_now;
            applied_at = std::time::Instant::now();
        }
        match mesh.tick() {
            Ok(now) => {
                if status.as_ref() != Some(&now) {
                    write_status(&paths.status, &now)?;
                    status = Some(now);
                }
                complained = None;
            }
            Err(e) if once => return Err(e),
            Err(e) => {
                let msg = format!("{e:#}");
                if complained.as_ref() != Some(&msg) {
                    tracing::error!(error = %msg, "checking the mesh");
                    complained = Some(msg);
                }
            }
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(CHECK_EVERY);
    }
}

/// Where `ingot net` keeps things. Tests move them with `COPPER_NET_RUN`
/// (the ready and status files) and `COPPER_NET_KEYS`.
struct Paths {
    ready: PathBuf,
    status: PathBuf,
    keys: PathBuf,
}

impl Paths {
    fn from_env() -> Self {
        let run = std::env::var_os("COPPER_NET_RUN").map(PathBuf::from);
        let file = |default: &str| run.as_ref().map_or_else(|| PathBuf::from(default), |d| d.join(Path::new(default).file_name().unwrap()));
        Self {
            ready: file(READY_FILE),
            status: file(STATUS_FILE),
            keys: std::env::var_os("COPPER_NET_KEYS").map_or_else(|| PathBuf::from(KEY_DIR), PathBuf::from),
        }
    }
}

/// Readable by ingot, which isn't root; it holds only the public key.
fn write_status(path: &Path, status: &mesh::Status) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(status)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn apply(plan: &Plan, ready: &Path, _mesh: &str) -> anyhow::Result<()> {
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1").context("turning on IP forwarding")?;
    pipe("nft", &["-f", "-"], &plan.render())?;
    docker_user(true);
    mark_ready(ready)
}

#[cfg(target_os = "macos")]
fn apply(plan: &Plan, ready: &Path, mesh: &str) -> anyhow::Result<()> {
    pipe("sysctl", &["-w", "net.inet.ip.forwarding=1"], "")?;
    let uplink = mac_uplink()?;
    pipe("pfctl", &["-q", "-a", PF_ANCHOR, "-f", "-"], &plan.render_pf(MAC_BRIDGE, &uplink, Some(mesh)))?;
    // macOS turns pf on for its VM network, but not before a VM runs. -E
    // takes a reference rather than flipping pf on for everyone, once per run.
    static ENABLED: std::sync::Once = std::sync::Once::new();
    let mut enabled = Ok(());
    ENABLED.call_once(|| enabled = pipe("pfctl", &["-q", "-E"], ""));
    enabled?;
    mark_ready(ready)
}

fn mark_ready(ready: &Path) -> anyhow::Result<()> {
    if let Some(dir) = ready.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(ready, b"")?;
    Ok(())
}

/// The interface macOS's default route leaves by: where VM traffic is NATed.
#[cfg(target_os = "macos")]
fn mac_uplink() -> anyhow::Result<String> {
    let out = std::process::Command::new("route").args(["-n", "get", "default"]).output()?;
    String::from_utf8_lossy(&out.stdout).lines()
        .find_map(|l| l.trim().strip_prefix("interface:").map(|i| i.trim().to_string()))
        .filter(|i| i.bytes().all(|b| b.is_ascii_alphanumeric()))
        .context("no default route")
}

/// Copper's gateway beside macOS's own address on its VM bridge, once that
/// exists, so VMs reach the Mac at their gateway and the Mac reaches the
/// /24 through the bridge.
fn mac_gateway(gw: Ipv4Addr) -> anyhow::Result<()> {
    let out = std::process::Command::new("ifconfig").arg(MAC_BRIDGE).output()?;
    if !out.status.success() || String::from_utf8_lossy(&out.stdout).contains(&format!("inet {gw} ")) {
        return Ok(()); // no VM bridge yet, or already there
    }
    pipe("ifconfig", &[MAC_BRIDGE, "alias", &gw.to_string(), "netmask", "255.255.255.0"], "")?;
    tracing::info!(bridge = MAC_BRIDGE, %gw, "added copper's gateway to the VM bridge");
    Ok(())
}

/// `ingot net --stop`: take copper's rules and the mesh out again. The key
/// stays, so the node keeps its identity in the mesh.
pub fn stop() -> anyhow::Result<()> {
    let paths = Paths::from_env();
    let _ = std::fs::remove_file(&paths.ready);
    let _ = std::fs::remove_file(&paths.status);
    let quiet = |cmd: &str, args: &[&str]| {
        let _ = std::process::Command::new(cmd).args(args).stderr(std::process::Stdio::null()).status();
    };
    if cfg!(target_os = "macos") {
        quiet("pfctl", &["-q", "-a", PF_ANCHOR, "-F", "all"]);
        let _ = std::fs::remove_file(reach::SOCKET);
    } else {
        quiet("nft", &["delete", "table", "inet", "copper"]);
        docker_user(false);
    }
    mesh::remove();
    Ok(())
}

/// Docker drops forwarded traffic it doesn't know about; let copper's
/// through its `DOCKER-USER` chain, when there is one.
fn docker_user(add: bool) {
    let iptables = |args: &[&str]| std::process::Command::new("iptables").args(args)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
    if !iptables(&["-n", "-L", "DOCKER-USER"]) {
        return;
    }
    for dir in ["-i", "-o"] {
        let rule = [dir, "cu-+", "-j", "ACCEPT"];
        if add {
            if !iptables(&[&["-C", "DOCKER-USER"][..], &rule].concat()) {
                iptables(&[&["-I", "DOCKER-USER"][..], &rule].concat());
            }
        } else {
            while iptables(&[&["-D", "DOCKER-USER"][..], &rule].concat()) {}
        }
    }
}

fn pipe(cmd: &str, args: &[&str], input: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new(cmd).args(args)
        .stdin(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
        .spawn().with_context(|| format!("running {cmd}"))?;
    child.stdin.take().unwrap().write_all(input.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{cmd}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use copper_api::VmAddress;
    use uuid::Uuid;

    fn net() -> NodeNetwork {
        NodeNetwork { subnet: "10.42.1.0/24".into(), gateway: "10.42.1.1".into(), space: "10.42.0.0/16".into() }
    }
    fn project(name: &str, vms: &[(&str, &str)]) -> ProjectNetwork {
        ProjectNetwork { id: Uuid::new_v4(), name: name.into(), vms: vms.iter().map(|(n, a)| VmAddress { name: n.to_string(), address: a.to_string() }).collect() }
    }
    fn file(projects: Vec<ProjectNetwork>) -> NetFile {
        NetFile { generation: 3, network: net(), projects, dns: None, peers: vec![] }
    }
    fn peer(subnet: &str, key: char) -> MeshPeer {
        MeshPeer { node: Uuid::new_v4(), hostname: format!("n-{subnet}"), public_key: format!("{}=", key.to_string().repeat(43)), subnet: subnet.into(),
            endpoints: vec![copper_api::Endpoint { address: "192.168.68.113:51820".into(), via: copper_api::Via::Lan },
                            copper_api::Endpoint { address: "100.69.49.52:51820".into(), via: copper_api::Via::Tailnet }] }
    }

    #[test]
    fn other_nodes_send_only_to_this_nodes_vms() {
        let rules = Plan::check(&NetFile { peers: vec![peer("10.42.3.0/24", 'B')], ..file(vec![]) }).unwrap().render();
        assert!(rules.contains("iifname \"copper-wg\" ip daddr != 10.42.1.0/24 drop"), "{rules}");
        // The mesh isn't a cu-* interface, so the walls don't stop traffic leaving through it.
        assert!(!mesh::IFACE.starts_with("cu-"));
    }

    #[test]
    fn peers_are_checked_like_everything_else() {
        let ok = Plan::check(&NetFile { peers: vec![peer("10.42.3.0/24", 'B'), peer("10.42.4.0/24", 'C')], ..file(vec![]) }).unwrap();
        assert_eq!(ok.peers[0].endpoints[0].1, copper_api::Via::Lan, "best first, as kiln sent them");
        for bad in [
            vec![peer("10.42.1.0/24", 'B')],
            vec![peer("192.168.68.0/24", 'B')],
            vec![peer("10.42.3.0/24", 'B'), peer("10.42.3.0/24", 'C')],
            vec![MeshPeer { public_key: "nope".into(), ..peer("10.42.3.0/24", 'B') }],
            vec![MeshPeer { endpoints: vec![copper_api::Endpoint { address: "192.168.68.113".into(), via: copper_api::Via::Lan }], ..peer("10.42.3.0/24", 'B') }],
        ] {
            assert!(Plan::check(&NetFile { peers: bad.clone(), ..file(vec![]) }).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn each_local_vm_takes_traffic_only_from_its_own_project() {
        let lab = project("lab", &[("k3s-1", "10.42.1.10"), ("k3s-2", "10.42.3.10")]);
        let home = project("home", &[("backup", "10.42.1.11")]);
        let away = project("away", &[("db", "10.42.4.11")]);
        let rules = Plan::check(&file(vec![lab, home, away])).unwrap().render();
        assert!(rules.contains("set project_0 {\n    type ipv4_addr\n    elements = { 10.42.1.10, 10.42.3.10 }"), "{rules}");
        assert!(rules.contains("set project_1 {\n    type ipv4_addr\n    elements = { 10.42.1.11 }"));
        assert!(rules.contains("elements = { 10.42.1.10 : jump project_0, 10.42.1.11 : jump project_1 }"));
        assert!(!rules.contains("10.42.4.11"), "a project with no VM here needs no wall");
        assert!(rules.contains("oifname \"cu-*\" ip daddr vmap @walls\n    oifname \"cu-*\" drop"));
        // Walls come before anything that would let VM traffic through.
        assert!(rules.find("ip daddr vmap @walls").unwrap() < rules.find("ip daddr 10.42.0.0/16 accept").unwrap());
        assert!(rules.contains("ip saddr 10.42.0.0/16 ip daddr != 10.42.0.0/16 masquerade"));
        assert!(!rules.contains("prerouting"), "no DNS redirect until ingot answers");
    }

    #[test]
    fn dns_goes_to_ingot_once_it_answers() {
        let mut f = file(vec![]);
        f.dns = Some("10.42.1.1:5300".parse().unwrap());
        let rules = Plan::check(&f).unwrap().render();
        assert!(rules.contains("iifname \"cu-*\" ip daddr 10.42.1.1 meta l4proto { tcp, udp } th dport 53 dnat ip to 10.42.1.1:5300"), "{rules}");
        assert!(rules.contains("th dport { 53, 5300 } accept"));
        f.dns = Some("10.42.1.7:5300".parse().unwrap());
        assert!(Plan::check(&f).is_err(), "only on the gateway");
        f.dns = Some("10.42.1.1:22".parse().unwrap());
        assert!(Plan::check(&f).is_err(), "never a privileged port of the host's");
    }

    #[test]
    fn nothing_outside_coppers_range_reaches_the_rules() {
        for bad in [
            file(vec![project("x", &[("a", "192.168.68.1")])]),
            file(vec![project("x", &[("a", "10.42.1.10; flush ruleset")])]),
            file(vec![project("x", &[("a", "10.42.1.10")]), project("y", &[("b", "10.42.1.10")])]),
            NetFile { network: NodeNetwork { space: "8.8.0.0/16".into(), ..net() }, ..file(vec![]) },
            NetFile { network: NodeNetwork { subnet: "10.43.1.0/24".into(), gateway: "10.43.1.1".into(), ..net() }, ..file(vec![]) },
            NetFile { network: NodeNetwork { gateway: "10.42.1.9".into(), ..net() }, ..file(vec![]) },
        ] {
            assert!(Plan::check(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn on_a_mac_the_same_walls_go_into_pf() {
        let mac = NodeNetwork { subnet: "10.42.3.0/24".into(), gateway: "10.42.3.1".into(), space: "10.42.0.0/16".into() };
        let lab = project("lab", &[("k3s-1", "10.42.1.10"), ("k3s-2", "10.42.3.10")]);
        let home = project("home", &[("backup", "10.42.3.11")]);
        let f = NetFile { network: mac, dns: Some("10.42.3.1:5300".parse().unwrap()), ..file(vec![lab, home]) };
        let rules = Plan::check(&f).unwrap().render_pf("bridge100", "en0", Some("utun4"));
        for want in [
            "table <project_0> const { 10.42.1.10, 10.42.3.10 }\ntable <here_0> const { 10.42.3.10 }",
            "nat on en0 inet from 10.42.3.0/24 to ! 10.42.0.0/16 -> (en0:0)",
            "rdr pass on bridge100 inet proto { tcp, udp } from 10.42.3.0/24 to 10.42.3.1 port 53 -> 10.42.3.1 port 5300",
            "pass in quick on bridge100 inet proto tcp from 10.42.3.0/24 to 10.42.3.1 flags A/A no state",
            "pass in quick on { bridge100 utun4 } inet from <project_1> to <here_1> flags any keep state",
            "block drop in quick on { bridge100 utun4 } inet to 10.42.3.0/24",
            "block drop in quick on utun4 all",
        ] {
            assert!(rules.contains(want), "{want}\n---\n{rules}");
        }
        // pf wants tables, then translation, then filtering.
        let at = |s: &str| rules.find(s).unwrap();
        assert!(at("table <here_1>") < at("nat on") && at("rdr pass") < at("pass in quick"));
        // The gateway's own rules come before anything that would let a VM at it.
        assert!(at("to 10.42.3.1 flags A/A") < at("from <project_0>") && at("block drop in quick on bridge100 inet from 10.42.3.0/24 to 10.42.3.1") < at("to 10.42.0.0/16"));
    }

    #[test]
    fn with_no_file_yet_vms_get_nat_and_no_way_in() {
        let rules = Plan::base().render();
        assert!(rules.contains("map walls {\n    type ipv4_addr : verdict\n  }"), "{rules}");
        assert!(rules.contains("masquerade"));
    }
}
