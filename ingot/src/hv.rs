//! The hypervisor seam. The reconciler drives an instance through this trait
//! and never learns what a disk image, a domain or a launchd job is. Each
//! backend owns its storage layout as well as its VM lifecycle, because those
//! differ between platforms (qcow2 overlays under libvirt, raw images under
//! Apple's Virtualization.framework).
//!
//! Backends:
//! - `libvirt`: Linux, KVM through libvirt. The one in production.
//! - `tart`: macOS on Apple silicon, through the Tart CLI and launchd.

use copper_api::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomState {
    Running,
    /// Defined but not running (shut off, crashed, or never started).
    Off,
    /// Mid-transition, e.g. shutting down.
    Other,
}

/// Every method is idempotent and safe to call again after a crash.
pub trait Hypervisor {
    /// Host CPU threads and memory in MB, before any reserve.
    fn node_capacity(&self) -> anyhow::Result<(u32, u64)>;
    /// Disk budget in GB: free space plus the blocks already allocated to the
    /// given instances' storage, so kiln can subtract reservations once.
    fn allocatable_gb(&self, ids: &[InstanceId]) -> u64;

    /// Create the instance's storage (boot disk, cloud-init seed) if missing.
    fn prepare(&self, inst: &DesiredInstance) -> anyhow::Result<()>;
    /// Whether any storage for the instance still exists on this node.
    fn has_storage(&self, id: InstanceId) -> bool;
    /// Remove the instance's storage. Final. Must fail if anything remains.
    fn remove_storage(&self, id: InstanceId) -> anyhow::Result<()>;

    /// Register the VM with the platform so it persists across restarts of
    /// ingot and the host. Storage must already be prepared.
    fn define(&self, inst: &DesiredInstance) -> anyhow::Result<()>;
    fn start(&self, id: InstanceId) -> anyhow::Result<()>;
    /// Ask the guest to shut down cleanly.
    fn shutdown(&self, id: InstanceId) -> anyhow::Result<()>;
    /// Pull the plug.
    fn destroy(&self, id: InstanceId) -> anyhow::Result<()>;
    /// Forget the VM. Storage is handled separately by `remove_storage`.
    fn undefine(&self, id: InstanceId) -> anyhow::Result<()>;
    /// Bring a defined VM that is not running up to the spec's vCPUs, memory
    /// and disk size. Disks only grow; the guest's cloud-init grows its root
    /// filesystem on the next boot. Only called while the VM is off.
    fn reconfigure(&self, inst: &DesiredInstance) -> anyhow::Result<()>;

    /// `Ok(None)` when the platform has no such VM. `Err` when the platform
    /// itself can't be asked, which must never be mistaken for absence.
    fn state(&self, id: InstanceId) -> anyhow::Result<Option<DomState>>;
    fn addresses(&self, id: InstanceId) -> Vec<String>;

    /// Whether VMs here live on the node's copper network, at the addresses
    /// kiln gives them. Tart keeps its own NAT for now.
    fn copper_network(&self) -> bool {
        false
    }
    /// Set up the node's copper network if it isn't. Idempotent.
    fn ensure_network(&self, _net: &NodeNetwork) -> anyhow::Result<()> {
        Ok(())
    }
    /// Whether the host side of the copper network is in place, when the
    /// platform has one.
    fn network_ready(&self) -> Option<bool> {
        None
    }

    /// Live use of one running VM, when the platform can measure it.
    fn usage(&self, _id: InstanceId) -> Option<Usage> {
        None
    }
    /// Live use of the whole host.
    fn host_usage(&self) -> Option<Usage> {
        None
    }
}

pub const PREFIX: &str = "copper-";

pub fn name_of(id: InstanceId) -> String {
    format!("{PREFIX}{id}")
}

#[cfg(not(target_os = "macos"))]
pub type Native = crate::libvirt::Libvirt;
#[cfg(target_os = "macos")]
pub type Native = crate::tart::Tart;
