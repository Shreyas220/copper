use super::*;
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone)]
struct ScriptedClient(mpsc::UnboundedSender<Call>);
enum Call {
    Desired(Option<Generation>, oneshot::Sender<anyhow::Result<DesiredNodeState>>),
    Register(oneshot::Sender<anyhow::Result<RegistrationStatus>>),
    Observed(ObservedNodeState, oneshot::Sender<anyhow::Result<()>>),
}
impl NodeClient for ScriptedClient {
    async fn desired(&self, after: Option<Generation>) -> anyhow::Result<DesiredNodeState> {
        let (tx, rx) = oneshot::channel();
        self.0.send(Call::Desired(after, tx)).map_err(|_| anyhow::anyhow!("test stopped"))?;
        rx.await?
    }
    async fn register(&self, _: &RegisterRequest) -> anyhow::Result<RegistrationStatus> {
        let (tx, rx) = oneshot::channel();
        self.0.send(Call::Register(tx)).map_err(|_| anyhow::anyhow!("test stopped"))?;
        rx.await?
    }
    async fn observed(&self, report: &ObservedNodeState) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.0.send(Call::Observed(report.clone(), tx)).map_err(|_| anyhow::anyhow!("test stopped"))?;
        rx.await?
    }
}
fn http_error(status: u16) -> anyhow::Error {
    let response = http::Response::builder().status(status).body("").unwrap();
    reqwest::Response::from(response).error_for_status().unwrap_err().into()
}
fn registration() -> RegisterRequest {
    RegisterRequest { node_id: NodeId::new_v4(), hostname: "node".into(), arch: Arch::X86_64,
        capacity: Capacity { vcpus: 4, memory_mb: 8192, disk_gb: 100 } }
}
fn snapshot() -> DesiredNodeState { DesiredNodeState { generation: 7, instances: vec![] } }
async fn desired(calls: &mut mpsc::UnboundedReceiver<Call>) -> (Option<Generation>, oneshot::Sender<anyhow::Result<DesiredNodeState>>) {
    loop {
        match calls.recv().await.unwrap() {
            Call::Desired(after, reply) if !reply.is_closed() => return (after, reply),
            Call::Desired(_, _) => continue, // Cancelled long-poll.
            _ => panic!("expected desired request"),
        }
    }
}
async fn register(calls: &mut mpsc::UnboundedReceiver<Call>) -> oneshot::Sender<anyhow::Result<RegistrationStatus>> {
    loop {
        match calls.recv().await.unwrap() {
            Call::Register(reply) => return reply,
            Call::Desired(_, reply) if reply.is_closed() => continue,
            _ => panic!("expected registration request"),
        }
    }
}
async fn observed(calls: &mut mpsc::UnboundedReceiver<Call>) -> (ObservedNodeState, oneshot::Sender<anyhow::Result<()>>) {
    match calls.recv().await.unwrap() {
        Call::Observed(report, reply) => (report, reply),
        _ => panic!("expected observed request"),
    }
}

#[test]
fn only_invalid_credentials_and_retirement_are_terminal() {
    assert!(client::terminal(&http_error(401)));
    assert!(client::terminal(&http_error(410)));
    assert!(!client::terminal(&http_error(403)));
    assert!(client::pending(&http_error(403)));
    assert!(!client::pending(&http_error(500)));
}

#[tokio::test(start_paused = true)]
async fn transient_poll_failure_resets_after_for_immediate_refresh() {
    let dir = test_support::TempDir::new();
    let (client, mut calls) = mpsc::unbounded_channel();
    let (updates, mut state) = watch::channel(ControllerState::default());
    let (_failures, errors) = mpsc::channel(1);
    let task = tokio::spawn(desired_loop(ScriptedClient(client), dir.path().into(), None, registration(), updates, errors));
    let (after, reply) = desired(&mut calls).await;
    assert_eq!(after, None);
    reply.send(Ok(snapshot())).unwrap();
    state.changed().await.unwrap();
    assert!(state.borrow_and_update().desired.is_some());
    let (after, reply) = desired(&mut calls).await;
    assert_eq!(after, Some(7));
    reply.send(Err(anyhow::anyhow!("transient disconnect"))).unwrap();
    state.changed().await.unwrap();
    assert!(state.borrow_and_update().desired.is_none());
    let (after, _reply) = desired(&mut calls).await;
    assert_eq!(after, None);
    task.abort();
    let _ = task.await;
}

async fn pending_live_node_recovers(from_report: bool) {
    let dir = test_support::TempDir::new();
    let (client, mut calls) = mpsc::unbounded_channel();
    let (updates, mut state) = watch::channel(ControllerState::default());
    let (failures, errors) = mpsc::channel(1);
    let task = tokio::spawn(desired_loop(ScriptedClient(client), dir.path().into(), None, registration(), updates, errors));
    let (_, reply) = desired(&mut calls).await;
    reply.send(Ok(snapshot())).unwrap();
    state.changed().await.unwrap();
    let revision = state.borrow_and_update().revision;
    let (_, long_poll) = desired(&mut calls).await;
    if from_report {
        failures.send(ReportFailure { revision, error: http_error(403) }).await.unwrap();
        state.changed().await.unwrap();
        assert!(long_poll.is_closed());
    } else {
        long_poll.send(Err(http_error(403))).unwrap();
        state.changed().await.unwrap();
    }
    assert!(state.borrow_and_update().desired.is_none());
    register(&mut calls).await.send(Ok(RegistrationStatus::Pending)).unwrap();
    register(&mut calls).await.send(Ok(RegistrationStatus::Approved)).unwrap();
    let (after, reply) = desired(&mut calls).await;
    assert_eq!(after, None);
    reply.send(Ok(snapshot())).unwrap();
    state.changed().await.unwrap();
    assert!(state.borrow_and_update().desired.is_some());
    assert!(!task.is_finished());
    task.abort();
    let _ = task.await;
}

