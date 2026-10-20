use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use copper_api::*;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Fixture {
    state: AppState,
    app: axum::Router,
    admin: String,
    user: String,
    node: NodeId,
    secret: String,
}
impl Fixture {
    fn new() -> Self {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let admin = auth::issue_token(&conn, "admin", Some(Role::Admin)).unwrap();
        let user = auth::issue_token(&conn, "alice", None).unwrap();
        for name in ["admin", "alice"] {
            db::create_project(&conn, db::user_by_name(&conn, name).unwrap().unwrap().id, "default").unwrap();
        }
        let node = NodeId::new_v4();
        let secret = "a".repeat(64);
        db::register_node(&conn, &RegisterRequest { node_id: node, hostname: "node".into(), arch: Arch::X86_64,
            capacity: Capacity { vcpus: 1, memory_mb: 1024, disk_gb: 10 } }, &auth::hash(&secret)).unwrap();
        db::set_node_status(&conn, node, db::NodeDbStatus::Approved).unwrap();
        db::touch_node(&conn, node, &Capacity { vcpus: 1, memory_mb: 1024, disk_gb: 10 }).unwrap();
        let (changed, _) = tokio::sync::watch::channel(0);
        let state = AppState { db: Arc::new(Mutex::new(conn)), changed, bridge: "br0".into() };
        let app = axum::Router::new().merge(node_api::router()).merge(user_api::router()).with_state(state.clone());
        Self { state, app, admin, user, node, secret }
    }
    async fn call(&self, method: &str, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() })
    }
    async fn create(&self, name: &str) -> (StatusCode, Value) {
        self.call("POST", "/v1/projects/default/instances", &self.user, json!({"name":name,"vcpus":1,"memory_mb":1024,"arch":"x86_64","image":"ubuntu","disk_gb":10})).await
    }
    fn report(&self, id: &str, generation: u64) -> Value {
        json!({"generation_seen":generation,"instances":[{"id":id,"state":"absent"}],"capacity":{"vcpus":1,"memory_mb":1024,"disk_gb":10}})
    }
}

