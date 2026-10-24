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
}

#[derive(Debug, PartialEq)]
enum Step {
    Stay,
    /// Configure this endpoint afresh, so it has to complete a handshake.
    Try(usize),
}

impl PathChoice {
    fn new(now: Instant, now_unix: u64) -> Self {
        Self { current: 0, since_unix: now_unix, since: now, settled: false }
    }

    fn step(&mut self, endpoints: usize, handshake: Option<u64>, now: Instant, now_unix: u64) -> Step {
        if endpoints == 0 {
            return Step::Stay;
        }
        let next = |p: &mut Self, i: usize| {
            *p = Self { current: i, ..Self::new(now, now_unix) };
            Step::Try(i)
        };
        if !self.settled {
            if handshake.is_some_and(|t| t >= self.since_unix) {
                self.settled = true;
                Step::Stay
            } else if now.duration_since(self.since) >= TRY_FOR {
                next(self, (self.current + 1) % endpoints)
            } else {
                Step::Stay
            }
        } else if handshake.is_none_or(|t| now_unix.saturating_sub(t) > STALE_AFTER) {
            next(self, 0)
        } else {
            Step::Stay
        }
    }
}

pub struct Mesh {
    key_file: PathBuf,
    public_key: String,
    peers: HashMap<String, (Peer, PathChoice)>,
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
        let mesh = Self { key_file, public_key, peers: HashMap::new() };
        mesh.ensure_iface()?;
        Ok(mesh)
    }

    fn ensure_iface(&self) -> anyhow::Result<()> {
        if !Path::new("/sys/class/net").join(IFACE).exists() {
            run("ip", &["link", "add", IFACE, "type", "wireguard"], None).context("is the wireguard kernel module there?")?;
        }
        let key = self.key_file.to_string_lossy();
        run("wg", &["set", IFACE, "listen-port", &PORT.to_string(), "private-key", &key], None)?;
        run("ip", &["link", "set", IFACE, "mtu", &crate::vmnet::MTU.to_string(), "up"], None)?;
        Ok(())
    }

    /// Bring the interface's peers and routes in line with `wanted`. A new
    /// peer, or one whose endpoints changed, starts over from its first.
    pub fn apply(&mut self, wanted: &[Peer]) -> anyhow::Result<()> {
        self.ensure_iface()?;
        let (now, now_unix) = (Instant::now(), unix_now());
        let present = dump()?;
        for key in present.keys().filter(|k| !wanted.iter().any(|p| &&p.public_key == k)) {
            run("wg", &["set", IFACE, "peer", key, "remove"], None)?;
        }
        self.peers.retain(|k, _| wanted.iter().any(|p| &p.public_key == k));
        for peer in wanted {
            let same = self.peers.get(&peer.public_key).is_some_and(|(p, _)| p == peer) && present.contains_key(&peer.public_key);
            if !same {
                self.peers.insert(peer.public_key.clone(), (peer.clone(), PathChoice::new(now, now_unix)));
                set_peer(peer, 0)?;
            }
        }
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

    /// Move peers whose path isn't working on to their next endpoint, and
    /// say where each one stands.
    pub fn tick(&mut self) -> anyhow::Result<Status> {
        let (now, now_unix) = (Instant::now(), unix_now());
        let present = dump()?;
        let mut peers = vec![];
        for (key, (peer, path)) in &mut self.peers {
            let (endpoint, handshake) = present.get(key).cloned().unwrap_or((None, None));
            if let Step::Try(i) = path.step(peer.endpoints.len(), handshake, now, now_unix) {
                tracing::info!(peer = %key, endpoint = %peer.endpoints[i].0, "trying the next path to a peer");
                set_peer(peer, i)?;
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
fn set_peer(peer: &Peer, i: usize) -> anyhow::Result<()> {
    let _ = run("wg", &["set", IFACE, "peer", &peer.public_key, "remove"], None);
    let mut args = vec!["set", IFACE, "peer", &peer.public_key, "allowed-ips", &peer.subnet, "persistent-keepalive", KEEPALIVE];
    let endpoint = peer.endpoints.get(i).map(|(e, _)| e.to_string());
    if let Some(e) = &endpoint {
        args.extend(["endpoint", e]);
    }
    run("wg", &args, None)?;
    Ok(())
}

/// `wg show copper-wg dump`: each peer's current endpoint and last handshake.
fn dump() -> anyhow::Result<HashMap<String, (Option<SocketAddrV4>, Option<u64>)>> {
    Ok(parse_dump(&run("wg", &["show", IFACE, "dump"], None)?))
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
    let _ = run("ip", &["link", "del", IFACE], None);
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

    #[test]
    fn each_endpoint_gets_its_chance_until_one_answers() {
        let t0 = Instant::now();
        let mut p = PathChoice::new(t0, 1000);
        assert_eq!(p.step(2, None, t0 + Duration::from_secs(3), 1003), Step::Stay, "still waiting on the LAN");
        assert_eq!(p.step(2, None, t0 + Duration::from_secs(9), 1009), Step::Try(1), "the LAN didn't answer: the tailnet");
        // An old handshake from before the switch doesn't count.
        assert_eq!(p.step(2, Some(1004), t0 + Duration::from_secs(12), 1012), Step::Stay);
        assert!(!p.settled);
        assert_eq!(p.step(2, Some(1013), t0 + Duration::from_secs(14), 1014), Step::Stay);
        assert!(p.settled);
        assert_eq!(p.current, 1);
        // It stays while handshakes keep coming, and starts over from the LAN when they stop.
        assert_eq!(p.step(2, Some(1130), t0 + Duration::from_secs(150), 1150), Step::Stay);
        assert_eq!(p.step(2, Some(1130), t0 + Duration::from_secs(400), 1400), Step::Try(0));
        assert_eq!(p.step(2, None, t0 + Duration::from_secs(409), 1409), Step::Try(1));
        assert_eq!(p.step(2, None, t0 + Duration::from_secs(418), 1418), Step::Try(0), "and round again until something answers");
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
