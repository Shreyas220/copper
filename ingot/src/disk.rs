//! Boot disks and cloud-init seeds. A boot disk is a qcow2 overlay on a base
//! image; the seed is a small ISO that cloud-init reads on first boot.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};
use copper_api::*;

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
    pub fn image_path(&self, name: &str) -> PathBuf {
        self.images.join(format!("{name}.img"))
    }

    /// Create the overlay if it does not exist. Idempotent.
    pub fn ensure_disk(&self, id: InstanceId, spec: &InstanceSpec) -> anyhow::Result<PathBuf> {
        let path = self.disk_path(id);
        if path.exists() {
            return Ok(path);
        }
        let base = self.image_path(&spec.boot_disk.image);
        if !base.exists() {
            bail!("image {} not found at {}", spec.boot_disk.image, base.display());
        }
        std::fs::create_dir_all(&self.dir)?;
        // Write to a temp name and rename, so a crash never leaves a half-made
        // disk that looks real.
        let tmp = path.with_extension("qcow2.tmp");
        run(Command::new("qemu-img").args(["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b"]).arg(&base).arg(&tmp).arg(format!("{}G", spec.boot_disk.size_gb)))?;
        std::fs::rename(&tmp, &path)?;
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
        std::fs::write(&meta_data, format!("instance-id: {id}\nlocal-hostname: {name}\n"))?;
        let tmp = path.with_extension("iso.tmp");
        run(Command::new("cloud-localds").arg(&tmp).arg(&user_data).arg(&meta_data))?;
        std::fs::rename(&tmp, &path)?;
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
        Ok(())
    }

    /// Free space on the disk directory's filesystem, in GB.
    pub fn free_gb(&self) -> u64 {
        let probe = if self.dir.exists() { self.dir.clone() } else { self.dir.parent().map(Path::to_path_buf).unwrap_or(self.dir.clone()) };
        let c = std::ffi::CString::new(probe.to_string_lossy().as_bytes()).unwrap();
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return 0;
        }
        (st.f_bavail as u64 * st.f_frsize as u64) / (1024 * 1024 * 1024)
    }
}

fn user_data_yaml(name: &str, spec: &InstanceSpec) -> String {
    let mut y = String::from("#cloud-config\n");
    y.push_str(&format!("hostname: {name}\n"));
    y.push_str("manage_etc_hosts: true\n");
    // The guest agent lets ingot read addresses straight from the guest.
    y.push_str("packages:\n  - qemu-guest-agent\n");
    y.push_str("runcmd:\n  - [systemctl, enable, --now, qemu-guest-agent]\n");
    if !spec.ssh_keys.is_empty() {
        y.push_str("ssh_authorized_keys:\n");
        for k in &spec.ssh_keys {
            y.push_str(&format!("  - {k}\n"));
        }
    }
    y
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let out = cmd.output().with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !out.status.success() {
        bail!("{:?} failed: {}", cmd.get_program(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}
