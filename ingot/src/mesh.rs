//! The mesh between nodes: one WireGuard interface, `copper-wg`, with every
//! other approved node as a peer, each allowed exactly its /24. Run by
//! `ingot net` as root (see net.rs); the key is made here and its private
//! half never leaves this machine.
//!
//! Path: a peer's endpoints come best first, LAN addresses before its tailnet
//! address. Each is given `TRY_FOR` to complete a handshake before the next
//! is tried, and a handshake only completes with the real peer's key, so a
//! wrong guess (another site using the same LAN range) just moves on. Once
//! one works it stays until its handshakes go stale. A node that moves
//! reports new endpoints, which starts its peers over from the LAN; and
//! WireGuard itself follows a peer whose packets arrive from a new address.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use copper_api::Via;
use serde::{Deserialize, Serialize};

pub const IFACE: &str = "copper-wg";
pub const PORT: u16 = 51820;
const KEEPALIVE: &str = "25";
/// WireGuard retries a handshake every 5 s; this gives each endpoint one retry.
const TRY_FOR: Duration = Duration::from_secs(8);
/// WireGuard rekeys every 2 minutes while packets flow, and keepalives make
/// sure they do. A handshake older than this means the path broke.
const STALE_AFTER: u64 = 200;
/// On a worse path than the first, check this often whether the first
/// answers a ping now; each check that doesn't pan out doubles the wait.
const PROBE_FIRST: Duration = Duration::from_secs(30);
const PROBE_MAX: Duration = Duration::from_secs(30 * 60);

/// What `ingot net` tells ingot about the mesh, in `<run dir>/net-status.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub public_key: String,
    pub port: u16,
    #[serde(default)]
    pub peers: Vec<PeerStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub public_key: String,
    /// The path of the last handshake, while it's fresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_handshake: Option<u64>,
}

/// A peer from the network file, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    pub public_key: String,
    pub subnet: String,
    pub endpoints: Vec<(SocketAddrV4, Via)>,
}

