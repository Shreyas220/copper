//! Choose a node for a new instance, or refuse immediately.
//!
//! Free capacity is a node's reported capacity minus the sum of every instance
//! still assigned to it (anything not desired-absent counts, whether running
//! or stopped, because a stopped instance still owns its disk and may start).

use copper_api::*;
use rusqlite::Connection;

use crate::db::{self, NodeRow};
use crate::error::{Error, Result};

pub fn choose(conn: &Connection, spec: &InstanceSpec) -> Result<NodeRow> {
    let mut best: Option<(NodeRow, Capacity)> = None;
    for node in db::list_nodes(conn)? {
        if !node.online() || node.arch != spec.arch {
            continue;
        }
        let free = free_capacity(conn, &node)?;
        if free.vcpus < spec.vcpus || free.memory_mb < spec.memory_mb || free.disk_gb < spec.boot_disk.size_gb {
            continue;
        }
        // Prefer the node with the most free memory; simplest useful spread.
        if best.as_ref().is_none_or(|(_, b)| free.memory_mb > b.memory_mb) {
            best = Some((node, free));
        }
    }
    best.map(|(n, _)| n).ok_or_else(|| Error::conflict("no node has capacity for this instance"))
}

pub fn free_capacity(conn: &Connection, node: &NodeRow) -> Result<Capacity> {
    let mut free = node.capacity.clone();
    for inst in db::list_instances_on_node(conn, node.id)? {
        if inst.desired == DesiredState::Absent {
            continue;
        }
        free.vcpus = free.vcpus.saturating_sub(inst.spec.vcpus);
        free.memory_mb = free.memory_mb.saturating_sub(inst.spec.memory_mb);
        free.disk_gb = free.disk_gb.saturating_sub(inst.spec.boot_disk.size_gb);
    }
    Ok(free)
}
