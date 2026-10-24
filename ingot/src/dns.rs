//! DNS for VMs, one project at a time. Ingot answers on its gateway, port
//! `PORT`; `ingot net` redirects the VMs' port 53 there once it's listening.
//!
//! `<vm>.<project>.copper` resolves to the VM's address, but only for VMs in
//! that project: the asker's own address says which project it's in, and
//! other projects' names don't exist for it. Everything else is passed to the
//! node's own resolver, for VMs only.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use copper_api::{NetworkMap, NodeNetwork};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::watch;

pub const PORT: u16 = 5300;
const ZONE: &str = "copper";
const TTL: u32 = 30;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(3);
const RETRY: Duration = Duration::from_secs(5);

pub struct Dns {
    zone: RwLock<Zone>,
    gateway: watch::Sender<Option<Ipv4Addr>>,
    listening: Mutex<Option<SocketAddrV4>>,
}

impl Dns {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { zone: RwLock::default(), gateway: watch::Sender::new(None), listening: Mutex::default() })
    }

    /// The names VMs can look up, and where to answer, from desired state.
    pub fn update(&self, network: &NodeNetwork, map: &NetworkMap) {
        *self.zone.write().unwrap() = Zone::from_map(map);
        let gateway = network.gateway.parse().ok();
        self.gateway.send_if_modified(|g| std::mem::replace(g, gateway) != gateway);
    }

    /// Where ingot is answering, once it is.
    pub fn listening(&self) -> Option<SocketAddrV4> {
        *self.listening.lock().unwrap()
    }

    /// Answer on the gateway for as long as ingot runs. The gateway address
    /// only exists once the node's copper network is up, so binding is
    /// retried until it does.
    pub async fn serve(self: Arc<Self>) {
        if cfg!(target_os = "macos") {
            return; // macOS nodes keep macOS's own DNS for VMs, for now
        }
        let mut gateway = self.gateway.subscribe();
        let mut complained = None;
        loop {
            let at = gateway.borrow_and_update().map(|gw| SocketAddrV4::new(gw, PORT));
            if let Some(at) = at {
                match (UdpSocket::bind(at).await, TcpListener::bind(at).await) {
                    (Ok(udp), Ok(tcp)) => {
                        *self.listening.lock().unwrap() = Some(at);
                        tracing::info!(%at, "answering DNS for VMs");
                        complained = None;
                        tokio::select! {
                            _ = self.clone().answer_on(udp, tcp) => {}
                            _ = gateway.changed() => {}
                        }
                        *self.listening.lock().unwrap() = None;
                        continue;
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        let msg = e.to_string();
                        if complained.as_ref() != Some(&msg) {
                            tracing::warn!(%at, error = %msg, "can't answer DNS for VMs yet; retrying");
                            complained = Some(msg);
                        }
                    }
                }
            }
            tokio::select! {
                _ = gateway.changed() => {}
                _ = tokio::time::sleep(RETRY) => {}
            }
        }
    }

    async fn answer_on(self: Arc<Self>, udp: UdpSocket, tcp: TcpListener) {
        tokio::join!(self.clone().serve_udp(Arc::new(udp)), self.serve_tcp(tcp));
    }

    async fn serve_udp(self: Arc<Self>, socket: Arc<UdpSocket>) {
        let mut buf = [0u8; 1500];
        loop {
            let Ok((n, peer)) = socket.recv_from(&mut buf).await else { continue };
            let SocketAddr::V4(from) = peer else { continue };
            let (dns, socket, query) = (self.clone(), socket.clone(), buf[..n].to_vec());
            tokio::spawn(async move {
                if let Some(reply) = dns.respond(*from.ip(), &query, Upstream::Udp).await {
                    let _ = socket.send_to(&reply, peer).await;
                }
            });
        }
    }

    async fn serve_tcp(self: Arc<Self>, listener: TcpListener) {
        loop {
            let Ok((mut stream, SocketAddr::V4(from))) = listener.accept().await else { continue };
            let dns = self.clone();
            tokio::spawn(async move {
                while let Some(query) = read_framed(&mut stream).await {
                    let Some(reply) = dns.respond(*from.ip(), &query, Upstream::Tcp).await else { return };
                    if write_framed(&mut stream, &reply).await.is_err() { return }
                }
            });
        }
    }

    async fn respond(&self, from: Ipv4Addr, query: &[u8], via: Upstream) -> Option<Vec<u8>> {
        let local = answer(&self.zone.read().unwrap(), from, query);
        match local {
            Reply::Local(reply) => Some(reply),
            Reply::Forward => Some(match via.forward(query).await {
                Some(reply) => reply,
                None => reply(query, Rcode::ServFail, None)?,
            }),
            Reply::Drop => None,
        }
    }
}

