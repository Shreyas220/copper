//! The host side of copper's VM network. The unprivileged ingot writes what
//! the node's network should be to `<state dir>/network.json`; `ingot net`,
//! run as root by copper-net.service, checks it and turns it into one
//! nftables table, replaced atomically whenever the file changes:
//!
//! - VMs reach the internet, through NAT as the node, and other copper VMs.
//! - Traffic into a VM is let in only from VMs in the same project (the
//!   walls). VMs reach each other only through the host: their addresses are
//!   /32s and their bridge ports are isolated.
//! - Nothing else private: not the home LAN, the tailnet, link-local, or the
//!   node itself except DNS, which is redirected to ingot when it answers.
//!
//! Every rule matches only `cu-*` interfaces or copper's range, so the host's
//! own traffic, ssh and Tailscale included, is never touched. The file comes
//! from a less privileged process, so nothing in it reaches the ruleset
//! except addresses that parse and lie inside copper's range.

use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, ensure, Context};
use copper_api::{Generation, NetworkMap, NodeNetwork, ProjectNetwork};
use serde::{Deserialize, Serialize};

pub const FILE: &str = "network.json";
/// Exists while copper's rules are in place; ingot reports it as
/// `network_ready`.
pub const READY_FILE: &str = "/run/copper/net-ready";
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
}

impl NetFile {
    pub fn new(generation: Generation, network: NodeNetwork, map: &NetworkMap, dns: Option<SocketAddrV4>) -> Self {
        Self { generation, network, projects: map.projects.clone(), dns }
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
    gateway: Option<Ipv4Addr>,
    walls: Vec<Wall>,
    dns: Option<SocketAddrV4>,
}

impl Plan {
    /// Before ingot has written a file: NAT and nothing private, no walls
    /// yet, so nothing gets into a VM that it didn't ask for.
    pub fn base() -> Self {
        Self { space: Range::parse(DEFAULT_SPACE, 16).unwrap(), gateway: None, walls: vec![], dns: None }
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
        Ok(Self { space, gateway: Some(gateway), walls, dns: file.dns })
    }

    /// The nftables script: declares the table first so deleting it is safe
    /// on a fresh boot, then replaces it whole, in one transaction.
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
        let _ = write!(s, "  chain forward {{\n    type filter hook forward priority filter - 5; policy accept;\n    \
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
pub fn run(state_dir: &Path, once: bool) -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        bail!("ingot net only knows nftables so far; macOS nodes keep macOS's own VM network");
    }
    let path = state_dir.join(FILE);
    let ready = ready_file();
    let mut seen: Option<(SystemTime, u64)> = None;
    let mut applied_at = std::time::Instant::now() - REAPPLY_EVERY;
    loop {
        let stamp = std::fs::metadata(&path).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        if stamp != seen || applied_at.elapsed() >= REAPPLY_EVERY {
            let plan = match std::fs::read(&path) {
                Ok(bytes) => serde_json::from_slice::<NetFile>(&bytes).map_err(anyhow::Error::from).and_then(|f| Plan::check(&f).map(|p| (f.generation, p))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((0, Plan::base())),
                Err(e) => Err(e.into()),
            };
            match plan.and_then(|(generation, plan)| apply(&plan, &ready).map(|()| generation)) {
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
            applied_at = std::time::Instant::now();
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(CHECK_EVERY);
    }
}

fn ready_file() -> PathBuf {
    std::env::var_os("COPPER_NET_READY").map(PathBuf::from).unwrap_or_else(|| READY_FILE.into())
}

fn apply(plan: &Plan, ready: &Path) -> anyhow::Result<()> {
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1").context("turning on IP forwarding")?;
    pipe("nft", &["-f", "-"], &plan.render())?;
    docker_user(true);
    if let Some(dir) = ready.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(ready, b"")?;
    Ok(())
}

/// `ingot net --stop`: take copper's rules out again.
pub fn stop() -> anyhow::Result<()> {
    let _ = std::fs::remove_file(ready_file());
    let _ = std::process::Command::new("nft").args(["delete", "table", "inet", "copper"]).status();
    docker_user(false);
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
        NetFile { generation: 3, network: net(), projects, dns: None }
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
    fn with_no_file_yet_vms_get_nat_and_no_way_in() {
        let rules = Plan::base().render();
        assert!(rules.contains("map walls {\n    type ipv4_addr : verdict\n  }"), "{rules}");
        assert!(rules.contains("masquerade"));
    }
}
