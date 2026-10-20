//! HTTP client for the kiln node API.

use std::time::Duration;

use anyhow::Context;
use copper_api::*;

#[derive(Clone)]
pub struct Kiln {
    http: reqwest::Client,
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
        Self { http, base: base.trim_end_matches('/').to_string(), node_id, token }
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
