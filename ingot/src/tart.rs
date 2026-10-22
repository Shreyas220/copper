//! macOS backend: Apple's Virtualization.framework through the Tart CLI,
//! with launchd standing in for libvirtd as the thing that keeps VMs alive
//! across ingot restarts and logins.
//!
//! Layout on the Mac:
//! - Tart keeps each VM under `~/.tart/vms/copper-<id>/` with a raw `disk.img`.
//! - The cloud-init seed lives in ingot's disks dir as `<id>-seed.iso`.
//! - A running VM has a LaunchAgent `~/Library/LaunchAgents/com.copper.vm.<id>.plist`
//!   whose job is `tart run`. The plist exists exactly while the VM should be
//!   running: start writes and bootstraps it, stop/destroy boot it out and
//!   remove it. RunAtLoad brings running VMs back after a reboot and login.
//!
//! Requirements on the node: `tart` (brew install cirruslabs/cli/tart),
//! `qemu-img` (brew install qemu) for converting cloud images, and an
//! aarch64 cloud image at `<images dir>/<name>.img`. The node must stay
//! logged in, as Virtualization.framework runs in a user session.
//!
//! UNTESTED: this module has only been type-checked on Linux. First run on a
//! Mac mini will shake out Tart flag details.

// On Linux with `--features tart` the module is compiled but never selected.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};
use copper_api::*;

use crate::disk::{run, user_data_yaml, Disks};
use crate::hv::{name_of, DomState, Hypervisor};

/// VMs run on Tart's own NAT: Tart assigns their MAC and address (read back
/// through `tart ip`), and ingot reaches them from the Mac. The address kiln
/// gives on the node's copper /24 waits until the Mac's vmnet subnet is set
/// to it.
pub struct Tart {
    disks: Disks,
    home: PathBuf,
    uid: u32,
}

impl Tart {
    pub fn new(disks: Disks) -> anyhow::Result<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from).context("HOME is not set")?;
        let uid = unsafe { libc::getuid() };
        if Command::new("tart").arg("--version").output().is_err() {
            bail!("tart not found; brew install cirruslabs/cli/tart");
        }
        std::fs::create_dir_all(&disks.dir)?;
        Ok(Self { disks, home, uid })
    }

    fn vm_dir(&self, id: InstanceId) -> PathBuf {
        self.home.join(".tart/vms").join(name_of(id))
    }
    fn seed_path(&self, id: InstanceId) -> PathBuf {
        self.disks.dir.join(format!("{id}-seed.iso"))
    }
    fn label(id: InstanceId) -> String {
        format!("com.copper.vm.{id}")
    }
    fn plist_path(&self, id: InstanceId) -> PathBuf {
        self.home.join("Library/LaunchAgents").join(format!("{}.plist", Self::label(id)))
    }

    /// `tart list --format json` entry for our VM, if it exists.
    fn list_entry(&self, id: InstanceId) -> anyhow::Result<Option<serde_json::Value>> {
        let out = Command::new("tart").args(["list", "--format", "json"]).output().context("running tart list")?;
        if !out.status.success() {
            bail!("tart list failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let entries: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).context("parsing tart list")?;
        let name = name_of(id);
        Ok(entries.into_iter().find(|e| e["Name"].as_str() == Some(&name)))
    }

    fn bootout(&self, id: InstanceId) -> anyhow::Result<()> {
        // Exit status is non-zero when the job isn't loaded; that's fine.
        let _ = Command::new("launchctl").args(["bootout", &format!("gui/{}/{}", self.uid, Self::label(id))]).output();
        let plist = self.plist_path(id);
        match std::fs::remove_file(&plist) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", plist.display())),
        }
    }

    fn ensure_seed(&self, inst: &DesiredInstance) -> anyhow::Result<PathBuf> {
        let path = self.seed_path(inst.id);
        if path.exists() {
            return Ok(path);
        }
        let stage = self.disks.dir.join(format!("{}-seed", inst.id));
        std::fs::create_dir_all(&stage)?;
        std::fs::write(stage.join("user-data"), user_data_yaml(&inst.name, &inst.spec))?;
        std::fs::write(stage.join("meta-data"), format!("instance-id: {}\nlocal-hostname: {}\n", inst.id, serde_json::to_string(&inst.name)?))?;
        let tmp = path.with_extension("iso.tmp");
        // hdiutil appends .iso itself when the output has no extension; give it the full name.
        run(Command::new("hdiutil")
            .args(["makehybrid", "-quiet", "-iso", "-joliet", "-iso-volume-name", "cidata", "-joliet-volume-name", "cidata", "-o"])
            .arg(&tmp)
            .arg(&stage))?;
        crate::state::publish_file(&tmp, &path)?;
        let _ = std::fs::remove_dir_all(stage);
        Ok(path)
    }

    fn plist(&self, id: InstanceId, seed: &Path) -> String {
        let args = [
            "tart", "run", &name_of(id), "--no-graphics",
            "--disk", &format!("{}:ro", seed.display()),
        ];
        let strings: String = args.iter().map(|a| format!("    <string>{}</string>\n", xml_escape(a))).collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{strings}  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            label = Self::label(id),
            log = xml_escape(&self.disks.dir.join(format!("{id}.log")).to_string_lossy()),
        )
    }
}

