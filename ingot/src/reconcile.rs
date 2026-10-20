//! Drive the hypervisor toward a desired state. Idempotent: calling it twice
//! with the same input does nothing the second time.
//!
//! v1 talks to libvirt at qemu:///system. Only domains carrying copper
//! metadata are touched; everything else on the host is invisible to us.

use copper_api::DesiredNodeState;

pub async fn run(desired: &DesiredNodeState) -> anyhow::Result<()> {
    tracing::info!(generation = desired.generation, instances = desired.instances.len(), "reconcile");
    // TODO (needs libvirt-dev + `virt` crate):
    //   for each desired instance:
    //     Running -> ensure disk exists (qcow2 overlay), domain defined, domain running
    //     Stopped -> ensure domain defined, not running
    //     Absent  -> destroy, undefine, delete disk
    //   for each copper-tagged domain not in desired: leave it, report it.
    Ok(())
}
