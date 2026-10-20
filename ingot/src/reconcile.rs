//! Drive the hypervisor toward a desired state. Idempotent: calling it twice
//! with the same input does nothing the second time.
//!
//! Failures are retried with backoff for `FAIL_BUDGET`; after that the
//! instance is reported `Failed` with the last error until the desired state
//! changes, which resets the budget. Deletion retries indefinitely because
//! desired-absent is irreversible and cleanup must eventually complete.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use copper_api::*;

use crate::disk::Disks;
use crate::hv::{DomState, Hypervisor, Libvirt};

const FAIL_BUDGET: Duration = Duration::from_secs(5 * 60);
const RETRY_EVERY: Duration = Duration::from_secs(10);
/// How long to wait for a guest to honour ACPI shutdown before pulling the plug.
const GRACEFUL_STOP: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct Failure {
    since: Instant,
    last_try: Instant,
    error: String,
    /// The desired state that was being pursued when this started failing.
    desired: DesiredState,
}

impl Failure {
    fn exhausted(&self) -> bool {
        self.since.elapsed() >= FAIL_BUDGET
    }
}

pub struct Reconciler<H = Libvirt> {
    hv: H,
    disks: Disks,
    reserve: Capacity,
    failures: HashMap<InstanceId, Failure>,
    stop_requested: HashMap<InstanceId, Instant>,
    /// Snapshot whose generation is paired with observations under the same lock.
    desired: DesiredNodeState,
}

impl<H: Hypervisor> Reconciler<H> {
    pub fn new(hv: H, disks_dir: PathBuf, images_dir: PathBuf, reserve: Capacity) -> Self {
        Self {
            hv,
            disks: Disks { dir: disks_dir, images: images_dir },
            reserve,
            failures: HashMap::new(),
            stop_requested: HashMap::new(),
            desired: DesiredNodeState { generation: 0, instances: vec![] },
        }
    }

    pub fn capacity(&self) -> Capacity {
        self.capacity_for(&self.desired)
    }

    /// Cached inventory may account for existing blocks without authorizing actions.
    pub fn capacity_for(&self, inventory: &DesiredNodeState) -> Capacity {
        let (cpus, mem_mb) = self.hv.node_capacity().unwrap_or((0, 0));
        Capacity {
            vcpus: cpus.saturating_sub(self.reserve.vcpus),
            memory_mb: mem_mb.saturating_sub(self.reserve.memory_mb),
            disk_gb: self.disks.allocatable_gb(inventory.instances.iter().map(|i| i.id)).saturating_sub(self.reserve.disk_gb),
        }
    }

    pub fn reconcile(&mut self, desired: &DesiredNodeState) {
        self.desired = desired.clone();
        for inst in &desired.instances {
            // A change of desired state wipes the failure history for that instance.
            if self.failures.get(&inst.id).is_some_and(|f| f.desired != inst.state) {
                self.failures.remove(&inst.id);
            }
            if let Some(f) = self.failures.get(&inst.id) {
                if (inst.state != DesiredState::Absent && f.exhausted()) || f.last_try.elapsed() < RETRY_EVERY {
                    continue;
                }
            }
            let result = match inst.state {
                DesiredState::Running => self.ensure_running(inst),
                DesiredState::Stopped => self.ensure_stopped(inst.id),
                DesiredState::Absent => self.ensure_absent(inst.id),
            };
            match result {
                Ok(()) => {
                    self.failures.remove(&inst.id);
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    let now = Instant::now();
                    let f = self.failures.entry(inst.id).or_insert_with(|| Failure { since: now, last_try: now, error: msg.clone(), desired: inst.state });
                    f.last_try = now;
                    f.error = msg.clone();
                    if inst.state == DesiredState::Absent {
                        tracing::warn!(instance = %inst.id, error = %msg, "delete failed; will keep retrying");
                    } else if f.exhausted() {
                        tracing::error!(instance = %inst.id, error = %msg, "giving up until desired state changes");
                    } else {
                        tracing::warn!(instance = %inst.id, error = %msg, "reconcile failed; will retry");
                    }
                }
            }
        }
        // Forget bookkeeping for instances kiln no longer mentions.
        let known: std::collections::HashSet<_> = desired.instances.iter().map(|i| i.id).collect();
        self.failures.retain(|id, _| known.contains(id));
        self.stop_requested.retain(|id, _| known.contains(id));

    }

    fn ensure_running(&mut self, inst: &DesiredInstance) -> anyhow::Result<()> {
        self.stop_requested.remove(&inst.id);
        let disk = self.disks.ensure_disk(inst.id, &inst.spec)?;
        let seed = self.disks.ensure_seed(inst.id, &inst.name, &inst.spec)?;
        self.hv.define(inst.id, &inst.spec, &disk, &seed)?;
        self.hv.start(inst.id)
    }

