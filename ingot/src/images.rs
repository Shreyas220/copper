//! Base images fetched from kiln by hash. Downloads run in their own task, so
//! a slow transfer never holds up reconciliation or reporting. An instance
//! waiting for its image reports `starting` with a note saying so.
//!
//! Blobs land at `<images dir>/sha256/<hash>.img` only after the download
//! matched its hash, so a file's presence means it is verified.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::client::Kiln;

pub struct Images {
    dir: PathBuf,
    queue: mpsc::UnboundedSender<String>,
    status: Mutex<Status>,
}

#[derive(Default)]
struct Status {
    inflight: HashSet<String>,
    errors: HashMap<String, String>,
}

impl Images {
    pub fn new(dir: PathBuf) -> (Self, mpsc::UnboundedReceiver<String>) {
        let (queue, rx) = mpsc::unbounded_channel();
        (Self { dir, queue, status: Mutex::default() }, rx)
    }

    pub fn path(&self, hash: &str) -> PathBuf {
        self.dir.join(format!("{hash}.img"))
    }

    pub fn ready(&self, hash: &str) -> bool {
        self.path(hash).is_file()
    }

    /// Queue a download unless one is already running, and say what the
    /// instance is waiting for.
    pub fn want(&self, image: &str, hash: &str) -> String {
        let mut s = self.status.lock().unwrap();
        if s.inflight.insert(hash.to_string()) {
            let _ = self.queue.send(hash.to_string());
        }
        match s.errors.get(hash) {
            Some(e) => format!("fetching image {image} failed, retrying: {e}"),
            None => format!("fetching image {image}"),
        }
    }
}

/// One download at a time. A failure is remembered for the status note and
/// retried the next time reconciliation asks for the image.
pub async fn fetch_loop(kiln: Kiln, images: Arc<Images>, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(hash) = rx.recv().await {
        if !images.ready(&hash) {
            tracing::info!(image = %hash, "fetching image from kiln");
            let result = kiln.fetch_image(&hash, &images.path(&hash)).await;
            let mut s = images.status.lock().unwrap();
            match result {
                Ok(()) => {
                    s.errors.remove(&hash);
                    tracing::info!(image = %hash, "image fetched and verified");
                }
                Err(e) => {
                    tracing::warn!(image = %hash, error = %format!("{e:#}"), "image fetch failed");
                    s.errors.insert(hash.clone(), format!("{e:#}"));
                }
            }
        }
        images.status.lock().unwrap().inflight.remove(&hash);
    }
}