pub fn valid_key(key: &str) -> bool {
    key.len() == 44 && key.ends_with('=') && key[..43].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// Which endpoint a peer is on, and since when.
#[derive(Debug, Clone, PartialEq)]
struct PathChoice {
    current: usize,
    /// When `current` was configured, as unix seconds and as an instant.
    since_unix: u64,
    since: Instant,
    /// A handshake has completed on `current`.
    settled: bool,
    /// While giving the first endpoint another go: the one that worked, to
    /// go straight back to if the first still doesn't complete a handshake.
    back_to: Option<usize>,
    next_probe: Instant,
    probe_every: Duration,
}

#[derive(Debug, PartialEq)]
enum Step {
    Stay,
    /// Configure this endpoint afresh, so it has to complete a handshake.
    Try(usize),
    /// See whether the first endpoint answers a ping; `answered` says.
    Probe,
}

impl PathChoice {
    fn new(now: Instant, now_unix: u64) -> Self {
        Self { current: 0, since_unix: now_unix, since: now, settled: false, back_to: None, next_probe: now + PROBE_FIRST, probe_every: PROBE_FIRST }
    }

    /// Switch to endpoint `i`, unsettled, keeping the probing schedule.
    fn switch(&mut self, i: usize, now: Instant, now_unix: u64) -> Step {
        *self = Self { current: i, since_unix: now_unix, since: now, settled: false, ..self.clone() };
        Step::Try(i)
    }

    fn backoff(&mut self, now: Instant) {
        self.probe_every = (self.probe_every * 2).min(PROBE_MAX);
        self.next_probe = now + self.probe_every;
    }

    fn step(&mut self, endpoints: usize, handshake: Option<u64>, now: Instant, now_unix: u64) -> Step {
        if endpoints == 0 {
            return Step::Stay;
        }
        if !self.settled {
            if handshake.is_some_and(|t| t >= self.since_unix) {
                self.settled = true;
                if self.back_to.take().is_some() || self.current == 0 {
                    self.probe_every = PROBE_FIRST;
                }
                Step::Stay
            } else if now.duration_since(self.since) >= TRY_FOR {
                match self.back_to.take() {
                    // The first still doesn't answer: back to what worked, and wait longer next time.
                    Some(back) => {
                        self.backoff(now);
                        self.switch(back, now, now_unix)
                    }
                    None => self.switch((self.current + 1) % endpoints, now, now_unix),
                }
            } else {
                Step::Stay
            }
        } else if handshake.is_none_or(|t| now_unix.saturating_sub(t) > STALE_AFTER) {
            self.back_to = None;
            self.switch(0, now, now_unix)
        } else if self.current > 0 && now >= self.next_probe {
            Step::Probe
        } else {
            Step::Stay
        }
    }

    /// The result of a `Probe`. Only a reply is worth disturbing a working
    /// path for, and only a handshake on the first then settles it.
    fn answered(&mut self, reply: bool, now: Instant, now_unix: u64) -> Step {
        if reply {
            self.back_to = Some(self.current);
            self.switch(0, now, now_unix)
        } else {
            self.backoff(now);
            Step::Stay
        }
    }
}

pub struct Mesh {
    key_file: PathBuf,
    public_key: String,
    peers: HashMap<String, (Peer, PathChoice)>,
    iface: String,
    /// wireguard-go, on macOS.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    daemon: Option<std::process::Child>,
    /// The routes made through the mesh, on macOS.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    routes: std::collections::HashSet<String>,
}

impl Mesh {
    /// The node's key, made once and kept, and the interface listening.
    pub fn open(key_dir: &Path) -> anyhow::Result<Self> {
        let key_file = key_dir.join("private.key");
        if !key_file.exists() {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(key_dir)?;
            let key = run("wg", &["genkey"], None)?;
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&key_file)?;
            std::io::Write::write_all(&mut f, key.as_bytes())?;
            tracing::info!(file = %key_file.display(), "made the node's WireGuard key");
        }
        let private = std::fs::read_to_string(&key_file)?;
        let public_key = run("wg", &["pubkey"], Some(&private))?.trim().to_string();
        if !valid_key(&public_key) {
            bail!("wg pubkey gave {public_key:?}");
        }
        let mut mesh = Self { key_file, public_key, peers: HashMap::new(), iface: IFACE.into(), daemon: None, routes: Default::default() };
        mesh.ensure_iface(None)?;
        Ok(mesh)
    }

    /// The mesh's interface: `copper-wg` on Linux, whichever `utun`
    /// wireguard-go was given on macOS.
    pub fn iface(&self) -> &str {
        &self.iface
    }

    #[cfg(not(target_os = "macos"))]
    fn ensure_iface(&mut self, _gateway: Option<Ipv4Addr>) -> anyhow::Result<()> {
        if !Path::new("/sys/class/net").join(IFACE).exists() {
            run("ip", &["link", "add", IFACE, "type", "wireguard"], None).context("is the wireguard kernel module there?")?;
        }
        let key = self.key_file.to_string_lossy();
        run("wg", &["set", IFACE, "listen-port", &PORT.to_string(), "private-key", &key], None)?;
        run("ip", &["link", "set", IFACE, "mtu", &crate::vmnet::MTU.to_string(), "up"], None)?;
        Ok(())
    }

    /// macOS has no WireGuard in its kernel: wireguard-go runs as this
    /// process's child and makes a `utun`. The gateway goes on it as a
    /// point-to-point address, so the Mac has a source for the mesh.
    #[cfg(target_os = "macos")]
    fn ensure_iface(&mut self, gateway: Option<Ipv4Addr>) -> anyhow::Result<()> {
        let alive = self.daemon.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        if !alive {
            // One left by an earlier run would hold the port; its socket going makes it exit.
            stop_wireguard_go();
            std::fs::create_dir_all("/var/run/wireguard")?;
            let child = std::process::Command::new("wireguard-go").args(["-f", "utun"]).env("WG_TUN_NAME_FILE", NAME_FILE)
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
                .spawn().context("starting wireguard-go")?;
            let name = (0..50)
                .find_map(|_| {
                    std::thread::sleep(Duration::from_millis(100));
                    std::fs::read_to_string(NAME_FILE).ok().map(|s| s.trim().to_string()).filter(|s| s.starts_with("utun"))
                })
                .context("wireguard-go gave no interface name")?;
            tracing::info!(iface = %name, "mesh interface up");
            (self.iface, self.daemon) = (name, Some(child));
            self.routes.clear();
        }
        let key = self.key_file.to_string_lossy();
        run("wg", &["set", &self.iface, "listen-port", &PORT.to_string(), "private-key", &key], None)?;
        if let Some(gw) = gateway {
            let gw = gw.to_string();
            run("ifconfig", &[&self.iface, "inet", &gw, &gw, "netmask", "255.255.255.255", "mtu", &crate::vmnet::MTU.to_string(), "up"], None)?;
        }
        Ok(())
    }

    /// Bring the interface's peers and routes in line with `wanted`. A new
    /// peer, or one whose endpoints changed, starts over from its first.
    pub fn apply(&mut self, wanted: &[Peer], gateway: Option<Ipv4Addr>) -> anyhow::Result<()> {
        self.ensure_iface(gateway)?;
        let (now, now_unix) = (Instant::now(), unix_now());
        let present = dump(&self.iface)?;
        for key in present.keys().filter(|k| !wanted.iter().any(|p| &&p.public_key == k)) {
            run("wg", &["set", &self.iface, "peer", key, "remove"], None)?;
        }
        self.peers.retain(|k, _| wanted.iter().any(|p| &p.public_key == k));
        for peer in wanted {
            let same = self.peers.get(&peer.public_key).is_some_and(|(p, _)| p == peer) && present.contains_key(&peer.public_key);
            if !same {
                self.peers.insert(peer.public_key.clone(), (peer.clone(), PathChoice::new(now, now_unix)));
                set_peer(&self.iface, peer, 0)?;
            }
        }
        self.sync_routes(wanted)
    }

    #[cfg(not(target_os = "macos"))]
    fn sync_routes(&mut self, wanted: &[Peer]) -> anyhow::Result<()> {
        let routes = run("ip", &["-4", "route", "show", "dev", IFACE], None)?;
        for line in routes.lines() {
            let dest = line.split_whitespace().next().unwrap_or_default();
            if !wanted.iter().any(|p| p.subnet == dest) {
                run("ip", &["route", "del", dest, "dev", IFACE], None)?;
            }
        }
        for peer in wanted {
            run("ip", &["route", "replace", &peer.subnet, "dev", IFACE], None)?;
        }
        Ok(())
    }

    /// macOS routes have no `replace`; the ones made here are remembered, and
    /// all of them go with the interface when wireguard-go restarts.
    #[cfg(target_os = "macos")]
    fn sync_routes(&mut self, wanted: &[Peer]) -> anyhow::Result<()> {
        for subnet in self.routes.clone() {
            if !wanted.iter().any(|p| p.subnet == subnet) {
                let _ = run("route", &["-n", "delete", "-net", &subnet, "-interface", &self.iface], None);
                self.routes.remove(&subnet);
            }
        }
        for peer in wanted {
            if !self.routes.contains(&peer.subnet) {
                let _ = run("route", &["-n", "delete", "-net", &peer.subnet], None);
                run("route", &["-n", "add", "-net", &peer.subnet, "-interface", &self.iface], None)?;
                self.routes.insert(peer.subnet.clone());
            }
        }
        Ok(())
    }

    /// Move peers whose path isn't working on to their next endpoint, and
    /// say where each one stands.
    pub fn tick(&mut self) -> anyhow::Result<Status> {
        let (now, now_unix) = (Instant::now(), unix_now());
        let present = dump(&self.iface)?;
        let mut peers = vec![];
        for (key, (peer, path)) in &mut self.peers {
            let (endpoint, handshake) = present.get(key).cloned().unwrap_or((None, None));
            let mut step = path.step(peer.endpoints.len(), handshake, now, now_unix);
            if step == Step::Probe {
                let first = peer.endpoints[0].0;
                step = path.answered(pings(*first.ip()), now, now_unix);
            }
            if let Step::Try(i) = step {
                tracing::info!(peer = %key, endpoint = %peer.endpoints[i].0, "trying another path to a peer");
                set_peer(&self.iface, peer, i)?;
            }
            let fresh = handshake.is_some_and(|t| now_unix.saturating_sub(t) <= STALE_AFTER);
            let via = endpoint.filter(|_| fresh).map(|e| if tailnet(*e.ip()) { Via::Tailnet } else { Via::Lan });
            peers.push(PeerStatus { public_key: key.clone(), via, latest_handshake: handshake });
        }
        peers.sort_by(|a, b| a.public_key.cmp(&b.public_key));
        Ok(Status { public_key: self.public_key.clone(), port: PORT, peers })
    }
}

