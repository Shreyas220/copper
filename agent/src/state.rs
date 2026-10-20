//! On-disk cache of the last desired state, so the agent can reconcile while
//! the controller is unreachable.

use std::path::Path;

use anyhow::Context;
use homecloud_api::DesiredNodeState;

const FILE: &str = "desired.json";

pub fn load_desired(state_dir: &Path) -> anyhow::Result<Option<DesiredNodeState>> {
    let path = state_dir.join(FILE);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save_desired(state_dir: &Path, desired: &DesiredNodeState) -> anyhow::Result<()> {
    write_atomic(&state_dir.join(FILE), &serde_json::to_vec_pretty(desired)?)
}

/// Write to a temp file, fsync, rename. The file is always either the old or
/// the new version, never torn.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}
