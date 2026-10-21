//! Drive the hypervisor toward a desired state. Idempotent: calling it twice
//! with the same input does nothing the second time.
//!
//! Failures are retried with backoff for `FAIL_BUDGET`; after that the
//! instance is reported `Failed` with the last error until the desired state
//! changes, which resets the budget. Deletion retries indefinitely because
//! desired-absent is irreversible and cleanup must eventually complete.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use copper_api::*;

use crate::hv::{DomState, Hypervisor};
use crate::images::Images;

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

pub struct Reconciler<H = crate::hv::Native> {
    hv: H,
    reserve: Capacity,
    failures: HashMap<InstanceId, Failure>,
    stop_requested: HashMap<InstanceId, Instant>,
    /// Snapshot whose generation is paired with observations under the same lock.
    desired: DesiredNodeState,
    /// Images fetched from kiln. `None` means hashed images must already be present.
    images: Option<Arc<Images>>,
    /// Instances waiting for their base image, with the note to report.
    waiting: HashMap<InstanceId, String>,
    /// Size last applied to each VM by this process, so `reconfigure` runs
    /// once per change instead of on every tick.
    configured: HashMap<InstanceId, (u32, u64, u64)>,
}

impl<H: Hypervisor> Reconciler<H> {
    pub fn new(hv: H, reserve: Capacity) -> Self {
        Self {
            hv,
            reserve,
            failures: HashMap::new(),
            stop_requested: HashMap::new(),
            desired: DesiredNodeState { generation: 0, instances: vec![] },
            images: None,
            waiting: HashMap::new(),
            configured: HashMap::new(),
        }
    }

    pub fn with_images(mut self, images: Arc<Images>) -> Self {
        self.images = Some(images);
        self
    }