/// Configure a peer on its `i`th endpoint afresh: removing it first drops
/// any session, so the new endpoint has to complete a handshake of its own.
fn set_peer(iface: &str, peer: &Peer, i: usize) -> anyhow::Result<()> {
    let _ = run("wg", &["set", iface, "peer", &peer.public_key, "remove"], None);
    let mut args = vec!["set", iface, "peer", &peer.public_key, "allowed-ips", &peer.subnet, "persistent-keepalive", KEEPALIVE];
    let endpoint = peer.endpoints.get(i).map(|(e, _)| e.to_string());
    if let Some(e) = &endpoint {
        args.extend(["endpoint", e]);
    }
    run("wg", &args, None)?;
    Ok(())
}

/// Whether `ip` answers one ping within a second. Only a hint: a device at
/// another site with the same address answers too, and then the WireGuard
/// handshake that follows decides.
fn pings(ip: Ipv4Addr) -> bool {
    // A second to answer: `-W 1` on Linux, `-t 1` on macOS, where -W is milliseconds.
    let wait = if cfg!(target_os = "macos") { "-t" } else { "-W" };
    std::process::Command::new("ping").args(["-c", "1", wait, "1", &ip.to_string()])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status().is_ok_and(|s| s.success())
}

/// `wg show <iface> dump`: each peer's current endpoint and last handshake.
fn dump(iface: &str) -> anyhow::Result<HashMap<String, (Option<SocketAddrV4>, Option<u64>)>> {
    Ok(parse_dump(&run("wg", &["show", iface, "dump"], None)?))
}

