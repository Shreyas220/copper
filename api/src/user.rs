use serde::{Deserialize, Serialize};
use crate::node::{Arch, Capacity, DesiredState, InstanceId, NodeId, ObservedState, Usage};

/// Instance state as the user sees it. Derived from (desired, observed, node online),
/// never stored. See docs/design.md for the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
    Deleting,
    /// The node hosting it is offline.
    Unknown,
}

impl InstanceState {
    pub fn derive(desired: DesiredState, observed: Option<ObservedState>, node_online: bool) -> Self {
        use DesiredState as D;
        use ObservedState as O;
        let observed = observed.unwrap_or(O::Absent);
        // A failure the node already reported stays visible even if the node
        // has since gone offline or been retired.
        if observed == O::Failed && desired != D::Absent {
            return Self::Failed;
        }
        if !node_online {
            return Self::Unknown;
        }
        match (desired, observed) {
            (D::Absent, _) => Self::Deleting,
            (D::Running, O::Running) => Self::Running,
            (D::Running, O::Failed) => Self::Failed,
            (D::Running, _) => Self::Starting,
            (D::Stopped, O::Running | O::Stopping) => Self::Stopping,
            (D::Stopped, _) => Self::Stopped,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: InstanceId,
    pub project: String,
    pub name: String,
    pub state: InstanceState,
    pub vcpus: u32,
    pub memory_mb: u64,
    pub arch: Arch,
    pub disk_gb: u64,
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Admin only. Stripped for ordinary users.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeId>,
    /// Admin only: the spread group this instance was placed with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread: Option<String>,
    /// Admin only: the node's hostname, for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    pub image: String,
    /// Unix seconds.
    #[serde(default)]
    pub created_at: i64,
    /// The project owner, who is the only non-admin who can touch it.
    #[serde(default)]
    pub owner: String,
    /// Unix seconds when the node last reported it running after being
    /// anything else. `None` unless running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_since: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// The SSH keys written into it at first boot. Empty means nobody can
    /// log in, and only recreating it changes that.
    #[serde(default)]
    pub ssh_keys: Vec<InstanceKey>,
}

/// A key a VM was made with: enough to recognise it, not the key itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceKey {
    /// `SHA256:…`, as `ssh-keygen -l` prints it.
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateInstance {
    pub name: String,
    pub vcpus: u32,
    pub memory_mb: u64,
    pub arch: Arch,
    pub image: String,
    pub disk_gb: u64,
    /// SSH public keys for the default user.
    #[serde(default)]
    pub ssh_keys: Vec<String>,
    /// Admin only: place on exactly this node, or refuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeId>,
    /// Admin only: prefer nodes holding the fewest instances of this group
    /// in the same project. Soft: never refuses on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread: Option<String>,
}

/// Change a stopped instance's size. Omitted fields stay as they are. Disks
/// only grow.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResizeInstance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gb: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Online,
    Offline,
    Retired,
}

/// Per-node overcommit ratios, set by the admin. Placement admits up to
/// `reported capacity × ratio` of each resource.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Overcommit {
    pub cpu: f64,
    pub memory: f64,
    pub disk: f64,
}

impl Default for Overcommit {
    fn default() -> Self {
        Self { cpu: 1.0, memory: 1.0, disk: 1.0 }
    }
}

impl Overcommit {
    pub const MAX: f64 = 64.0;
    pub fn valid(&self) -> bool {
        [self.cpu, self.memory, self.disk].iter().all(|r| r.is_finite() && *r > 0.0 && *r <= Self::MAX)
    }
}

/// Partial update of a node's overcommit ratios.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_ratio: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub hostname: String,
    pub arch: Arch,
    pub status: NodeStatus,
    /// What the node reported: host resources minus its reserve.
    #[serde(default)]
    pub capacity: Option<Capacity>,
    /// Sum of every instance assigned to the node, including pending deletions.
    #[serde(default)]
    pub allocated: Option<Capacity>,
    #[serde(default)]
    pub overcommit: Overcommit,
    /// See `fingerprint`. `None` until the node has registered a credential.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Unix seconds of the last report.
    #[serde(default)]
    pub last_seen: Option<i64>,
    #[serde(default)]
    pub generation: u64,
    /// Names of the instances assigned to it, as `project/name`.
    #[serde(default)]
    pub instances: Vec<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// A base image in kiln's catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub name: String,
    pub arch: Arch,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_table() {
        use DesiredState as D;
        use InstanceState as S;
        use ObservedState as O;
        assert_eq!(S::derive(D::Running, None, true), S::Starting);
        assert_eq!(S::derive(D::Running, Some(O::Starting), true), S::Starting);
        assert_eq!(S::derive(D::Running, Some(O::Running), true), S::Running);
        assert_eq!(S::derive(D::Running, Some(O::Failed), true), S::Failed);
        assert_eq!(S::derive(D::Stopped, Some(O::Running), true), S::Stopping);
        assert_eq!(S::derive(D::Stopped, Some(O::Stopped), true), S::Stopped);
        assert_eq!(S::derive(D::Stopped, None, true), S::Stopped);
        assert_eq!(S::derive(D::Absent, Some(O::Running), true), S::Deleting);
        assert_eq!(S::derive(D::Running, Some(O::Running), false), S::Unknown);
        assert_eq!(S::derive(D::Running, Some(O::Failed), false), S::Failed);
        assert_eq!(S::derive(D::Absent, Some(O::Failed), true), S::Deleting);
    }
}
