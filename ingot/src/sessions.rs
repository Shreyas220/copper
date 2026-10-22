//! Sessions kiln asks this node to carry: TCP to one of its VMs' ports, for
//! `cu ssh` and `cu forward`, and on libvirt nodes the VM's serial console.
//! Kiln tells us about a session over a long-poll; we connect to our own VM,
//! then dial kiln back to carry the bytes, or tell kiln why we couldn't. See
//! kiln/src/sessions.rs for the whole flow.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use copper_api::ConsoleRequest;

use crate::client::Kiln;
use crate::reconcile::Reconciler;

const RETRY: Duration = Duration::from_secs(5);
/// How long to wait for the VM's port before telling kiln it's not up yet.
const CONNECT: Duration = Duration::from_secs(3);

/// Runs for the life of ingot. Errors only delay it; registration and
/// approval problems are the desired-state loop's business.
pub async fn session_loop(kiln: Kiln, reconciler: Arc<Mutex<Reconciler>>, libvirt: String) {
    loop {
        match kiln.session_requests().await {
            Ok(requests) => {
                for req in requests {
                    match req.port {
                        Some(port) => tokio::spawn(tcp(kiln.clone(), reconciler.clone(), req, port)),
                        None => tokio::spawn(console(kiln.clone(), libvirt.clone(), req)),
                    };
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "session poll failed");
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

async fn refuse(kiln: &Kiln, req: &ConsoleRequest, reason: &str) {
    if let Err(e) = kiln.refuse(req.session, reason).await {
        tracing::warn!(instance = %req.instance, error = %e, "couldn't tell kiln a session was refused");
    }
}

/// Connect to the VM first, so a port that isn't open yet is a clean
/// refusal kiln passes on, and the caller retries.
async fn tcp(kiln: Kiln, reconciler: Arc<Mutex<Reconciler>>, req: ConsoleRequest, port: u16) {
    let id = req.instance;
    let target = tokio::task::spawn_blocking(move || reconciler.lock().unwrap().reach(id)).await.ok().flatten();
    let Some(ip) = target else {
        return refuse(&kiln, &req, "the vm has no address yet; it's still booting").await;
    };
    let mut vm = match tokio::time::timeout(CONNECT, tokio::net::TcpStream::connect(SocketAddr::new(ip, port))).await {
        Ok(Ok(stream)) => stream,
        _ => return refuse(&kiln, &req, &format!("nothing answers on port {port} yet")).await,
    };
    let mut conn = match kiln.attach(req.session).await {
        Ok(conn) => conn,
        Err(e) => return tracing::warn!(instance = %id, error = %e, "session attach failed"),
    };
    tracing::info!(instance = %id, port, "tunnel opened");
    let result = tokio::io::copy_bidirectional(&mut conn, &mut vm).await;
    tracing::info!(instance = %id, port, ?result, "tunnel closed");
}

/// Older bridged VMs report their own addresses, so a guest controls them.
/// Only connect to private LAN addresses that aren't this host's: nothing a
/// bridged guest couldn't reach by itself. Loopback, link-local and the
/// tailnet's addresses are never targets.
pub fn relay_allowed(ip: IpAddr) -> bool {
    let private = match ip {
        IpAddr::V4(v4) => v4.is_private(),
        // Unique local, minus Tailscale's fd7a:115c:a1e0::/48.
        IpAddr::V6(v6) => {
            let s = v6.segments();
            (s[0] & 0xfe00) == 0xfc00 && !(s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0)
        }
    };
    // Binding succeeds only for an address assigned to this host.
    private && std::net::UdpSocket::bind(SocketAddr::new(ip, 0)).is_err()
}

#[cfg(target_os = "macos")]
async fn console(kiln: Kiln, _libvirt: String, req: ConsoleRequest) {
    refuse(&kiln, &req, "this node runs Tart, which has no serial console").await;
}

/// Pump bytes between kiln and the VM's serial console.
#[cfg(not(target_os = "macos"))]
async fn console(kiln: Kiln, libvirt: String, req: ConsoleRequest) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let conn = match kiln.attach(req.session).await {
        Ok(conn) => conn,
        Err(e) => return tracing::warn!(instance = %req.instance, error = %e, "console attach failed"),
    };
    tracing::info!(instance = %req.instance, "console opened");
    let (mut from_kiln, mut to_kiln) = tokio::io::split(conn);
    let (to_vm, input) = std::sync::mpsc::channel::<Vec<u8>>();
    let (output, mut from_vm) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let pump = tokio::task::spawn_blocking(move || serial::pump(&libvirt, req.instance, input, output));
    let upstream = async {
        let mut buf = vec![0u8; 4096];
        while let Ok(n) = from_kiln.read(&mut buf).await {
            if n == 0 || to_vm.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    };
    let downstream = async {
        while let Some(bytes) = from_vm.recv().await {
            if to_kiln.write_all(&bytes).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = upstream => {},
        _ = downstream => {},
    }
    // Dropping `to_vm` makes the writer thread abort the libvirt stream,
    // which unblocks the reader.
    drop(to_vm);
    if let Ok(Err(e)) = pump.await {
        tracing::warn!(instance = %req.instance, error = %format!("{e:#}"), "console ended");
    }
    tracing::info!(instance = %req.instance, "console closed");
}

#[cfg(not(target_os = "macos"))]
mod serial {
    use std::sync::{mpsc, Arc};

    use copper_api::InstanceId;
    use virt::connect::Connect;
    use virt::domain::Domain;
    use virt::stream::Stream;

    use crate::hv::name_of;

    /// Blocking: a reader loop on this thread and a writer thread, sharing
    /// one libvirt stream. Ends when either side goes away.
    pub fn pump(uri: &str, id: InstanceId, input: mpsc::Receiver<Vec<u8>>, output: tokio::sync::mpsc::Sender<Vec<u8>>) -> anyhow::Result<()> {
        let err = |e: virt::error::Error| anyhow::anyhow!("libvirt: {}", e.message());
        let mut conn = Connect::open(Some(uri)).map_err(err)?;
        let result = (|| {
            let domain = Domain::lookup_by_name(&conn, &name_of(id)).map_err(err)?;
            let stream = Arc::new(Stream::new(&conn, 0).map_err(err)?);
            // FORCE takes the console from any other session, e.g. a forgotten tab.
            domain.open_console(None, &stream, virt::sys::VIR_DOMAIN_CONSOLE_FORCE).map_err(err)?;
            let writer_stream = stream.clone();
            let writer = std::thread::spawn(move || {
                for bytes in input {
                    if writer_stream.send(&bytes).is_err() {
                        break;
                    }
                }
                // The browser left: abort so the reader's blocking recv returns.
                unsafe { virt::sys::virStreamAbort(writer_stream.as_ptr()) };
            });
            let mut buf = vec![0u8; 4096];
            while let Ok(n) = stream.recv(&mut buf) {
                if n == 0 || output.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            drop(output);
            let _ = writer.join();
            anyhow::Ok(())
        })();
        let _ = conn.close();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_vms_are_only_reached_at_private_lan_addresses() {
        for ok in ["10.255.255.1", "172.31.255.1", "192.168.254.254", "fd12:3456::1"] {
            assert!(relay_allowed(ok.parse().unwrap()), "{ok}");
        }
        for bad in ["127.0.0.1", "0.0.0.0", "169.254.1.1", "100.75.219.83", "8.8.8.8", "::1", "fd7a:115c:a1e0::1", "fe80::1"] {
            assert!(!relay_allowed(bad.parse().unwrap()), "{bad}");
        }
    }
}
