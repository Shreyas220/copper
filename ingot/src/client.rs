//! HTTP client for the kiln node API.

use std::time::Duration;

use anyhow::Context;
use copper_api::*;

#[derive(Clone)]
pub struct Kiln {
    http: reqwest::Client,
    base: String,
    node_id: NodeId,
}

impl Kiln {
    pub fn new(base: &str, node_id: NodeId) -> Self {
        let http = reqwest::Client::builder()
            // Longer than kiln's long-poll window, so a healthy poll never times out here.
            .timeout(Duration::from_secs(40))
            .build()
            .unwrap();
        Self { http, base: base.trim_end_matches('/').to_string(), node_id }
    }

    pub async fn register(&self, req: &RegisterRequest) -> anyhow::Result<RegistrationStatus> {
        let resp: RegisterResponse = self
            .http
            .post(format!("{}/nodes/register", self.base))
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
        let mut req = self.http.get(format!("{}/nodes/{}/desired", self.base, self.node_id));
        if let Some(a) = after {
            req = req.query(&[("after", a)]);
        }
        Ok(req.send().await?.error_for_status()?.json().await.context("decoding desired state")?)
    }

    pub async fn observed(&self, report: &ObservedNodeState) -> anyhow::Result<()> {
        self.http
            .post(format!("{}/nodes/{}/observed", self.base, self.node_id))
            .json(report)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}
