//! Serial consoles, libvirt side. Kiln tells us about a session over a
//! long-poll; we dial back and pump bytes between that connection and the
//! VM's serial console. See kiln/src/console.rs for the whole flow.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use copper_api::{ConsoleRequest, InstanceId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use virt::connect::Connect;
use virt::domain::Domain;
use virt::stream::Stream;

use crate::client::Kiln;
use crate::hv::name_of;

const RETRY: Duration = Duration::from_secs(5);

/// Runs for the life of ingot. Errors only delay it; registration and
/// approval problems are the desired-state loop's business.
pub async fn console_loop(kiln: Kiln, uri: String) {
    loop {
        match kiln.console_requests().await {
            Ok(requests) => {
                for req in requests {
                    tokio::spawn(serve(kiln.clone(), uri.clone(), req));
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "console poll failed");
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

async fn serve(kiln: Kiln, uri: String, req: ConsoleRequest) {
    let conn = match kiln.attach_console(req.session).await {
        Ok(conn) => conn,
        Err(e) => return tracing::warn!(instance = %req.instance, error = %e, "console attach failed"),
    };
    tracing::info!(instance = %req.instance, "console opened");
    let (mut from_kiln, mut to_kiln) = tokio::io::split(conn);
    let (to_vm, input) = mpsc::channel::<Vec<u8>>();
    let (output, mut from_vm) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let pump = tokio::task::spawn_blocking(move || pump(&uri, req.instance, input, output));
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

/// Blocking: a reader loop on this thread and a writer thread, sharing one
/// libvirt stream. Ends when either side goes away.
fn pump(uri: &str, id: InstanceId, input: mpsc::Receiver<Vec<u8>>, output: tokio::sync::mpsc::Sender<Vec<u8>>) -> anyhow::Result<()> {
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