fn parse_dump(out: &str) -> HashMap<String, (Option<SocketAddrV4>, Option<u64>)> {
    out.lines().skip(1).filter_map(|line| {
        let f: Vec<&str> = line.split('\t').collect();
        let endpoint = f.get(2)?.parse().ok();
        let handshake = f.get(4)?.parse::<u64>().ok().filter(|t| *t > 0);
        Some((f[0].to_string(), (endpoint, handshake)))
    }).collect()
}

/// Where other nodes can reach this one on `port`: its LAN addresses, then
/// its tailnet address. Run by ingot, unprivileged. Copper's own addresses,
/// and Docker's, libvirt's and macOS's VM networks, don't count.
pub fn local_endpoints(port: u16, copper: impl Fn(Ipv4Addr) -> bool) -> Vec<copper_api::Endpoint> {
    const NOT_A_WAY_IN: &[&str] = &["lo", "cu-", "copper-wg", "docker", "br-", "virbr", "veth", "vnet", "bridge", "vmenet"];
    let (mut lan, mut tail) = (vec![], vec![]);
    for (name, ip) in ipv4_addresses() {
        if NOT_A_WAY_IN.iter().any(|p| name.starts_with(p)) || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || copper(ip) {
            continue;
        }
        let e = copper_api::Endpoint { address: SocketAddrV4::new(ip, port).to_string(), via: if tailnet(ip) { Via::Tailnet } else { Via::Lan } };
        if tailnet(ip) { tail.push(e) } else { lan.push(e) }
    }
    lan.truncate(6);
    tail.truncate(2);
    lan.into_iter().chain(tail).collect()
}

fn ipv4_addresses() -> Vec<(String, Ipv4Addr)> {
    let mut out = vec![];
    // SAFETY: getifaddrs hands back a list we only read, then free once.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut at = list;
        while let Some(ifa) = at.as_ref() {
            at = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || i32::from((*ifa.ifa_addr).sa_family) != libc::AF_INET {
                continue;
            }
            let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            out.push((name, Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))));
        }
        libc::freeifaddrs(list);
    }
    out
}

/// Tailscale's range, `100.64.0.0/10`.
pub fn tailnet(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..128).contains(&b)
}

/// `ingot net --stop`: the interface, its peers and routes go.
pub fn remove() {
    if cfg!(target_os = "macos") {
        stop_wireguard_go();
    } else {
        let _ = run("ip", &["link", "del", IFACE], None);
    }
}

/// Where wireguard-go writes the `utun` it was given, on macOS.
const NAME_FILE: &str = "/var/run/wireguard/copper-wg.name";

