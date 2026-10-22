//! HTTP client for the kiln node API.

use std::time::Duration;

use anyhow::Context;
use copper_api::*;

#[derive(Clone)]
pub struct Kiln {
    http: reqwest::Client,
    /// Image downloads take minutes, so they get a stall timeout instead of
    /// a total one.
    download: reqwest::Client,
    /// Sessions (an SSH shell, a serial console) can sit idle for hours, so
    /// only connecting is timed.
    sessions: reqwest::Client,
    base: String,
    node_id: NodeId,
    token: String,
}

impl Kiln {
    pub fn new(base: &str, node_id: NodeId, token: String) -> Self {
        let http = reqwest::Client::builder()
            // Longer than kiln's long-poll window, so a healthy poll never times out here.
            .timeout(Duration::from_secs(40))
            .build()
            .unwrap();
        let download = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .build()
            .unwrap();
        let sessions = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build().unwrap();
        Self { http, download, sessions, base: base.trim_end_matches('/').to_string(), node_id, token }
    }

    /// Long-poll for sessions someone opened on this node's VMs.
    pub async fn session_requests(&self) -> anyhow::Result<Vec<ConsoleRequest>> {
        let url = format!("{}/nodes/{}/consoles", self.base, self.node_id);
        Ok(self.http.get(url).bearer_auth(&self.token).send().await?.error_for_status()?.json().await.context("decoding session requests")?)
    }

    /// Dial kiln back for one session. The upgraded connection then carries
    /// it both ways, for as long as it lasts, idle or not.
    pub async fn attach(&self, session: uuid::Uuid) -> anyhow::Result<reqwest::Upgraded> {
        let resp = self.sessions
            .get(format!("{}/nodes/{}/consoles/{session}", self.base, self.node_id))
            .bearer_auth(&self.token)
            .header(reqwest::header::CONNECTION, "upgrade")
            .header(reqwest::header::UPGRADE, "copper-console")
            .send()
            .await?;
        anyhow::ensure!(resp.status() == reqwest::StatusCode::SWITCHING_PROTOCOLS, "kiln refused the session: {}", resp.status());
        Ok(resp.upgrade().await?)
    }

    /// Tell kiln why this node can't carry a session.
    pub async fn refuse(&self, session: uuid::Uuid, reason: &str) -> anyhow::Result<()> {
        self.http
            .post(format!("{}/nodes/{}/consoles/{session}/refuse", self.base, self.node_id))
            .bearer_auth(&self.token)
            .json(&SessionRefused { reason: reason.to_string() })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Download a catalogue blob to `dest`, verifying its SHA-256 on the way.
    /// Nothing appears at `dest` unless the hash matched.
    pub async fn fetch_image(&self, hash: &str, dest: &std::path::Path) -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncWriteExt;
        anyhow::ensure!(valid_sha256(hash), "invalid image hash");
        let dir = dest.parent().context("image path needs a directory")?;
        tokio::fs::create_dir_all(dir).await.with_context(|| format!("creating {}", dir.display()))?;
        let mut resp = self.download
            .get(format!("{}/nodes/{}/images/{hash}", self.base, self.node_id))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?;
        let tmp = dest.with_extension("part");
        let mut file = tokio::fs::File::create(&tmp).await.with_context(|| format!("creating {}", tmp.display()))?;
        let mut hasher = Sha256::new();
        let copied = async {
            while let Some(chunk) = resp.chunk().await? {
                hasher.update(&chunk);
                file.write_all(&chunk).await?;
            }
            file.sync_all().await?;
            anyhow::Ok(())
        }
        .await;
        drop(file);
        let verified = copied.and_then(|()| {
            let got = format!("{:x}", hasher.finalize());
            if got == hash { Ok(()) } else { Err(anyhow::anyhow!("download did not match its hash (got {got})")) }
        });
        if let Err(e) = verified {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        crate::state::publish_file(&tmp, dest)
    }

    pub async fn register(&self, req: &RegisterRequest) -> anyhow::Result<RegistrationStatus> {
        let resp: RegisterResponse = self
            .http
            .post(format!("{}/nodes/register", self.base))
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("decoding register response")?;
        Ok(resp.status)
    }

    pub async fn desired(&self, after: Option<Generation>) -> anyhow::Result<DesiredNodeState> {
        let mut req = self.http.get(format!("{}/nodes/{}/desired", self.base, self.node_id)).bearer_auth(&self.token);
        if let Some(a) = after {
            req = req.query(&[("after", a)]);
        }
        Ok(req.send().await?.error_for_status()?.json().await.context("decoding desired state")?)
    }

    pub async fn observed(&self, report: &ObservedNodeState) -> anyhow::Result<()> {
        self.http
            .post(format!("{}/nodes/{}/observed", self.base, self.node_id))
            .bearer_auth(&self.token)
            .json(report)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Authentication and retirement failures require operator intervention.
pub fn terminal(error: &anyhow::Error) -> bool {
    error.downcast_ref::<reqwest::Error>().and_then(|e| e.status()).is_some_and(|s|
        matches!(s, reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::GONE))
}

pub fn pending(error: &anyhow::Error) -> bool {
    error.downcast_ref::<reqwest::Error>().and_then(|e| e.status()) == Some(reqwest::StatusCode::FORBIDDEN)
}

/// Allows protocol recovery to be tested without sockets or a live controller.
pub(crate) trait NodeClient: Send + Sync {
    fn register(&self, req: &RegisterRequest) -> impl std::future::Future<Output = anyhow::Result<RegistrationStatus>> + Send;
    fn desired(&self, after: Option<Generation>) -> impl std::future::Future<Output = anyhow::Result<DesiredNodeState>> + Send;
    fn observed(&self, report: &ObservedNodeState) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

impl NodeClient for Kiln {
    async fn register(&self, req: &RegisterRequest) -> anyhow::Result<RegistrationStatus> { Kiln::register(self, req).await }
    async fn desired(&self, after: Option<Generation>) -> anyhow::Result<DesiredNodeState> { Kiln::desired(self, after).await }
    async fn observed(&self, report: &ObservedNodeState) -> anyhow::Result<()> { Kiln::observed(self, report).await }
}