/// Which project each VM is in, and every project's VMs by name.
#[derive(Debug, Default)]
struct Zone {
    project_of: HashMap<Ipv4Addr, usize>,
    projects: Vec<(String, HashMap<String, Ipv4Addr>)>,
}

impl Zone {
    fn from_map(map: &NetworkMap) -> Self {
        let mut zone = Self::default();
        for p in &map.projects {
            let mut vms = HashMap::new();
            for vm in &p.vms {
                let Ok(ip) = vm.address.parse::<Ipv4Addr>() else { continue };
                vms.insert(vm.name.to_ascii_lowercase(), ip);
                zone.project_of.insert(ip, zone.projects.len());
            }
            zone.projects.push((p.name.to_ascii_lowercase(), vms));
        }
        zone
    }

    fn lookup(&self, from: Ipv4Addr, labels: &[String]) -> Lookup {
        let Some(&p) = self.project_of.get(&from) else { return Lookup::Refused };
        let (project, vms) = &self.projects[p];
        match labels {
            [vm, name, zone] if name == project && zone == ZONE => vms.get(vm).map_or(Lookup::NotFound, |ip| Lookup::Found(*ip)),
            [name, zone] if name == project && zone == ZONE => Lookup::NoData,
            [zone] if zone == ZONE => Lookup::NoData,
            _ => Lookup::NotFound,
        }
    }
}

#[derive(Debug, PartialEq)]
enum Lookup {
    Found(Ipv4Addr),
    /// The name exists, with no address of the type asked for.
    NoData,
    NotFound,
    /// Not a VM ingot knows.
    Refused,
}

#[derive(Debug, PartialEq)]
enum Reply {
    Local(Vec<u8>),
    Forward,
    Drop,
}

#[derive(Clone, Copy)]
enum Rcode {
    NoError = 0,
    FormErr = 1,
    ServFail = 2,
    NxDomain = 3,
    Refused = 5,
}

const TYPE_A: u16 = 1;
const TYPE_ANY: u16 = 255;

/// The one question of a query: its labels, lowercase, its type, and where
/// the question section ends.
fn question(query: &[u8]) -> Option<(Vec<String>, u16, usize)> {
    if query.len() < 12 || u16::from_be_bytes([query[4], query[5]]) != 1 {
        return None;
    }
    let mut labels = vec![];
    let mut at = 12;
    loop {
        let len = *query.get(at)? as usize;
        at += 1;
        if len == 0 {
            break;
        }
        // Queries don't compress their one name; anything else is refused as malformed.
        if len > 63 {
            return None;
        }
        labels.push(String::from_utf8_lossy(query.get(at..at + len)?).to_ascii_lowercase());
        at += len;
    }
    let qtype = u16::from_be_bytes([*query.get(at)?, *query.get(at + 1)?]);
    let end = at + 4;
    (end <= query.len()).then_some((labels, qtype, end))
}

fn answer(zone: &Zone, from: Ipv4Addr, query: &[u8]) -> Reply {
    if query.len() < 12 || query[2] & 0x80 != 0 {
        return Reply::Drop; // too short to answer, or itself a response
    }
    let Some((labels, qtype, _)) = question(query) else {
        return reply(query, Rcode::FormErr, None).map_or(Reply::Drop, Reply::Local);
    };
    let in_zone = labels.last().is_some_and(|l| l == ZONE);
    let lookup = zone.lookup(from, &labels);
    let (rcode, address) = match (in_zone, lookup) {
        // Nobody but the node's VMs gets anything, the upstream resolver included.
        (_, Lookup::Refused) => (Rcode::Refused, None),
        (false, _) => return Reply::Forward,
        (true, Lookup::Found(ip)) if qtype == TYPE_A || qtype == TYPE_ANY => (Rcode::NoError, Some(ip)),
        (true, Lookup::Found(_)) | (true, Lookup::NoData) => (Rcode::NoError, None),
        (true, Lookup::NotFound) => (Rcode::NxDomain, None),
    };
    reply(query, rcode, address).map_or(Reply::Drop, Reply::Local)
}