/// wireguard-go exits once its control socket is gone.
fn stop_wireguard_go() {
    if let Ok(name) = std::fs::read_to_string(NAME_FILE) {
        let name = name.trim();
        if name.starts_with("utun") && name[4..].bytes().all(|b| b.is_ascii_digit()) {
            let _ = std::fs::remove_file(format!("/var/run/wireguard/{name}.sock"));
        }
        let _ = std::fs::remove_file(NAME_FILE);
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn run(cmd: &str, args: &[&str], input: Option<&str>) -> anyhow::Result<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(cmd).args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().with_context(|| format!("running {cmd}"))?;
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seconds after a start at unix time 1000.
    fn at(t0: Instant, s: u64) -> (Instant, u64) {
        (t0 + Duration::from_secs(s), 1000 + s)
    }

    #[test]
    fn each_endpoint_gets_its_chance_until_one_answers() {
        let t0 = Instant::now();
        let step = |p: &mut PathChoice, handshake: Option<u64>, s| { let (t, u) = at(t0, s); p.step(2, handshake, t, u) };
        let mut p = PathChoice::new(t0, 1000);
        assert_eq!(step(&mut p, None, 3), Step::Stay, "still waiting on the LAN");
        assert_eq!(step(&mut p, None, 9), Step::Try(1), "the LAN didn't answer: the tailnet");
        // An old handshake from before the switch doesn't count.
        assert_eq!(step(&mut p, Some(1004), 12), Step::Stay);
        assert!(!p.settled);
        assert_eq!(step(&mut p, Some(1013), 14), Step::Stay);
        assert!(p.settled && p.current == 1);
        assert_eq!(step(&mut p, Some(1018), 20), Step::Stay, "it stays while handshakes keep coming");
        // ...and starts over from the LAN when they stop.
        assert_eq!(step(&mut p, Some(1018), 220), Step::Try(0));
        assert_eq!(step(&mut p, None, 229), Step::Try(1));
        assert_eq!(step(&mut p, None, 238), Step::Try(0), "and round again until something answers");
    }

    #[test]
    fn a_peer_on_the_tailnet_gets_back_to_the_lan_once_the_lan_answers() {
        let t0 = Instant::now();
        let step = |p: &mut PathChoice, handshake: Option<u64>, s| { let (t, u) = at(t0, s); p.step(2, handshake, t, u) };
        let answered = |p: &mut PathChoice, reply: bool, s| { let (t, u) = at(t0, s); p.answered(reply, t, u) };
        // Both nodes started at once and missed each other on the LAN.
        let mut p = PathChoice::new(t0, 1000);
        assert_eq!(step(&mut p, None, 9), Step::Try(1));
        assert_eq!(step(&mut p, Some(1010), 10), Step::Stay);
        assert_eq!(step(&mut p, Some(1030), 31), Step::Probe, "30 s on, see whether the LAN answers");
        assert_eq!(answered(&mut p, false, 31), Step::Stay, "it doesn't: stay, and wait 60 s");
        assert_eq!(step(&mut p, Some(1060), 61), Step::Stay);
        assert_eq!(step(&mut p, Some(1090), 92), Step::Probe);
        // It answers a ping but not WireGuard: straight back, not round the list.
        assert_eq!(answered(&mut p, true, 92), Step::Try(0));
        assert_eq!(step(&mut p, None, 101), Step::Try(1));
        assert_eq!(p.probe_every, Duration::from_secs(120));
        assert_eq!(step(&mut p, Some(1102), 102), Step::Stay);
        // Next time it completes a handshake too: the LAN it is.
        assert_eq!(step(&mut p, Some(1200), 223), Step::Probe);
        assert_eq!(answered(&mut p, true, 223), Step::Try(0));
        assert_eq!(step(&mut p, Some(1224), 224), Step::Stay);
        assert!(p.settled && p.current == 0 && p.probe_every == PROBE_FIRST);
        assert_eq!(step(&mut p, Some(1300), 400), Step::Stay, "nothing better to look for");
    }

    #[test]
    fn this_machine_offers_its_lan_first_and_never_loopback_or_coppers_own() {
        let found = local_endpoints(PORT, |ip| ip.octets()[..2] == [10, 42]);
        println!("{found:?}");
        let ips: Vec<SocketAddrV4> = found.iter().map(|e| e.address.parse().unwrap()).collect();
        assert!(ips.iter().all(|a| !a.ip().is_loopback() && !a.ip().is_link_local() && a.ip().octets()[..2] != [10, 42] && a.port() == PORT));
        let first_tailnet = found.iter().position(|e| e.via == Via::Tailnet).unwrap_or(found.len());
        assert!(found[first_tailnet..].iter().all(|e| e.via == Via::Tailnet), "LAN addresses come first");
    }

    #[test]
    fn a_peer_without_endpoints_waits_to_be_called() {
        let t0 = Instant::now();
        assert_eq!(PathChoice::new(t0, 0).step(0, None, t0 + Duration::from_secs(60), 60), Step::Stay);
    }

    #[test]
    fn the_dump_gives_endpoints_and_handshakes() {
        let out = "PRIVATE\tPUBLIC\t51820\toff\n\
                   AAAA=\t(none)\t192.168.68.113:51820\t10.42.3.0/24\t1792758000\t1000\t2000\t25\n\
                   BBBB=\t(none)\t(none)\t10.42.4.0/24\t0\t0\t0\t25\n";
        let d = parse_dump(out);
        assert_eq!(d["AAAA="], (Some("192.168.68.113:51820".parse().unwrap()), Some(1792758000)));
        assert_eq!(d["BBBB="], (None, None));
        assert!(tailnet("100.69.49.52".parse().unwrap()) && !tailnet("100.128.0.1".parse().unwrap()) && !tailnet("192.168.68.113".parse().unwrap()));
        assert!(valid_key(&format!("{}=", "Ab+/".repeat(10) + "xyz")) && !valid_key("AAAA="));
    }
}