    /// A running instance whose catalogue image isn't here yet waits for the
    /// download instead of failing; waiting never consumes the failure budget.
    fn waiting_for_image(&self, inst: &DesiredInstance) -> Option<String> {
        let images = self.images.as_ref()?;
        let hash = inst.spec.boot_disk.sha256.as_deref()?;
        if inst.state != DesiredState::Running || !valid_sha256(hash) || images.ready(hash) {
            return None;
        }
        Some(images.want(&inst.spec.boot_disk.image, hash))
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
            disk_gb: self.hv.allocatable_gb(&inventory.instances.iter().map(|i| i.id).collect::<Vec<_>>()).saturating_sub(self.reserve.disk_gb),
        }
    }

    pub fn reconcile(&mut self, desired: &DesiredNodeState) {
        self.desired = desired.clone();
        for inst in &desired.instances {
            // A change of desired state wipes the failure history for that instance.
            if self.failures.get(&inst.id).is_some_and(|f| f.desired != inst.state) {
                self.failures.remove(&inst.id);
            }
            match self.waiting_for_image(inst) {
                Some(note) => {
                    self.waiting.insert(inst.id, note);
                    continue;
                }
                None => {
                    self.waiting.remove(&inst.id);
                }
            }
            if let Some(f) = self.failures.get(&inst.id) {
                if (inst.state != DesiredState::Absent && f.exhausted()) || f.last_try.elapsed() < RETRY_EVERY {
                    continue;
                }
            }
            let result = match inst.state {
                DesiredState::Running => self.ensure_running(inst),
                DesiredState::Stopped => self.ensure_stopped(inst),
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
        self.waiting.retain(|id, _| known.contains(id));
        self.configured.retain(|id, _| known.contains(id));
    }

    fn ensure_running(&mut self, inst: &DesiredInstance) -> anyhow::Result<()> {
        self.stop_requested.remove(&inst.id);
        self.hv.prepare(inst)?;
        self.hv.define(inst)?;
        if self.hv.state(inst.id)? == Some(DomState::Off) {
            self.reconfigure(inst)?;
        }
        self.hv.start(inst.id)
    }

    /// Apply a resize to a VM that is off. Idempotent in the backend; cached
    /// here so an unchanged spec costs nothing per tick.
    fn reconfigure(&mut self, inst: &DesiredInstance) -> anyhow::Result<()> {
        let size = (inst.spec.vcpus, inst.spec.memory_mb, inst.spec.boot_disk.size_gb);
        if self.configured.get(&inst.id) == Some(&size) {
            return Ok(());
        }
        self.hv.reconfigure(inst)?;
        self.configured.insert(inst.id, size);
        Ok(())
    }

    fn ensure_stopped(&mut self, inst: &DesiredInstance) -> anyhow::Result<()> {
        let id = inst.id;
        match self.hv.state(id)? {
            None => {
                self.stop_requested.remove(&id);
                Ok(())
            }
            Some(DomState::Off) => {
                self.stop_requested.remove(&id);
                self.reconfigure(inst)
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
        self.hv.remove_storage(id)?;
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
                (Ok(None), _) if self.waiting.contains_key(&id) => (ObservedState::Starting, self.waiting.get(&id).cloned()),
                (Ok(None), _) if self.hv.has_storage(id) => (ObservedState::Stopped, None),
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

    /// A hypervisor that keeps real files for storage (so cleanup semantics
    /// are exercised) but fakes the VM lifecycle.
    #[derive(Default)]
    struct Fake {
        disks: crate::disk::Disks,
        state: RefCell<Option<DomState>>,
        fail_lookup: bool,
        fail_delete: bool,
        fail_start: bool,
        calls: RefCell<Vec<&'static str>>,
    }
    impl Hypervisor for Fake {
        fn node_capacity(&self) -> anyhow::Result<(u32, u64)> { Ok((8, 16384)) }
        fn allocatable_gb(&self, ids: &[InstanceId]) -> u64 { self.disks.allocatable_gb(ids.iter().copied()) }
        fn prepare(&self, _: &DesiredInstance) -> anyhow::Result<()> { Ok(()) }
        fn has_storage(&self, id: InstanceId) -> bool { self.disks.disk_exists(id) }
        fn remove_storage(&self, id: InstanceId) -> anyhow::Result<()> { self.disks.remove(id) }
        fn state(&self, _: InstanceId) -> anyhow::Result<Option<DomState>> {
            if self.fail_lookup { anyhow::bail!("libvirt disconnected") }
            Ok(*self.state.borrow())
        }
        fn define(&self, _: &DesiredInstance) -> anyhow::Result<()> { Ok(()) }
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
        fn reconfigure(&self, _: &DesiredInstance) -> anyhow::Result<()> {
            assert_ne!(*self.state.borrow(), Some(DomState::Running), "reconfigured a running VM");
            self.calls.borrow_mut().push("reconfigure");
            Ok(())
        }
        fn addresses(&self, _: InstanceId) -> Vec<String> { vec![] }
    }
    fn snapshot(state: DesiredState) -> DesiredNodeState {
        DesiredNodeState { generation: 7, instances: vec![DesiredInstance {
            id: InstanceId::new_v4(), name: "vm".into(), state,
            spec: InstanceSpec {
                vcpus: 1, memory_mb: 1024, arch: Arch::X86_64,
                boot_disk: BootDisk { image: "ubuntu".into(), sha256: None, size_gb: 10 },
                nics: vec![], ssh_keys: vec![],
            },
        }] }
    }
    #[test]
    fn running_instance_waits_for_its_image_without_spending_the_failure_budget() {
        let dir = TempDir::new();
        let (images, mut queue) = Images::new(dir.path().join("sha256"));
        let images = Arc::new(images);
        let mut r = reconciler(&dir, Fake::default()).with_images(images.clone());
        let mut d = snapshot(DesiredState::Running);
        let hash = "cd".repeat(32);
        d.instances[0].spec.boot_disk.sha256 = Some(hash.clone());
        for _ in 0..3 { r.reconcile(&d); }
        let report = r.observe();
        assert_eq!(report.instances[0].state, ObservedState::Starting);
        assert_eq!(report.instances[0].error.as_deref(), Some("fetching image ubuntu"));
        assert!(r.failures.is_empty());
        assert!(r.hv.calls.borrow().is_empty());
        // Only one download is queued while it is in flight.
        assert_eq!(queue.try_recv().unwrap(), hash);
        assert!(queue.try_recv().is_err());
        std::fs::create_dir_all(dir.path().join("sha256")).unwrap();
        std::fs::write(images.path(&hash), "verified blob").unwrap();
        r.reconcile(&d);
        assert_eq!(*r.hv.calls.borrow(), vec!["start"]);
        assert_eq!(r.observe().instances[0].state, ObservedState::Running);
    }
    #[test]
    fn resize_is_applied_once_per_change_and_only_while_off() {
        let dir = TempDir::new();
        let mut r = reconciler(&dir, Fake { state: RefCell::new(Some(DomState::Off)), ..Fake::default() });
        let reconfigures = |r: &Reconciler<Fake>| r.hv.calls.borrow().iter().filter(|c| **c == "reconfigure").count();
        let mut d = snapshot(DesiredState::Stopped);
        r.reconcile(&d);
        r.reconcile(&d);
        assert_eq!(reconfigures(&r), 1);
        d.instances[0].spec.memory_mb = 2048;
        d.generation += 1;
        r.reconcile(&d);
        assert_eq!(reconfigures(&r), 2);
        // Starting applies a pending change first; a running VM is left alone.
        d.instances[0].spec.vcpus = 2;
        d.instances[0].state = DesiredState::Running;
        r.reconcile(&d);
        r.reconcile(&d);
        assert_eq!(reconfigures(&r), 3);
        assert_eq!(r.hv.calls.borrow()[2..4], ["reconfigure", "start"]);
        d.instances[0].spec.vcpus = 4;
        r.reconcile(&d);
        assert_eq!(reconfigures(&r), 3);
    }
    fn reconciler(dir: &TempDir, mut hv: Fake) -> Reconciler<Fake> {
        hv.disks = crate::disk::Disks { dir: dir.path().to_owned(), images: dir.path().to_owned() };
        Reconciler::new(hv, Capacity { vcpus: 0, memory_mb: 0, disk_gb: 0 })
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
        let path = r.hv.disks.disk_path(d.instances[0].id);
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
        std::fs::create_dir(r.hv.disks.seed_path(d.instances[0].id)).unwrap();
        r.reconcile(&d);
        assert_ne!(r.observe().instances[0].state, ObservedState::Absent);
    }
    #[test]
    fn exhausted_start_can_be_reset_by_stop_then_start() {
        let dir = TempDir::new();
        let mut r = reconciler(&dir, Fake { fail_start: true, ..Fake::default() });
        let mut d = snapshot(DesiredState::Running);
        let id = d.instances[0].id;
        std::fs::write(r.hv.disks.disk_path(id), "disk").unwrap();
        std::fs::write(r.hv.disks.seed_path(id), "seed").unwrap();
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
        let path = r.hv.disks.disk_path(d.instances[0].id);
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
        let path = r.hv.disks.disk_path(id);
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