#[tokio::test(start_paused = true)]
async fn pending_desired_response_reregisters_and_resumes_after_approval() {
    pending_live_node_recovers(false).await;
}

#[tokio::test(start_paused = true)]
async fn pending_observed_response_reregisters_and_resumes_after_approval() {
    pending_live_node_recovers(true).await;
}

#[tokio::test(start_paused = true)]
async fn report_failure_cancels_long_poll_and_refreshes_without_after() {
    let dir = test_support::TempDir::new();
    let (client, mut calls) = mpsc::unbounded_channel();
    let (updates, mut state) = watch::channel(ControllerState::default());
    let (failures, errors) = mpsc::channel(1);
    let task = tokio::spawn(desired_loop(ScriptedClient(client), dir.path().into(), None, registration(), updates, errors));
    desired(&mut calls).await.1.send(Ok(snapshot())).unwrap();
    state.changed().await.unwrap();
    let revision = state.borrow_and_update().revision;
    let (_, long_poll) = desired(&mut calls).await;
    failures.send(ReportFailure { revision, error: anyhow::anyhow!("report failed") }).await.unwrap();
    state.changed().await.unwrap();
    assert!(state.borrow_and_update().desired.is_none());
    assert!(long_poll.is_closed());
    assert_eq!(desired(&mut calls).await.0, None);
    task.abort();
    let _ = task.await;
}

#[tokio::test(start_paused = true)]
async fn stale_report_failure_cannot_revoke_a_newer_response_for_the_same_generation() {
    let dir = test_support::TempDir::new();
    let (client, mut calls) = mpsc::unbounded_channel();
    let (updates, mut state) = watch::channel(ControllerState::default());
    let (failures, errors) = mpsc::channel(1);
    let task = tokio::spawn(desired_loop(ScriptedClient(client), dir.path().into(), None, registration(), updates, errors));
    desired(&mut calls).await.1.send(Ok(snapshot())).unwrap();
    state.changed().await.unwrap();
    let old_revision = state.borrow_and_update().revision;
    desired(&mut calls).await.1.send(Ok(snapshot())).unwrap();
    state.changed().await.unwrap();
    let new_revision = state.borrow_and_update().revision;
    assert!(new_revision > old_revision);
    failures.send(ReportFailure { revision: old_revision, error: anyhow::anyhow!("old report failed") }).await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(desired(&mut calls).await.0, Some(7));
    let latest = state.borrow();
    assert_eq!(latest.revision, new_revision);
    assert!(latest.desired.is_some());
    task.abort();
    drop(latest);
    let _ = task.await;
}

struct EmptyHypervisor;
impl hv::Hypervisor for EmptyHypervisor {
    fn node_capacity(&self) -> anyhow::Result<(u32, u64)> { Ok((4, 8192)) }
    fn allocatable_gb(&self, _: &[InstanceId]) -> u64 { 0 }
    fn prepare(&self, _: &DesiredInstance) -> anyhow::Result<()> { unreachable!() }
    fn has_storage(&self, _: InstanceId) -> bool { false }
    fn remove_storage(&self, _: InstanceId) -> anyhow::Result<()> { unreachable!() }
    fn state(&self, _: InstanceId) -> anyhow::Result<Option<hv::DomState>> { unreachable!() }
    fn define(&self, _: &DesiredInstance) -> anyhow::Result<()> { unreachable!() }
    fn start(&self, _: InstanceId) -> anyhow::Result<()> { unreachable!() }
    fn shutdown(&self, _: InstanceId) -> anyhow::Result<()> { unreachable!() }
    fn destroy(&self, _: InstanceId) -> anyhow::Result<()> { unreachable!() }
    fn undefine(&self, _: InstanceId) -> anyhow::Result<()> { unreachable!() }
    fn addresses(&self, _: InstanceId) -> Vec<String> { unreachable!() }
}

#[tokio::test]
async fn worker_reports_old_failures_without_overwriting_new_desired_state() {
    let dir = test_support::TempDir::new();
    let _ = &dir;
    let reconciler = Arc::new(Mutex::new(Reconciler::new(EmptyHypervisor, Capacity { vcpus: 0, memory_mb: 0, disk_gb: 0 })));
    let (client, mut calls) = mpsc::unbounded_channel();
    let (updates, state) = watch::channel(ControllerState { revision: 1, desired: Some(snapshot()) });
    let (failures, mut errors) = mpsc::channel(1);
    let task = tokio::spawn(reconcile_loop(ScriptedClient(client), state, failures, reconciler));
    let (_, reply) = observed(&mut calls).await;
    updates.send_replace(ControllerState { revision: 2, desired: Some(snapshot()) });
    reply.send(Err(anyhow::anyhow!("old report failed"))).unwrap();
    assert_eq!(errors.recv().await.unwrap().revision, 1);
    let (report, _reply) = observed(&mut calls).await;
    assert_eq!(report.generation_seen, 7);
    assert_eq!(updates.borrow().revision, 2);
    assert!(updates.borrow().desired.is_some());
    task.abort();
    let _ = task.await;
}
