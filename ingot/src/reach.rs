//! How a Mac's host reaches its own VMs. macOS's Local Network privacy keeps
//! any program a LaunchAgent starts, ingot included, from talking to
//! addresses on the Mac's own networks until someone clicks Allow on the
//! Mac's screen; only root is exempt. So on a Mac, `ingot net`, already
//! root, answers the VMs' DNS and opens ingot's connections to them: ingot
//! asks over a Unix socket only its own user can open, for one port on one
//! of the node's VMs, and the helper carries the bytes. On Linux ingot does
//! both itself.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use copper_api::{NetworkMap, NodeNetwork, ProjectNetwork};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::watch;

use crate::dns::Dns;

pub const SOCKET: &str = "/var/run/copper/reach.sock";
/// How long the helper waits for the VM's port.
const CONNECT: Duration = Duration::from_secs(3);
/// A request is one line, `10.42.3.10:22`; the answer is `ok`, or `no: why`.
const MAX_LINE: usize = 64;

/// What `ingot net` does for ingot on a Mac.
pub struct Host {
    pub dns: Arc<Dns>,
    vms: watch::Sender<Vec<Ipv4Addr>>,
}

impl Host {
    /// Answer DNS on the gateway once there is one, and connections on
    /// `socket`, which only `owner`, ingot's user, can open.
    pub fn start(socket: &Path, owner: u32) -> anyhow::Result<Self> {
        let listener = listen(socket, owner)?;
        let (vms, rx) = watch::channel(vec![]);
        tokio::spawn(relay(listener, rx));
        let dns = Dns::new();
        tokio::spawn(dns.clone().serve());
        Ok(Self { dns, vms })
    }

    /// From each checked network file: the names VMs look up, and the
    /// node's own VMs, the only addresses ingot may ask for.
    pub fn update(&self, network: &NodeNetwork, projects: &[ProjectNetwork], local: Vec<Ipv4Addr>) {
        self.dns.update(network, &NetworkMap { projects: projects.to_vec(), peers: vec![] });
        self.vms.send_if_modified(|vms| std::mem::replace(vms, local.clone()) != local);
    }
}

/// Root's umask leaves others without write permission on the socket from
/// the start, so nobody else can connect before it's handed to `owner`.
fn listen(path: &Path, owner: u32) -> anyhow::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).with_context(|| format!("listening on {}", path.display()))?;
    std::os::unix::fs::chown(path, Some(owner), None)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

async fn relay(listener: UnixListener, vms: watch::Receiver<Vec<Ipv4Addr>>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let vms = vms.clone();
        tokio::spawn(async move {
            if let Err(e) = carry(stream, &vms).await {
                tracing::debug!(error = %format!("{e:#}"), "relay to a vm ended");
            }
        });
    }
}

async fn carry(mut ingot: UnixStream, vms: &watch::Receiver<Vec<Ipv4Addr>>) -> anyhow::Result<()> {
    let line = read_line(&mut ingot).await?;
    let to: SocketAddrV4 = line.parse().with_context(|| format!("{line:?} isn't an address"))?;
    if !vms.borrow().contains(to.ip()) {
        ingot.write_all(b"no: not a vm on this node\n").await?;
        bail!("{to} isn't a vm on this node");
    }
    let Ok(Ok(mut vm)) = tokio::time::timeout(CONNECT, TcpStream::connect(to)).await else {
        ingot.write_all(b"no: nothing answers\n").await?;
        return Ok(());
    };
    ingot.write_all(b"ok\n").await?;
    tokio::io::copy_bidirectional(&mut ingot, &mut vm).await?;
    Ok(())
}

/// ingot's side: a stream to `to` through `ingot net`, or why not.
pub async fn connect(socket: &Path, to: SocketAddrV4) -> anyhow::Result<UnixStream> {
    let mut helper = UnixStream::connect(socket).await.with_context(|| format!("connecting to {}", socket.display()))?;
    helper.write_all(format!("{to}\n").as_bytes()).await?;
    match read_line(&mut helper).await?.as_str() {
        "ok" => Ok(helper),
        answer => bail!("{}", answer.strip_prefix("no: ").unwrap_or(answer)),
    }
}

/// One byte at a time, so nothing after the line is read with it.
async fn read_line(stream: &mut UnixStream) -> anyhow::Result<String> {
    let mut line = vec![];
    loop {
        let byte = stream.read_u8().await.context("connection closed")?;
        if byte == b'\n' {
            return Ok(String::from_utf8(line)?);
        }
        line.push(byte);
        if line.len() > MAX_LINE {
            bail!("line too long");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[tokio::test]
    async fn ingot_reaches_its_own_vms_through_the_helper_and_nothing_else() {
        // Short: macOS's own temp dir is past its 104-byte limit for socket paths.
        let socket = std::path::PathBuf::from(format!("/tmp/copper-reach-{}.sock", &uuid::Uuid::new_v4().to_string()[..8]));
        let me = unsafe { libc::getuid() };
        let listener = listen(&socket, me).unwrap();
        assert_eq!(std::fs::metadata(&socket).unwrap().mode() & 0o777, 0o600, "only ingot's user");

        let vm = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let std::net::SocketAddr::V4(at) = vm.local_addr().unwrap() else { unreachable!() };
        let (vms, rx) = watch::channel(vec![]);
        tokio::spawn(relay(listener, rx));

        let refused = connect(&socket, at).await.unwrap_err();
        assert_eq!(refused.to_string(), "not a vm on this node");

        vms.send(vec![*at.ip()]).unwrap();
        tokio::spawn(async move {
            let (mut s, _) = vm.accept().await.unwrap();
            let mut buf = [0u8; 5];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(&buf.map(|b| b.to_ascii_uppercase())).await.unwrap();
        });
        let mut stream = connect(&socket, at).await.unwrap();
        stream.write_all(b"hello").await.unwrap();
        let mut back = [0u8; 5];
        stream.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"HELLO");

        let closed = SocketAddrV4::new(*at.ip(), 1);
        assert_eq!(connect(&socket, closed).await.unwrap_err().to_string(), "nothing answers", "a vm whose port isn't open");
        for junk in ["10.42.3.10", "not an address", &"9".repeat(100)] {
            let mut s = UnixStream::connect(&socket).await.unwrap();
            s.write_all(format!("{junk}\n").as_bytes()).await.unwrap();
            let mut rest = vec![];
            let _ = s.read_to_end(&mut rest).await;
            assert!(rest.is_empty(), "{junk:?} gets nothing but a closed connection");
        }
        std::fs::remove_file(socket).unwrap();
    }
}
