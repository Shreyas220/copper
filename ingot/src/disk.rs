//! Boot disks and cloud-init seeds. A boot disk is a qcow2 overlay on a base
//! image; the seed is a small ISO that cloud-init reads on first boot.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};
use copper_api::*;

#[derive(Default)]
pub struct Disks {
    pub dir: PathBuf,
    pub images: PathBuf,
}

impl Disks {
    pub fn disk_path(&self, id: InstanceId) -> PathBuf {
        self.dir.join(format!("{id}.qcow2"))
    }
    pub fn seed_path(&self, id: InstanceId) -> PathBuf {
        self.dir.join(format!("{id}-seed.iso"))
    }
    pub fn image_path(&self, name: &str) -> anyhow::Result<PathBuf> {
        if !valid_image_name(name) { bail!("invalid image name") }
        let root = self.images.canonicalize().context("opening images directory")?;
        let image = root.join(format!("{name}.img")).canonicalize().context("opening base image")?;
        if !image.starts_with(&root) || !image.is_file() { bail!("image escapes images directory or is not a file") }
        Ok(image)
    }

    /// Create the overlay if it does not exist. Idempotent.
    pub fn ensure_disk(&self, id: InstanceId, spec: &InstanceSpec) -> anyhow::Result<PathBuf> {
        let path = self.disk_path(id);
        if path.exists() {
            return Ok(path);
        }
        let base = self.image_path(&spec.boot_disk.image)?;
        std::fs::create_dir_all(&self.dir)?;
        // Write to a temp name and rename, so a crash never leaves a half-made
        // disk that looks real.
        let tmp = path.with_extension("qcow2.tmp");
        run(Command::new("qemu-img").args(["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b"]).arg(&base).arg(&tmp).arg(format!("{}G", spec.boot_disk.size_gb)))?;
        crate::state::publish_file(&tmp, &path)?;
        tracing::info!(instance = %id, disk = %path.display(), "boot disk created");
        Ok(path)
    }

    /// Create the cloud-init seed if it does not exist. Idempotent.
    pub fn ensure_seed(&self, id: InstanceId, name: &str, spec: &InstanceSpec) -> anyhow::Result<PathBuf> {
        let path = self.seed_path(id);
        if path.exists() {
            return Ok(path);
        }
        std::fs::create_dir_all(&self.dir)?;
        let user_data = self.dir.join(format!("{id}-user-data"));
        let meta_data = self.dir.join(format!("{id}-meta-data"));
        std::fs::write(&user_data, user_data_yaml(name, spec))?;
        std::fs::write(&meta_data, format!("instance-id: {id}\nlocal-hostname: {}\n", serde_json::to_string(name)?))?;
        let tmp = path.with_extension("iso.tmp");
        run(Command::new("cloud-localds").arg(&tmp).arg(&user_data).arg(&meta_data))?;
        crate::state::publish_file(&tmp, &path)?;
        let _ = std::fs::remove_file(user_data);
        let _ = std::fs::remove_file(meta_data);
        Ok(path)
    }

    pub fn disk_exists(&self, id: InstanceId) -> bool {
        self.disk_path(id).exists()
    }

    /// Remove disk and seed. Final.
    pub fn remove(&self, id: InstanceId) -> anyhow::Result<()> {
        for p in [self.disk_path(id), self.seed_path(id)] {
            match std::fs::remove_file(&p) {
                Ok(()) => tracing::info!(instance = %id, file = %p.display(), "removed"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", p.display())),
            }
        }
        if self.dir.exists() { std::fs::File::open(&self.dir)?.sync_all()?; }
        Ok(())
    }

    /// Physical free bytes plus Copper's allocated blocks. Kiln subtracts
    /// virtual reservations once; unrelated host usage reduces this budget.
    pub fn allocatable_gb(&self, ids: impl Iterator<Item = InstanceId>) -> u64 {
        use std::os::unix::fs::MetadataExt;
        let probe = if self.dir.exists() { self.dir.clone() } else { self.dir.parent().map(Path::to_path_buf).unwrap_or(self.dir.clone()) };
        let c = std::ffi::CString::new(probe.to_string_lossy().as_bytes()).unwrap();
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return 0;
        }
        let mut allocated = 0u64;
        for id in ids {
            match std::fs::symlink_metadata(self.disk_path(id)) {
                Ok(meta) if meta.is_file() => allocated = allocated.saturating_add(meta.blocks().saturating_mul(512)),
                Ok(_) => return 0,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                Err(_) => return 0,
            }
        }
        (st.f_bavail.saturating_mul(st.f_frsize).saturating_add(allocated)) / (1024 * 1024 * 1024)
    }
}

pub fn user_data_yaml(name: &str, spec: &InstanceSpec) -> String {
    let mut y = String::from("#cloud-config\n");
    y.push_str(&format!("hostname: {}\n", serde_json::to_string(name).unwrap()));
    y.push_str("manage_etc_hosts: true\n");
    // The guest agent lets ingot read addresses straight from the guest.
    y.push_str("packages:\n  - qemu-guest-agent\n");
    y.push_str("runcmd:\n  - [systemctl, enable, --now, qemu-guest-agent]\n");
    if !spec.ssh_keys.is_empty() {
        y.push_str("ssh_authorized_keys:\n");
        for k in &spec.ssh_keys {
            y.push_str(&format!("  - {}\n", serde_json::to_string(k).unwrap()));
        }
    }
    y
}

pub fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let out = cmd.output().with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !out.status.success() {
        bail!("{:?} failed: {}", cmd.get_program(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn image_paths_reject_traversal_and_symlink_escape() {
        let dir = TempDir::new();
        let images = dir.path().join("images");
        std::fs::create_dir(&images).unwrap();
        std::fs::write(dir.path().join("private.img"), "data").unwrap();
        std::os::unix::fs::symlink(dir.path().join("private.img"), images.join("escape.img")).unwrap();
        let disks = Disks { dir: dir.path().into(), images };
        assert!(disks.image_path("../private").is_err());
        assert!(disks.image_path("escape").is_err());
        std::fs::write(disks.images.join("ubuntu-24.04.img"), "image").unwrap();
        assert!(disks.image_path("ubuntu-24.04").is_ok());
    }
    #[test]
    fn cloud_init_values_are_quoted_without_newline_injection() {
        let spec = InstanceSpec { vcpus: 1, memory_mb: 1024, arch: Arch::X86_64,
            boot_disk: BootDisk { image: "ubuntu".into(), size_gb: 10 }, nics: vec![],
            ssh_keys: vec!["ssh-ed25519 key\nruncmd:\n  - malicious".into()] };
        let data = user_data_yaml("vm\nusers: []", &spec);
        assert!(!data.contains("\nusers:"));
        assert_eq!(data.lines().filter(|line| *line == "runcmd:").count(), 1);
        let line = data.lines().find(|line| line.starts_with("  - \"ssh-")).unwrap();
        let key: String = serde_json::from_str(line.strip_prefix("  - ").unwrap()).unwrap();
        assert_eq!(key, spec.ssh_keys[0]);
    }
}
