use serde::{Deserialize, Serialize};
use crate::node::{Arch, DesiredState, InstanceId, NodeId, ObservedState};

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Online,
    Offline,
    Retired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub hostname: String,
    pub arch: Arch,
    pub status: NodeStatus,
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
