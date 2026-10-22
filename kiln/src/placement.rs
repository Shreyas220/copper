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

fn size_of(spec: &InstanceSpec) -> Capacity {
    Capacity { vcpus: spec.vcpus, memory_mb: spec.memory_mb, disk_gb: spec.boot_disk.size_gb }
}

// ---- quotas ------------------------------------------------------------------

/// What a project's instances hold, stopped and deleting ones included.
pub fn project_usage(conn: &Connection, project: db::Uuid) -> Result<Capacity> {
    Ok(allocated(db::list_instances_in_project(conn, project)?.iter()))
}

/// Why `extra` more would break the project's caps, if it would. `release`
/// is what an instance being resized already holds.
pub fn quota_refusal(conn: &Connection, project: &db::Project, spec: &InstanceSpec, release: Option<InstanceId>) -> Result<Option<String>> {
    let instances = db::list_instances_in_project(conn, project.id)?;
    let used = allocated(instances.iter().filter(|i| Some(i.id) != release));
    let want = size_of(spec);
    let caps = &project.quota;
    let over = |cap: Option<u64>, used: u64, want: u64, what: &str| {
        cap.filter(|cap| used + want > *cap).map(|cap| format!("project {} is capped at {cap} {what} and holds {used}", project.name))
    };
    Ok(over(caps.vcpus.map(u64::from), used.vcpus as u64, want.vcpus as u64, "vcpu")
        .or_else(|| over(caps.memory_mb, used.memory_mb, want.memory_mb, "MB of memory"))
        .or_else(|| over(caps.disk_gb, used.disk_gb, want.disk_gb, "GB of disk")))
}

// ---- dry runs ------------------------------------------------------------------

/// Would this instance be placed? The same rules as `choose`, without
/// creating anything. Per-node detail is for admins only.
pub fn check(conn: &Connection, project: &db::Project, spec: &InstanceSpec, hints: &Hints, admin: bool) -> Result<FitCheck> {
    let mut largest = Capacity { vcpus: 0, memory_mb: 0, disk_gb: 0 };
    let mut nodes = vec![];
    for node in db::list_nodes(conn)? {
        if node.arch != spec.arch || node.status != db::NodeDbStatus::Approved {
            continue;
        }
        let free = free(&node, db::list_instances_on_node(conn, node.id)?.iter());
        let eligible = node.online() && hints.node.is_none_or(|pinned| pinned == node.id);
        if eligible {
            largest.vcpus = largest.vcpus.max(free.vcpus);
            largest.memory_mb = largest.memory_mb.max(free.memory_mb);
            largest.disk_gb = largest.disk_gb.max(free.disk_gb);
        }
        nodes.push(NodeFit { node: node.id, hostname: node.hostname.clone(), status: node.public_status(), fits: eligible && fits(&free, spec), free });
    }
    let reason = if let Some(r) = quota_refusal(conn, project, spec, None)? {
        Some(r)
    } else if !nodes.iter().any(|n| n.status == NodeStatus::Online) {
        Some(format!("no {} node is online", arch_name(spec.arch)))
    } else if !nodes.iter().any(|n| n.fits) {
        Some(format!(
            "won't fit right now. the most any one machine can give is {} vcpu, {} MB of memory and {} GB of disk",
            largest.vcpus, largest.memory_mb, largest.disk_gb
        ))
    } else {
        None
    };
    Ok(FitCheck { fits: reason.is_none(), reason, largest, nodes: admin.then_some(nodes) })
}

pub fn arch_name(arch: Arch) -> &'static str {
    match arch {
        Arch::X86_64 => "x86_64",
        Arch::Aarch64 => "aarch64",
    }
}
