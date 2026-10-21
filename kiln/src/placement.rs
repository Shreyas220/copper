//! Choose a node for a new instance, or refuse immediately.
//!
//! A node admits up to its reported capacity times its overcommit ratio for
//! each resource. Allocated capacity is the sum of every instance still
//! assigned to it, including stopped instances and pending deletions.
//! Resources are only released when the agent confirms deletion and the row
//! is removed.

use std::cmp::Reverse;

use copper_api::*;
use rusqlite::Connection;

use crate::db::{self, InstanceRow, NodeRow};
use crate::error::{Error, Result};

/// Admin-only placement hints.
#[derive(Default)]
pub struct Hints<'a> {
    /// Place on exactly this node or refuse.
    pub node: Option<NodeId>,
    /// Prefer nodes with the fewest instances of this group in this project.
    pub spread: Option<(db::Uuid, &'a str)>,
}

pub fn choose(conn: &Connection, spec: &InstanceSpec, hints: &Hints) -> Result<NodeRow> {
    let mut best: Option<(NodeRow, (usize, Reverse<u64>))> = None;
    for node in db::list_nodes(conn)? {
        if hints.node.is_some_and(|pinned| pinned != node.id) || !node.online() || node.arch != spec.arch {
            continue;
        }
        let instances = db::list_instances_on_node(conn, node.id)?;
        let free = free(&node, instances.iter());
        if !fits(&free, spec) {
            continue;
        }
        let peers = hints.spread.map_or(0, |(project, group)| {
            instances.iter().filter(|i| i.project_id == project && i.spread.as_deref() == Some(group)).count()
        });
        // Fewest group peers first, then the most free memory.
        let score = (peers, Reverse(free.memory_mb));
        if best.as_ref().is_none_or(|(_, b)| score < *b) {
            best = Some((node, score));
        }
    }
    best.map(|(n, _)| n).ok_or_else(|| match hints.node {
        Some(_) => Error::conflict("the requested node is offline, has another architecture, or lacks capacity"),
        None => Error::conflict("no node has capacity for this instance"),
    })
}

/// Whether `spec` fits on the instance's own node once its current
/// allocation is released. Used by resize; instances never move.
pub fn fits_in_place(conn: &Connection, node: &NodeRow, id: InstanceId, spec: &InstanceSpec) -> Result<bool> {
    let instances = db::list_instances_on_node(conn, node.id)?;
    Ok(fits(&free(node, instances.iter().filter(|i| i.id != id)), spec))
}

/// Reported capacity scaled by the node's overcommit ratios.
pub fn admissible(node: &NodeRow) -> Capacity {
    let scale = |v: f64, r: f64| (v * r).floor();
    Capacity {
        vcpus: scale(node.capacity.vcpus as f64, node.overcommit.cpu) as u32,
        memory_mb: scale(node.capacity.memory_mb as f64, node.overcommit.memory) as u64,
        disk_gb: scale(node.capacity.disk_gb as f64, node.overcommit.disk) as u64,
    }
}

pub fn allocated<'a>(instances: impl Iterator<Item = &'a InstanceRow>) -> Capacity {
    let mut used = Capacity { vcpus: 0, memory_mb: 0, disk_gb: 0 };
    for inst in instances {
        used.vcpus = used.vcpus.saturating_add(inst.spec.vcpus);
        used.memory_mb = used.memory_mb.saturating_add(inst.spec.memory_mb);
        used.disk_gb = used.disk_gb.saturating_add(inst.spec.boot_disk.size_gb);
    }
    used
}

fn free<'a>(node: &NodeRow, instances: impl Iterator<Item = &'a InstanceRow>) -> Capacity {
    let total = admissible(node);
    let used = allocated(instances);
    Capacity {
        vcpus: total.vcpus.saturating_sub(used.vcpus),
        memory_mb: total.memory_mb.saturating_sub(used.memory_mb),
        disk_gb: total.disk_gb.saturating_sub(used.disk_gb),
    }
}

fn fits(free: &Capacity, spec: &InstanceSpec) -> bool {
    free.vcpus >= spec.vcpus && free.memory_mb >= spec.memory_mb && free.disk_gb >= spec.boot_disk.size_gb
}
