//! Node identity: a random UUID created on first start. A reinstall loses the
//! file and is therefore a new node, by design.

use std::path::Path;

use anyhow::Context;
use copper_api::NodeId;

const FILE: &str = "node-id";

pub fn load_or_create(state_dir: &Path) -> anyhow::Result<NodeId> {
    let path = state_dir.join(FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().parse().with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let id = NodeId::new_v4();
            crate::state::write_atomic(&path, id.to_string().as_bytes())?;
            tracing::info!(%id, "generated new node id");
            Ok(id)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}