#[tokio::test]
async fn node_identity_requires_pinned_credential_and_admin_approval() {
    let f = Fixture::new();
    let desired = format!("/nodes/{}/desired", f.node);
    assert_eq!(f.call("GET", &desired, "wrong", Value::Null).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(f.call("GET", &desired, &f.user, Value::Null).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(f.call("GET", &desired, &f.secret, Value::Null).await.0, StatusCode::OK);
    let registration = json!({"node_id":f.node,"hostname":"impostor","arch":"x86_64","capacity":{"vcpus":100,"memory_mb":100000,"disk_gb":1000}});
    assert_eq!(f.call("POST", "/nodes/register", &"b".repeat(64), registration).await.0, StatusCode::UNAUTHORIZED);
    let conn = f.state.db.lock().unwrap();
    assert_eq!(db::node(&conn, f.node).unwrap().unwrap().hostname, "node");
}

#[tokio::test]
async fn deleting_reserves_capacity_until_authenticated_current_confirmation() {
    let f = Fixture::new();
    let (status, vm) = f.create("vm").await;
    assert_eq!(status, StatusCode::CREATED);
    let id = vm["id"].as_str().unwrap();
    assert_eq!(f.call("DELETE", "/v1/projects/default/instances/vm", &f.user, Value::Null).await.0, StatusCode::ACCEPTED);
    assert_eq!(f.create("next").await.0, StatusCode::CONFLICT);
    let path = format!("/nodes/{}/observed", f.node);
    assert_eq!(f.call("POST", &path, "forged", f.report(id, 2)).await.0, StatusCode::UNAUTHORIZED);
    // An earlier absence must never acknowledge a newer delete request.
    assert_eq!(f.call("POST", &path, &f.secret, f.report(id, 1)).await.0, StatusCode::OK);
    assert_eq!(f.create("next").await.0, StatusCode::CONFLICT);
    assert_eq!(f.call("POST", &path, &f.secret, f.report(id, 99)).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.call("POST", &path, &f.secret, f.report(id, 2)).await.0, StatusCode::OK);
    assert_eq!(f.create("next").await.0, StatusCode::CREATED);
    // Removing a row also changes the desired snapshot's generation.
    assert_eq!(db::node(&f.state.db.lock().unwrap(), f.node).unwrap().unwrap().generation, 4);
}

#[tokio::test]
async fn concurrent_creates_cannot_both_take_the_last_slot() {
    let f = Fixture::new();
    let (a, b) = tokio::join!(f.create("first"), f.create("second"));
    let mut statuses = [a.0.as_u16(), b.0.as_u16()];
    statuses.sort();
    assert_eq!(statuses, [201, 409]);
}

#[tokio::test]
async fn retirement_blocks_agent_and_forgetting_is_an_explicit_admin_action() {
    let f = Fixture::new();
    let (_, vm) = f.create("vm").await;
    let forget = format!("/v1/instances/{}/forget", vm["id"].as_str().unwrap());
    assert_eq!(f.call("POST", &forget, &f.admin, Value::Null).await.0, StatusCode::CONFLICT);
    assert_eq!(f.call("POST", &format!("/v1/nodes/{}/retire", f.node), &f.admin, Value::Null).await.0, StatusCode::OK);
    assert_eq!(f.call("GET", &format!("/nodes/{}/desired", f.node), &f.secret, Value::Null).await.0, StatusCode::GONE);
    assert_eq!(f.call("POST", &format!("/nodes/{}/observed", f.node), &f.secret, f.report(vm["id"].as_str().unwrap(), 1)).await.0, StatusCode::GONE);
    assert_eq!(f.call("POST", &forget, &f.user, Value::Null).await.0, StatusCode::FORBIDDEN);
    assert_eq!(f.call("POST", &forget, &f.admin, Value::Null).await.0, StatusCode::NO_CONTENT);
    assert_eq!(f.call("GET", "/v1/projects/default/instances/vm", &f.user, Value::Null).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admins_can_inspect_other_users_projects_without_exposing_them_to_users() {
    let f = Fixture::new();
    f.create("vm").await;
    let (status, projects) = f.call("GET", "/v1/projects", &f.admin, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(projects.as_array().unwrap().len(), 2);
    assert_eq!(f.call("GET", "/v1/projects/alice:default/instances/vm", &f.admin, Value::Null).await.0, StatusCode::OK);
    assert_eq!(f.call("GET", "/v1/projects/admin:default/instances", &f.user, Value::Null).await.0, StatusCode::FORBIDDEN);
}

#[test]
fn issuing_a_token_preserves_role_unless_explicitly_changed() {
    let f = Fixture::new();
    let conn = f.state.db.lock().unwrap();
    let token = auth::issue_token(&conn, "admin", None).unwrap();
    assert_eq!(db::user_by_token_hash(&conn, &auth::hash(&token)).unwrap().unwrap().role, Role::Admin);
    auth::issue_token(&conn, "admin", Some(Role::User)).unwrap();
    assert_eq!(db::user_by_name(&conn, "admin").unwrap().unwrap().role, Role::User);
}

#[tokio::test]
async fn failed_retirement_rolls_back_and_connection_remains_usable() {
    let f = Fixture::new();
    f.create("vm").await;
    f.state.db.lock().unwrap().execute_batch("CREATE TRIGGER fail_observation BEFORE UPDATE OF observed_json ON instances BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    let path = format!("/v1/nodes/{}/retire", f.node);
    assert_eq!(f.call("POST", &path, &f.admin, Value::Null).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    {
        let conn = f.state.db.lock().unwrap();
        assert!(conn.is_autocommit());
        assert_eq!(db::node(&conn, f.node).unwrap().unwrap().status, db::NodeDbStatus::Approved);
        conn.execute_batch("DROP TRIGGER fail_observation").unwrap();
    }
    assert_eq!(f.call("POST", &path, &f.admin, Value::Null).await.0, StatusCode::OK);
}

#[tokio::test]
async fn failed_generation_bump_rolls_back_desired_state_change() {
    let f = Fixture::new();
    f.create("vm").await;
    f.state.db.lock().unwrap().execute_batch("CREATE TRIGGER fail_generation BEFORE UPDATE OF generation ON nodes BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    assert_eq!(f.call("POST", "/v1/projects/default/instances/vm/stop", &f.user, Value::Null).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    let conn = f.state.db.lock().unwrap();
    assert!(conn.is_autocommit());
    assert_eq!(db::list_instances_on_node(&conn, f.node).unwrap()[0].desired, DesiredState::Running);
}

#[tokio::test]
async fn retirement_wakes_an_inflight_long_poll() {
    let f = Fixture::new();
    let path = format!("/nodes/{}/desired?after=0", f.node);
    let (poll, retired) = tokio::join!(
        tokio::time::timeout(std::time::Duration::from_secs(1), f.call("GET", &path, &f.secret, Value::Null)),
        async {
            tokio::task::yield_now().await;
            f.call("POST", &format!("/v1/nodes/{}/retire", f.node), &f.admin, Value::Null).await
        }
    );
    assert_eq!(retired.0, StatusCode::OK);
    assert_eq!(poll.unwrap().0, StatusCode::GONE);
}

#[tokio::test]
async fn restarting_agent_registration_preserves_last_observed_capacity() {
    let f = Fixture::new();
    {
        let conn = f.state.db.lock().unwrap();
        db::touch_node(&conn, f.node, &Capacity { vcpus: 2, memory_mb: 2048, disk_gb: 20 }).unwrap();
    }
    assert_eq!(f.create("existing").await.0, StatusCode::CREATED);
    // A restarted agent without cached inventory only knows physical free space.
    let registration = json!({"node_id":f.node,"hostname":"restarted","arch":"x86_64","capacity":{"vcpus":2,"memory_mb":2048,"disk_gb":0}});
    let (status, reply) = f.call("POST", "/nodes/register", &f.secret, registration).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reply["status"], "approved");
    assert_eq!(f.create("next").await.0, StatusCode::CREATED);
    assert_eq!(db::node(&f.state.db.lock().unwrap(), f.node).unwrap().unwrap().hostname, "restarted");
}
