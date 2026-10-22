//! Background chores: notice nodes that went quiet, and keep tables small.

use std::time::Duration;

use crate::db::{self, NewEvent, NodeDbStatus, OFFLINE_AFTER_SECS};
use crate::AppState;

const EVERY: Duration = Duration::from_secs(10);
/// The event log keeps this much history.
const KEEP_EVENTS_SECS: i64 = 90 * 24 * 3600;

pub async fn run(state: AppState) {
    let mut ticks: u64 = 0;
    let mut timer = tokio::time::interval(EVERY);
    loop {
        timer.tick().await;
        ticks += 1;
        let conn = state.db.lock().unwrap();
        if let Err(e) = mark_quiet_nodes(&conn) {
            tracing::warn!(error = %e, "checking for quiet nodes");
        }
        // Housekeeping about once an hour.
        if ticks % 360 == 1 {
            let now = db::now();
            if let Err(e) = db::prune_events(&conn, now - KEEP_EVENTS_SECS) {
                tracing::warn!(error = %e, "pruning events");
            }
            if let Err(e) = db::delete_expired_sessions(&conn, now) {
                tracing::warn!(error = %e, "pruning sessions");
            }
        }
    }
}

/// Log once when an approved node stops reporting. The node's own next
/// report logs "node back" and clears the flag.
pub(crate) fn mark_quiet_nodes(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let now = db::now();
    for node in db::list_nodes(conn)? {
        let silent = node.last_seen.is_some_and(|t| now - t > OFFLINE_AFTER_SECS);
        if node.status == NodeDbStatus::Approved && silent && !node.quiet {
            db::set_quiet(conn, node.id, true)?;
            let detail = format!("{} stopped reporting; its vms keep running as they were", node.hostname);
            db::record(conn, NewEvent::new("node quiet", detail).node(node.id))?;
            tracing::warn!(node = %node.id, host = %node.hostname, "node went quiet");
        }
    }
    Ok(())
}
