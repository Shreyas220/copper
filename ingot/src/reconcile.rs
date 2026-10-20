//! Drive the hypervisor toward a desired state. Idempotent: calling it twice
//! with the same input does nothing the second time.
//!
//! Failures are retried with backoff for `FAIL_BUDGET`; after that the
//! instance is reported `Failed` with the last error until the desired state
//! changes, which resets the budget.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use copper_api::*;

use crate::disk::Disks;
use crate::hv::{DomState, Libvirt};

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

pub struct Reconciler {
    hv: Libvirt,
    disks: Disks,
    reserve: Capacity,
    failures: HashMap<InstanceId, Failure>,
    stop_requested: HashMap<InstanceId, Instant>,
    /// Names from the last desired state, for the guest hostname.
    names: HashMap<InstanceId, String>,
}

impl Reconciler {
    pub fn new(hv: Libvirt, disks_dir: PathBuf, images_dir: PathBuf, reserve: Capacity) -> Self {
        Self {
            hv,
            disks: Disks { dir: disks_dir, images: images_dir },
            reserve,
            failures: HashMap::new(),
            stop_requested: HashMap::new(),
            names: HashMap::new(),
        }
    }

    pub fn capacity(&self) -> Capacity {
        let (cpus, mem_mb) = self.hv.node_capacity().unwrap_or((0, 0));
        Capacity {
            vcpus: cpus.saturating_sub(self.reserve.vcpus),
            memory_mb: mem_mb.saturating_sub(self.reserve.memory_mb),
            disk_gb: self.disks.free_gb().saturating_sub(self.reserve.disk_gb),
        }
    }

    pub fn reconcile(&mut self, desired: &DesiredNodeState) {
        for inst in &desired.instances {
            // A change of desired state wipes the failure history for that instance.
            if self.failures.get(&inst.id).is_some_and(|f| f.desired != inst.state) {
                self.failures.remove(&inst.id);
            }
            if let Some(f) = self.failures.get(&inst.id) {
                if f.exhausted() || f.last_try.elapsed() < RETRY_EVERY {
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
                    if f.exhausted() {
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
        self.names.retain(|id, _| known.contains(id));
    }

    fn ensure_running(&mut self, inst: &DesiredInstance) -> anyhow::Result<()> {
        self.stop_requested.remove(&inst.id);
        self.names.insert(inst.id, inst.name.clone());
        let disk = self.disks.ensure_disk(inst.id, &inst.spec)?;
        let seed = self.disks.ensure_seed(inst.id, &inst.name, &inst.spec)?;
        self.hv.define(inst.id, &inst.spec, &disk, &seed)?;
        self.hv.start(inst.id)
    }

    fn ensure_stopped(&mut self, id: InstanceId) -> anyhow::Result<()> {
        match self.hv.state(id) {
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

    /// What this node can see right now, for every instance it knows about.
    pub fn observe(&self) -> Vec<ObservedInstance> {
        let mut ids: Vec<InstanceId> = self.hv.list().unwrap_or_default();
        for id in self.names.keys().chain(self.failures.keys()) {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        ids.sort();
        ids.into_iter()
            .map(|id| {
                let failure = self.failures.get(&id).filter(|f| f.exhausted());
                let (state, error) = match (self.hv.state(id), failure) {
                    (_, Some(f)) => (ObservedState::Failed, Some(f.error.clone())),
                    (Some(DomState::Running), _) if self.stop_requested.contains_key(&id) => (ObservedState::Stopping, None),
                    (Some(DomState::Running), _) => (ObservedState::Running, None),
                    (Some(DomState::Other), _) => (ObservedState::Stopping, None),
                    (Some(DomState::Off), _) => (ObservedState::Stopped, None),
                    (None, _) if self.disks.disk_exists(id) => (ObservedState::Stopped, None),
                    (None, _) => (ObservedState::Absent, None),
                };
                let addresses = if state == ObservedState::Running { self.hv.addresses(id) } else { vec![] };
                ObservedInstance { id, state, error, addresses }
            })
            .collect()
    }
}