/// A response to `query`, with an A record when there's an address.
fn reply(query: &[u8], rcode: Rcode, address: Option<Ipv4Addr>) -> Option<Vec<u8>> {
    let end = question(query).map(|(_, _, end)| end);
    let flags = u16::from_be_bytes([query.get(2).copied()?, query.get(3).copied()?]);
    let authoritative = if matches!(rcode, Rcode::NoError | Rcode::NxDomain) { 0x0400 } else { 0 };
    let flags = 0x8000 | (flags & 0x7900) | authoritative | 0x0080 | rcode as u16;
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&query[..2]);
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&[0, end.is_some() as u8, 0, address.is_some() as u8, 0, 0, 0, 0]);
    if let Some(end) = end {
        out.extend_from_slice(&query[12..end]);
    }
    if let Some(ip) = address {
        out.extend_from_slice(&[0xc0, 0x0c, 0, TYPE_A as u8, 0, 1]);
        out.extend_from_slice(&TTL.to_be_bytes());
        out.extend_from_slice(&[0, 4]);
        out.extend_from_slice(&ip.octets());
    }
    Some(out)
}

#[derive(Clone, Copy)]
enum Upstream {
    Udp,
    Tcp,
}

impl Upstream {
    /// The node's own resolver answers everything outside `.copper`.
    async fn forward(self, query: &[u8]) -> Option<Vec<u8>> {
        let to = SocketAddr::new(upstream()?.into(), 53);
        tokio::time::timeout(UPSTREAM_TIMEOUT, async {
            match self {
                Upstream::Udp => {
                    let socket = UdpSocket::bind("0.0.0.0:0").await.ok()?;
                    socket.send_to(query, to).await.ok()?;
                    let mut buf = vec![0u8; 4096];
                    loop {
                        let n = socket.recv(&mut buf).await.ok()?;
                        if n >= 2 && buf[..2] == query[..2] {
                            return Some(buf[..n].to_vec());
                        }
                    }
                }
                Upstream::Tcp => {
                    let mut stream = TcpStream::connect(to).await.ok()?;
                    write_framed(&mut stream, query).await.ok()?;
                    read_framed(&mut stream).await
                }
            }
        })
        .await
        .ok()?
    }
}

/// The first IPv4 nameserver in /etc/resolv.conf: 127.0.0.53 on a host with
/// systemd-resolved.
fn upstream() -> Option<Ipv4Addr> {
    std::fs::read_to_string("/etc/resolv.conf").ok()?.lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .find_map(|a| a.trim().parse().ok())
}

async fn read_framed(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len).await.ok()?;
    let mut msg = vec![0u8; u16::from_be_bytes(len) as usize];
    stream.read_exact(&mut msg).await.ok()?;
    Some(msg)
}

