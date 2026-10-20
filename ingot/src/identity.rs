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

/// Persist the enrollment secret before registration, with owner-only access.
pub fn load_or_create_token(state_dir: &Path) -> anyhow::Result<String> {
    let path = state_dir.join("node-token");
    match std::fs::read_to_string(&path) {
        Ok(token) if copper_api::valid_node_token(&token) => Ok(token),
        Ok(_) => anyhow::bail!("invalid node credential at {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let token = format!("{}{}", NodeId::new_v4().simple(), NodeId::new_v4().simple());
            crate::state::write_atomic(&path, token.as_bytes())?;
            Ok(token)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn credential_survives_restart_and_is_private() {
        let dir = crate::test_support::TempDir::new();
        let token = load_or_create_token(dir.path()).unwrap();
        assert_eq!(token, load_or_create_token(dir.path()).unwrap());
        assert_eq!(std::fs::metadata(dir.path().join("node-token")).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
