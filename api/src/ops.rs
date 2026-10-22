//! What the console watches: the event log, metrics, quotas and fit checks.

use serde::{Deserialize, Serialize};

use crate::node::{Capacity, NodeId, Usage};
use crate::user::NodeStatus;

/// About the kiln answering. `hostname` matches the node record of the
/// machine kiln runs on, if that machine is also a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KilnInfo {
    pub hostname: String,
    pub version: String,
}

/// One line of the event log, newest first when listed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    /// Unix seconds.
    pub at: i64,
    /// Short and stable, e.g. `created`, `failed`, `node quiet`.
    pub kind: String,
    /// Human detail, e.g. `thinkcentre · 192.168.1.42`.
    pub detail: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub instance: Option<String>,
    /// Hostname of the node involved. Admin only.
    #[serde(default)]
    pub node: Option<String>,
    /// Who asked for it, when a person did.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Sample {
    pub at: i64,
    pub usage: Usage,
}

/// Per-project caps. `None` means no cap.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaCaps {
    #[serde(default)]
    pub vcpus: Option<u32>,
    #[serde(default)]
    pub memory_mb: Option<u64>,
    #[serde(default)]
    pub disk_gb: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quota {
    pub project: String,
    pub owner: String,
    pub caps: QuotaCaps,
    /// Everything the project's instances hold, including stopped ones.
    pub used: Capacity,
}

/// The answer to "would this fit?", without creating anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FitCheck {
    pub fits: bool,
    /// Why not, in words, when it doesn't.
    #[serde(default)]
    pub reason: Option<String>,
    /// The most any single node of this arch could take right now, bounded
    /// by what the project's quota still allows.
    pub largest: Capacity,
    /// Per-node detail. Admin only, since only admins see placement.
    #[serde(default)]
    pub nodes: Option<Vec<NodeFit>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeFit {
    pub node: NodeId,
    pub hostname: String,
    pub status: NodeStatus,
    pub free: Capacity,
    pub fits: bool,
}