async fn write_framed(stream: &mut TcpStream, msg: &[u8]) -> std::io::Result<()> {
    let len = u16::try_from(msg.len()).map_err(|_| std::io::Error::other("message too long"))?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(msg).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use copper_api::{ProjectNetwork, VmAddress};

    fn map() -> NetworkMap {
        let project = |name: &str, vms: &[(&str, &str)]| ProjectNetwork {
            id: uuid::Uuid::new_v4(),
            name: name.into(),
            vms: vms.iter().map(|(n, a)| VmAddress { name: n.to_string(), address: a.to_string() }).collect(),
        };
        NetworkMap { projects: vec![
            project("lab", &[("k3s-1", "10.42.1.10"), ("k3s-3", "10.42.4.10")]),
            project("home", &[("backup", "10.42.1.11")]),
        ], peers: vec![] }
    }

    /// A query for `name`, type `qtype`, with recursion desired.
    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.trim_end_matches('.').split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.extend_from_slice(&[0, 0, qtype as u8, 0, 1]);
        q
    }

    fn ask(from: &str, name: &str, qtype: u16) -> Reply {
        answer(&Zone::from_map(&map()), from.parse().unwrap(), &query(name, qtype))
    }

    fn rcode_and_address(reply: Reply) -> (u8, Option<Ipv4Addr>) {
        let Reply::Local(r) = reply else { panic!("{reply:?}") };
        assert_eq!(&r[..2], &[0x12, 0x34], "same id");
        assert_ne!(r[2] & 0x80, 0, "a response");
        let address = (r[7] == 1).then(|| Ipv4Addr::new(r[r.len() - 4], r[r.len() - 3], r[r.len() - 2], r[r.len() - 1]));
        (r[3] & 0x0f, address)
    }

    #[test]
    fn a_vm_finds_its_own_projects_vms_on_any_node() {
        assert_eq!(rcode_and_address(ask("10.42.1.10", "k3s-3.lab.copper", TYPE_A)), (0, Some(Ipv4Addr::new(10, 42, 4, 10))));
        assert_eq!(rcode_and_address(ask("10.42.1.10", "K3S-1.Lab.Copper.", TYPE_A)), (0, Some(Ipv4Addr::new(10, 42, 1, 10))), "any case");
        assert_eq!(rcode_and_address(ask("10.42.1.10", "k3s-3.lab.copper", 28)), (0, None), "no IPv6 address, but the name exists");
        assert_eq!(rcode_and_address(ask("10.42.1.10", "nope.lab.copper", TYPE_A)), (3, None));
    }

    #[test]
    fn other_projects_names_dont_exist() {
        assert_eq!(rcode_and_address(ask("10.42.1.10", "backup.home.copper", TYPE_A)), (3, None));
        assert_eq!(rcode_and_address(ask("10.42.1.11", "k3s-1.lab.copper", TYPE_A)), (3, None));
        assert_eq!(rcode_and_address(ask("10.42.1.11", "backup.home.copper", TYPE_A)), (0, Some(Ipv4Addr::new(10, 42, 1, 11))));
    }

    #[test]
    fn only_vms_get_answers_and_the_rest_goes_upstream() {
        assert_eq!(ask("10.42.1.10", "ubuntu.com", TYPE_A), Reply::Forward);
        assert_eq!(rcode_and_address(ask("192.168.68.50", "ubuntu.com", TYPE_A)), (5, None), "no open resolver");
        assert_eq!(rcode_and_address(ask("192.168.68.50", "k3s-1.lab.copper", TYPE_A)), (5, None));
    }

    /// Over real sockets, UDP and TCP. Linux puts all of 127.0.0.0/8 on
    /// loopback, so two "VMs" in different projects can ask from their own
    /// addresses without root.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn each_project_hears_only_its_own_names_over_udp_and_tcp() {
        let project = |name: &str, vm: &str, address: &str| ProjectNetwork {
            id: uuid::Uuid::new_v4(), name: name.into(), vms: vec![VmAddress { name: vm.into(), address: address.into() }],
        };
        let dns = Dns::new();
        *dns.zone.write().unwrap() = Zone::from_map(&NetworkMap {
            projects: vec![project("lab", "k3s-1", "127.0.0.2"), project("home", "backup", "127.0.0.3")], peers: vec![],
        });
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let at = udp.local_addr().unwrap();
        let tcp = TcpListener::bind(at).await.unwrap();
        tokio::spawn(dns.clone().answer_on(udp, tcp));

        let over_udp = |from: &'static str, name: &'static str| async move {
            let s = UdpSocket::bind(format!("{from}:0")).await.unwrap();
            s.send_to(&query(name, TYPE_A), at).await.unwrap();
            let mut buf = [0u8; 512];
            let n = tokio::time::timeout(Duration::from_secs(2), s.recv(&mut buf)).await.unwrap().unwrap();
            rcode_and_address(Reply::Local(buf[..n].to_vec()))
        };
        assert_eq!(over_udp("127.0.0.2", "k3s-1.lab.copper").await, (0, Some(Ipv4Addr::new(127, 0, 0, 2))));
        assert_eq!(over_udp("127.0.0.3", "k3s-1.lab.copper").await, (3, None), "home can't see lab");
        assert_eq!(over_udp("127.0.0.3", "backup.home.copper").await, (0, Some(Ipv4Addr::new(127, 0, 0, 3))));
        assert_eq!(over_udp("127.0.0.4", "backup.home.copper").await.0, 5, "not a VM: refused");

        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.3:0".parse().unwrap()).unwrap();
        let mut stream = socket.connect(at).await.unwrap();
        write_framed(&mut stream, &query("backup.home.copper", TYPE_A)).await.unwrap();
        let reply = read_framed(&mut stream).await.unwrap();
        assert_eq!(rcode_and_address(Reply::Local(reply)), (0, Some(Ipv4Addr::new(127, 0, 0, 3))));
        write_framed(&mut stream, &query("k3s-1.lab.copper", TYPE_A)).await.unwrap();
        assert_eq!(rcode_and_address(Reply::Local(read_framed(&mut stream).await.unwrap())).0, 3, "and the same over TCP");
    }

    #[test]
    fn malformed_queries_get_no_further_than_formerr() {
        let zone = Zone::from_map(&map());
        let from = Ipv4Addr::new(10, 42, 1, 10);
        assert_eq!(answer(&zone, from, &[1, 2, 3]), Reply::Drop);
        let mut q = query("k3s-1.lab.copper", TYPE_A);
        q.truncate(20);
        assert_eq!(rcode_and_address(answer(&zone, from, &q)), (1, None));
        let mut response = query("k3s-1.lab.copper", TYPE_A);
        response[2] |= 0x80;
        assert_eq!(answer(&zone, from, &response), Reply::Drop, "never answers a response");
    }
}
