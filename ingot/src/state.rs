//! On-disk cache of the last desired state, so the agent can reconcile while
//! the controller is unreachable.

use std::path::Path;

use anyhow::Context;
use copper_api::DesiredNodeState;

const FILE: &str = "desired.json";

pub fn load_desired(state_dir: &Path) -> anyhow::Result<Option<DesiredNodeState>> {
    let path = state_dir.join(FILE);
    match std::fs::read(&path) {
        // A cache we can't parse (e.g. after a protocol change) is treated as
        // absent rather than fatal: kiln will send a fresh copy.
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(d) => Ok(Some(d)),
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "ignoring unreadable desired-state cache");
                Ok(None)
            }
        },
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