    fn ensure_stopped(&mut self, id: InstanceId) -> anyhow::Result<()> {
        match self.hv.state(id)? {
            None | Some(DomState::Off) => {
                self.stop_requested.remove(&id);
                Ok(())
            }
            Some(_) => {
                let since = *self.stop_requested.entry(id).or_insert_with(Instant::now);
                if since.elapsed() >= GRACEFUL_STOP {
                    tracing::warn!(instance = %id, "guest ignored shutdown; destroying");
                    self.hv.destroy(id)
                } else {
                    self.hv.shutdown(id)
                }
            }
        }
    }

    fn ensure_absent(&mut self, id: InstanceId) -> anyhow::Result<()> {
        self.hv.undefine(id)?;
        self.disks.remove(id)?;
        self.stop_requested.remove(&id);
        Ok(())
    }

    /// Report every desired instance, including deletions completed before a restart.
    pub fn observe(&self) -> ObservedNodeState {
        let instances = self.desired.instances.iter().map(|inst| {
            let id = inst.id;
            let failure = self.failures.get(&id);
            let (state, error) = match (self.hv.state(id), failure) {
                (Err(e), _) => (ObservedState::Failed, Some(format!("{e:#}"))),
                (_, Some(f)) if f.exhausted() => (ObservedState::Failed, Some(f.error.clone())),
                (Ok(Some(DomState::Running)), _) if self.stop_requested.contains_key(&id) => (ObservedState::Stopping, None),
                (Ok(Some(DomState::Running)), _) => (ObservedState::Running, None),
                (Ok(Some(DomState::Other)), _) => (ObservedState::Stopping, None),
                (Ok(Some(DomState::Off)), _) => (ObservedState::Stopped, None),
                // Never acknowledge deletion while disk cleanup is failing, even
                // when the domain and boot disk have already been removed.
                (Ok(None), Some(f)) if inst.state == DesiredState::Absent => (ObservedState::Stopping, Some(f.error.clone())),
                (Ok(None), _) if self.disks.disk_exists(id) => (ObservedState::Stopped, None),
                (Ok(None), _) => (ObservedState::Absent, None),
            };
            let addresses = if state == ObservedState::Running { self.hv.addresses(id) } else { vec![] };
            ObservedInstance { id, state, error, addresses }
        }).collect();
        ObservedNodeState { generation_seen: self.desired.generation, instances, capacity: self.capacity() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use crate::test_support::TempDir;

    #[derive(Default)]
    struct Fake {
        state: RefCell<Option<DomState>>,
        fail_lookup: bool,
        fail_delete: bool,
        fail_start: bool,
        calls: RefCell<Vec<&'static str>>,
    }
    impl Hypervisor for Fake {
        fn node_capacity(&self) -> anyhow::Result<(u32, u64)> { Ok((8, 16384)) }
        fn state(&self, _: InstanceId) -> anyhow::Result<Option<DomState>> {
            if self.fail_lookup { anyhow::bail!("libvirt disconnected") }
            Ok(*self.state.borrow())
        }
        fn define(&self, _: InstanceId, _: &InstanceSpec, _: &std::path::Path, _: &std::path::Path) -> anyhow::Result<()> { Ok(()) }
        fn start(&self, _: InstanceId) -> anyhow::Result<()> {
            self.calls.borrow_mut().push("start");
            if self.fail_start { anyhow::bail!("start failed") }
            *self.state.borrow_mut() = Some(DomState::Running);
            Ok(())
        }
        fn shutdown(&self, _: InstanceId) -> anyhow::Result<()> { self.calls.borrow_mut().push("shutdown"); Ok(()) }
        fn destroy(&self, _: InstanceId) -> anyhow::Result<()> {
            self.calls.borrow_mut().push("destroy");
            *self.state.borrow_mut() = Some(DomState::Off);
            Ok(())
        }
        fn undefine(&self, _: InstanceId) -> anyhow::Result<()> {
            if self.fail_delete || self.fail_lookup { anyhow::bail!("cannot confirm domain removal") }
            *self.state.borrow_mut() = None;
            Ok(())
        }
        fn addresses(&self, _: InstanceId) -> Vec<String> { vec![] }
    }
    fn snapshot(state: DesiredState) -> DesiredNodeState {
        DesiredNodeState { generation: 7, instances: vec![DesiredInstance {
            id: InstanceId::new_v4(), name: "vm".into(), state,
            spec: InstanceSpec {
                vcpus: 1, memory_mb: 1024, arch: Arch::X86_64,
                boot_disk: BootDisk { image: "ubuntu".into(), size_gb: 10 },
                nics: vec![], ssh_keys: vec![],
            },
        }] }
    }
    fn reconciler(dir: &TempDir, hv: Fake) -> Reconciler<Fake> {
        Reconciler::new(hv, dir.path().to_owned(), dir.path().to_owned(), Capacity { vcpus: 0, memory_mb: 0, disk_gb: 0 })
    }
    #[test]
    fn stopped_instance_deleted_after_restart_is_reported_repeatedly() {
        let dir = TempDir::new();
        let mut r = reconciler(&dir, Fake::default());
        let mut d = snapshot(DesiredState::Stopped);
        r.reconcile(&d);
        d.instances[0].state = DesiredState::Absent;
        d.generation += 1;
        r.reconcile(&d);
        for _ in 0..2 {
            let report = r.observe();
            assert_eq!(report.generation_seen, 8);
            assert_eq!(report.instances[0].state, ObservedState::Absent);
        }
        // A new process also acknowledges an already completed deletion.
        let mut restarted = reconciler(&dir, Fake::default());
        restarted.reconcile(&d);
        assert_eq!(restarted.observe().instances[0].state, ObservedState::Absent);
    }
    #[test]
    fn hypervisor_failure_preserves_disk_and_never_acknowledges_absent() {
        let dir = TempDir::new();
        let mut r = reconciler(&dir, Fake { fail_lookup: true, ..Fake::default() });
        let d = snapshot(DesiredState::Absent);
        let path = r.disks.disk_path(d.instances[0].id);
        std::fs::write(&path, "valuable disk").unwrap();
        r.reconcile(&d);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "valuable disk");
        assert_eq!(r.observe().instances[0].state, ObservedState::Failed);
    }
    #[test]
    fn partial_cleanup_is_not_acknowledged() {
        let dir = TempDir::new();
        let mut r = reconciler(&dir, Fake::default());
        let d = snapshot(DesiredState::Absent);
        // An unremovable seed simulates failure after boot-disk removal.
        std::fs::create_dir(r.disks.seed_path(d.instances[0].id)).unwrap();
        r.reconcile(&d);
        assert_ne!(r.observe().instances[0].state, ObservedState::Absent);
    }
    #[test]
    fn exhausted_start_can_be_reset_by_stop_then_start() {
        let dir = TempDir::new();
        let mut r = reconciler(&dir, Fake { fail_start: true, ..Fake::default() });
        let mut d = snapshot(DesiredState::Running);
        let id = d.instances[0].id;
        std::fs::write(r.disks.disk_path(id), "disk").unwrap();
        std::fs::write(r.disks.seed_path(id), "seed").unwrap();
        r.reconcile(&d);
        r.failures.get_mut(&id).unwrap().since = Instant::now() - FAIL_BUDGET;
        r.reconcile(&d);
        assert_eq!(r.hv.calls.borrow().len(), 1);
        assert_eq!(r.observe().instances[0].state, ObservedState::Failed);
        d.instances[0].state = DesiredState::Stopped;
        r.reconcile(&d);
        assert!(r.failures.is_empty());
        d.instances[0].state = DesiredState::Running;
        r.reconcile(&d);
        assert_eq!(r.hv.calls.borrow().len(), 2);
    }
    #[test]
    fn graceful_stop_forces_shutdown_after_deadline() {
        let dir = TempDir::new();
        let mut r = reconciler(&dir, Fake { state: RefCell::new(Some(DomState::Running)), ..Fake::default() });
        let d = snapshot(DesiredState::Stopped);
        r.reconcile(&d);
        assert_eq!(r.observe().instances[0].state, ObservedState::Stopping);
        r.stop_requested.insert(d.instances[0].id, Instant::now() - GRACEFUL_STOP);
        r.reconcile(&d);
        assert_eq!(*r.hv.calls.borrow(), vec!["shutdown", "destroy"]);
        assert_eq!(r.observe().instances[0].state, ObservedState::Stopped);
    }
    #[tokio::test]
    async fn disconnected_worker_preserves_cached_delete_and_existing_disk() {
        let dir = TempDir::new();
        let d = snapshot(DesiredState::Absent);
        let r = reconciler(&dir, Fake { state: RefCell::new(Some(DomState::Running)), ..Fake::default() });
        let path = r.disks.disk_path(d.instances[0].id);
        std::fs::write(&path, "valuable disk").unwrap();
        let r = std::sync::Arc::new(std::sync::Mutex::new(r));
        assert!(crate::reconcile_snapshot(r.clone(), None).await.unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "valuable disk");
        assert_eq!(*r.lock().unwrap().hv.state.borrow(), Some(DomState::Running));
        let report = crate::reconcile_snapshot(r.clone(), Some(d)).await.unwrap().unwrap();
        assert_eq!(report.instances[0].state, ObservedState::Absent);
        assert!(!path.exists());
    }

    #[test]
    fn deletion_keeps_retrying_after_the_failure_budget_and_recovers() {
        let dir = TempDir::new();
        let mut r = reconciler(&dir, Fake { fail_delete: true, ..Fake::default() });
        let d = snapshot(DesiredState::Absent);
        let id = d.instances[0].id;
        let path = r.disks.disk_path(id);
        std::fs::write(&path, "valuable disk").unwrap();
        r.reconcile(&d);
        let failure = r.failures.get_mut(&id).unwrap();
        failure.since = Instant::now() - FAIL_BUDGET;
        failure.last_try = Instant::now() - RETRY_EVERY;
        r.reconcile(&d);
        assert!(r.failures[&id].last_try.elapsed() < RETRY_EVERY);
        assert!(path.exists());
        r.hv.fail_delete = false;
        r.failures.get_mut(&id).unwrap().last_try = Instant::now() - RETRY_EVERY;
        r.reconcile(&d);
        assert!(!path.exists());
        assert_eq!(r.observe().instances[0].state, ObservedState::Absent);
        assert!(r.failures.is_empty());
    }

}