impl Hypervisor for Tart {
    fn node_capacity(&self) -> anyhow::Result<(u32, u64)> {
        let sysctl = |key: &str| -> anyhow::Result<u64> {
            let out = Command::new("sysctl").args(["-n", key]).output().with_context(|| format!("sysctl {key}"))?;
            String::from_utf8_lossy(&out.stdout).trim().parse().with_context(|| format!("parsing sysctl {key}"))
        };
        Ok((sysctl("hw.ncpu")? as u32, sysctl("hw.memsize")? / (1024 * 1024)))
    }

    fn allocatable_gb(&self, ids: &[InstanceId]) -> u64 {
        use std::os::unix::fs::MetadataExt;
        let vms = self.home.join(".tart/vms");
        let probe = if vms.exists() { vms } else { self.home.clone() };
        let c = std::ffi::CString::new(probe.to_string_lossy().as_bytes()).unwrap();
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return 0;
        }
        let mut allocated = 0u64;
        for id in ids {
            if let Ok(meta) = std::fs::metadata(self.vm_dir(*id).join("disk.img")) {
                allocated = allocated.saturating_add(meta.blocks().saturating_mul(512));
            }
        }
        ((st.f_bavail as u64).saturating_mul(st.f_frsize as u64).saturating_add(allocated)) / (1024 * 1024 * 1024)
    }

    fn prepare(&self, inst: &DesiredInstance) -> anyhow::Result<()> {
        if inst.spec.arch != Arch::Aarch64 {
            bail!("this node runs aarch64 guests only");
        }
        let name = name_of(inst.id);
        let dir = self.vm_dir(inst.id);
        if !dir.join("disk.img").exists() {
            let base = self.disks.base_image(&inst.spec.boot_disk)?;
            // Create the VM shell, then replace its empty raw disk with the
            // cloud image converted to raw and grown to the requested size.
            run(Command::new("tart").args(["create", &name, "--linux", "--disk-size", &inst.spec.boot_disk.size_gb.to_string()]))?;
            let disk = dir.join("disk.img");
            let tmp = dir.join("disk.img.tmp");
            run(Command::new("qemu-img").args(["convert", "-q", "-f", "qcow2", "-O", "raw"]).arg(&base).arg(&tmp))?;
            run(Command::new("qemu-img").args(["resize", "-q", "-f", "raw"]).arg(&tmp).arg(format!("{}G", inst.spec.boot_disk.size_gb)))?;
            crate::state::publish_file(&tmp, &disk)?;
            tracing::info!(instance = %inst.id, disk = %disk.display(), "boot disk created");
        }
        // vCPUs and memory are applied by `reconfigure` before every start.
        self.ensure_seed(inst)?;
        Ok(())
    }

    fn reconfigure(&self, inst: &DesiredInstance) -> anyhow::Result<()> {
        if self.state(inst.id)? == Some(DomState::Running) {
            bail!("vm is running; resize applies after it stops");
        }
        let name = name_of(inst.id);
        run(Command::new("tart").args(["set", &name, "--cpu", &inst.spec.vcpus.to_string(), "--memory", &inst.spec.memory_mb.to_string()]))?;
        let disk = self.vm_dir(inst.id).join("disk.img");
        let want = inst.spec.boot_disk.size_gb * (1 << 30);
        if std::fs::metadata(&disk).with_context(|| format!("reading {}", disk.display()))?.len() < want {
            run(Command::new("qemu-img").args(["resize", "-q", "-f", "raw"]).arg(&disk).arg(format!("{}G", inst.spec.boot_disk.size_gb)))?;
            tracing::info!(instance = %inst.id, size_gb = inst.spec.boot_disk.size_gb, "boot disk grown");
        }
        Ok(())
    }

    fn has_storage(&self, id: InstanceId) -> bool {
        self.vm_dir(id).exists() || self.seed_path(id).exists()
    }

    fn remove_storage(&self, id: InstanceId) -> anyhow::Result<()> {
        if self.vm_dir(id).exists() {
            run(Command::new("tart").args(["delete", &name_of(id)]))?;
            tracing::info!(instance = %id, "vm deleted");
        }
        let seed = self.seed_path(id);
        match std::fs::remove_file(&seed) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", seed.display())),
        }
        if self.has_storage(id) {
            bail!("storage still present after removal");
        }
        Ok(())
    }

    /// `prepare` already registered the VM with Tart; nothing persistent to add
    /// until it should run, which is what `start` does.
    fn define(&self, _: &DesiredInstance) -> anyhow::Result<()> {
        Ok(())
    }

    fn start(&self, id: InstanceId) -> anyhow::Result<()> {
        if self.state(id)? == Some(DomState::Running) {
            return Ok(());
        }
        let plist = self.plist_path(id);
        std::fs::create_dir_all(plist.parent().unwrap())?;
        crate::state::write_atomic(&plist, self.plist(id, &self.seed_path(id)).as_bytes())?;
        run(Command::new("launchctl").args(["bootstrap", &format!("gui/{}", self.uid)]).arg(&plist))?;
        tracing::info!(instance = %id, "vm started");
        Ok(())
    }

    fn shutdown(&self, id: InstanceId) -> anyhow::Result<()> {
        if self.state(id)? != Some(DomState::Running) {
            return Ok(());
        }
        // Graceful stop can take a while; don't hold the reconciler for it.
        // The reconciler escalates to `destroy` after its own deadline.
        Command::new("tart").args(["stop", &name_of(id), "--timeout", "55"]).spawn().context("spawning tart stop")?;
        Ok(())
    }

    fn destroy(&self, id: InstanceId) -> anyhow::Result<()> {
        self.bootout(id)?;
        if self.state(id)? == Some(DomState::Running) {
            run(Command::new("tart").args(["stop", &name_of(id), "--timeout", "1"]))?;
        }
        tracing::info!(instance = %id, "vm destroyed");
        Ok(())
    }

    fn undefine(&self, id: InstanceId) -> anyhow::Result<()> {
        self.destroy(id)
    }

    fn state(&self, id: InstanceId) -> anyhow::Result<Option<DomState>> {
        Ok(self.list_entry(id)?.map(|e| {
            if e["Running"].as_bool() == Some(true) || e["State"].as_str() == Some("running") {
                DomState::Running
            } else {
                DomState::Off
            }
        }))
    }

    fn addresses(&self, id: InstanceId) -> Vec<String> {
        let out = Command::new("tart").args(["ip", &name_of(id), "--wait", "1"]).output();
        match out {
            Ok(o) if o.status.success() => {
                let ip = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if ip.is_empty() { vec![] } else { vec![ip] }
            }
            _ => vec![],
        }
    }
}

fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        .replace('\"', "&quot;").replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_tart_with_seed_on_its_own_nat() {
        let t = Tart {
            disks: Disks { dir: "/tmp/copper".into(), images: "/tmp/copper/images".into() },
            home: "/Users/copper".into(),
            uid: 501,
        };
        let id = InstanceId::nil();
        let p = t.plist(id, &t.seed_path(id));
        assert!(p.contains("<string>com.copper.vm.00000000-0000-0000-0000-000000000000</string>"));
        assert!(p.contains("<string>copper-00000000-0000-0000-0000-000000000000</string>"));
        assert!(p.contains("<string>/tmp/copper/00000000-0000-0000-0000-000000000000-seed.iso:ro</string>"));
        assert!(!p.contains("--net-bridged"), "not on the site's LAN");
        assert_eq!(t.plist_path(id), PathBuf::from("/Users/copper/Library/LaunchAgents/com.copper.vm.00000000-0000-0000-0000-000000000000.plist"));
    }
}
