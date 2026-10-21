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
        let catalog = std::env::temp_dir().join(format!("kiln-test-{}", uuid::Uuid::new_v4()));
        let state = AppState { db: Arc::new(Mutex::new(conn)), changed, bridge: "br0".into(), catalog };
        let app = axum::Router::new().merge(node_api::router()).merge(user_api::router()).with_state(state.clone());
        Self { state, app, admin, user, node, secret }
    }
    /// Another approved, online node.
    fn add_node(&self, capacity: Capacity) -> NodeId {
        let conn = self.state.db.lock().unwrap();
        let id = NodeId::new_v4();
        db::register_node(&conn, &RegisterRequest { node_id: id, hostname: format!("n-{id}"), arch: Arch::X86_64, capacity: capacity.clone() },
            &auth::hash(&format!("{:064x}", id.as_u128()))).unwrap();
        db::set_node_status(&conn, id, db::NodeDbStatus::Approved).unwrap();
        db::touch_node(&conn, id, &capacity).unwrap();
        id
    }
    fn instance_row(&self, name: &str) -> db::InstanceRow {
        let conn = self.state.db.lock().unwrap();
        let project = db::project_by_name(&conn, db::user_by_name(&conn, "alice").unwrap().unwrap().id, "default").unwrap().unwrap();
        db::instance_by_name(&conn, project.id, name).unwrap().unwrap()
    }
    fn generation(&self) -> u64 {
        db::node(&self.state.db.lock().unwrap(), self.node).unwrap().unwrap().generation
    }
    async fn report_state(&self, id: &str, state: &str) -> StatusCode {
        let report = json!({"generation_seen":self.generation(),"instances":[{"id":id,"state":state}],"capacity":{"vcpus":1,"memory_mb":1024,"disk_gb":10}});
        self.call("POST", &format!("/nodes/{}/observed", self.node), &self.secret, report).await.0
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

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.state.catalog);
    }
}

#[tokio::test]
async fn ssh_tunnel_needs_the_owner_a_running_instance_and_a_lan_address() {
    let f = Fixture::new();
    let (_, vm) = f.create("vm").await;
    let id = vm["id"].as_str().unwrap().to_string();
    let path = "/v1/projects/default/instances/vm/ssh";
    let tunnel = |token: &str, path: &str| {
        Request::builder().uri(path).header("authorization", format!("Bearer {token}"))
            .header("connection", "upgrade").header("upgrade", user_api::TUNNEL_PROTOCOL).body(Body::empty()).unwrap()
    };
    let status = |req: Request<Body>| async { f.app.clone().oneshot(req).await.unwrap().status() };
    assert_eq!(f.call("GET", path, &f.user, Value::Null).await.0, StatusCode::UPGRADE_REQUIRED);
    assert_eq!(status(tunnel("nope", path)).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status(tunnel(&f.user, path)).await, StatusCode::CONFLICT); // still starting
    assert_eq!(status(tunnel(&f.user, "/v1/projects/admin:default/instances/vm/ssh")).await, StatusCode::FORBIDDEN);
    // A guest that reports loopback or a tailnet address must not reach the host or the tailnet.
    let report = json!({"generation_seen":f.generation(),"instances":[{"id":id,"state":"running","addresses":["127.0.0.1","100.75.219.83"]}],
        "capacity":{"vcpus":1,"memory_mb":1024,"disk_gb":10}});
    assert_eq!(f.call("POST", &format!("/nodes/{}/observed", f.node), &f.secret, report).await.0, StatusCode::OK);
    let resp = f.app.clone().oneshot(tunnel(&f.user, path)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: Value = serde_json::from_slice(&to_bytes(resp.into_body(), 1024).await.unwrap()).unwrap();
    assert!(body["message"].as_str().unwrap().contains("no LAN address"));
}

#[test]
fn tunnel_targets_are_private_lan_addresses_only() {
    for ok in ["10.255.255.1", "172.31.255.1", "192.168.254.254", "fd12:3456::1"] {
        assert!(user_api::relay_allowed(ok.parse().unwrap()), "{ok}");
    }
    for bad in ["127.0.0.1", "0.0.0.0", "169.254.1.1", "100.75.219.83", "8.8.8.8", "::1", "fd7a:115c:a1e0::1", "fe80::1"] {
        assert!(!user_api::relay_allowed(bad.parse().unwrap()), "{bad}");
    }
}

#[test]
fn only_loopback_and_the_tailnet_are_trusted_for_plain_http() {
    for ok in ["127.0.0.1", "127.0.0.2", "100.75.219.83", "100.64.0.1", "100.127.255.254", "::1", "fd7a:115c:a1e0::1"] {
        assert!(trusted_network(ok.parse().unwrap()), "{ok}");
    }
    for lan in ["0.0.0.0", "192.168.68.109", "10.0.0.1", "100.128.0.1", "100.63.255.255", "::", "fd00::1"] {
        assert!(!trusted_network(lan.parse().unwrap()), "{lan}");
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

#[tokio::test]
async fn overcommit_ratios_are_admin_policy_per_node() {
    let f = Fixture::new();
    assert_eq!(f.create("first").await.0, StatusCode::CREATED);
    assert_eq!(f.create("second").await.0, StatusCode::CONFLICT);
    let policy = format!("/v1/nodes/{}/policy", f.node);
    assert_eq!(f.call("POST", &policy, &f.user, json!({"cpu_ratio": 2.0})).await.0, StatusCode::FORBIDDEN);
    for bad in [json!({"cpu_ratio": 0.0}), json!({"memory_ratio": -1.0}), json!({"disk_ratio": 1000.0})] {
        assert_eq!(f.call("POST", &policy, &f.admin, bad).await.0, StatusCode::BAD_REQUEST);
    }
    // Only the CPU ratio changes: memory still refuses the second instance.
    let (status, node) = f.call("POST", &policy, &f.admin, json!({"cpu_ratio": 2.0})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(node["overcommit"], json!({"cpu": 2.0, "memory": 1.0, "disk": 1.0}));
    assert_eq!(f.create("second").await.0, StatusCode::CONFLICT);
    f.call("POST", &policy, &f.admin, json!({"memory_ratio": 2.0, "disk_ratio": 2.0})).await;
    assert_eq!(f.create("second").await.0, StatusCode::CREATED);
    assert_eq!(f.create("third").await.0, StatusCode::CONFLICT);
    let (_, nodes) = f.call("GET", "/v1/nodes", &f.admin, Value::Null).await;
    assert_eq!(nodes[0]["allocated"], json!({"vcpus": 2, "memory_mb": 2048, "disk_gb": 20}));
    assert_eq!(nodes[0]["capacity"], json!({"vcpus": 1, "memory_mb": 1024, "disk_gb": 10}));
}

#[tokio::test]
async fn resize_needs_a_stopped_instance_and_room_on_its_own_node() {
    let f = Fixture::new();
    let (_, vm) = f.create("vm").await;
    let id = vm["id"].as_str().unwrap().to_string();
    let resize = "/v1/projects/default/instances/vm/resize";
    assert_eq!(f.call("POST", resize, &f.user, json!({"vcpus": 1})).await.0, StatusCode::CONFLICT);
    f.call("POST", "/v1/projects/default/instances/vm/stop", &f.user, Value::Null).await;
    // Desired is stopped but the node still reports it running.
    assert_eq!(f.report_state(&id, "running").await, StatusCode::OK);
    assert_eq!(f.call("POST", resize, &f.user, json!({"memory_mb": 512})).await.0, StatusCode::CONFLICT);
    assert_eq!(f.report_state(&id, "stopped").await, StatusCode::OK);
    assert_eq!(f.call("POST", resize, &f.user, json!({"disk_gb": 5})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.call("POST", resize, &f.user, json!({"memory_mb": 2048})).await.0, StatusCode::CONFLICT);
    let before = f.generation();
    let (status, out) = f.call("POST", resize, &f.user, json!({"memory_mb": 512})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(out["memory_mb"], 512);
    assert_eq!(out["disk_gb"], 10);
    assert_eq!(f.generation(), before + 1);
    // Growing into space it already holds plus what the ratio adds is fine.
    f.call("POST", &format!("/v1/nodes/{}/policy", f.node), &f.admin, json!({"disk_ratio": 2.0})).await;
    let (status, out) = f.call("POST", resize, &f.user, json!({"disk_gb": 20})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(out["disk_gb"], 20);
    assert_eq!(f.instance_row("vm").spec.boot_disk.size_gb, 20);
    // A no-op resize does not bump the generation.
    let before = f.generation();
    assert_eq!(f.call("POST", resize, &f.user, json!({})).await.0, StatusCode::OK);
    assert_eq!(f.generation(), before);
}

#[tokio::test]
async fn placement_hints_pin_or_spread_and_are_admin_only() {
    let f = Fixture::new();
    let big = Capacity { vcpus: 8, memory_mb: 16384, disk_gb: 200 };
    let (a, b) = (f.add_node(big.clone()), f.add_node(big));
    let body = |name: &str, extra: Value| {
        let mut v = json!({"name":name,"vcpus":1,"memory_mb":1024,"arch":"x86_64","image":"ubuntu","disk_gb":10});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    };
    let create = "/v1/projects/default/instances";
    assert_eq!(f.call("POST", create, &f.user, body("x", json!({"spread": "web"}))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(f.call("POST", create, &f.user, body("x", json!({"node": a}))).await.0, StatusCode::FORBIDDEN);
    let (status, pinned) = f.call("POST", create, &f.admin, body("pinned", json!({"node": a}))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(pinned["node"], json!(a));
    assert_eq!(f.call("POST", create, &f.admin, body("nowhere", json!({"node": NodeId::new_v4()}))).await.0, StatusCode::NOT_FOUND);
    // Without the hint both would land on the node with the most free memory.
    let mut placed = vec![];
    for name in ["web-1", "web-2"] {
        let (status, vm) = f.call("POST", create, &f.admin, body(name, json!({"spread": "web"}))).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(vm["spread"], "web");
        placed.push(vm["node"].clone());
    }
    assert_ne!(placed[0], placed[1]);
    assert!(placed.contains(&json!(a)) && placed.contains(&json!(b)));
    // Ordinary users never see placement or groups.
    let (_, listed) = f.call("GET", "/v1/projects/admin:default/instances", &f.admin, Value::Null).await;
    assert_eq!(listed.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn catalogue_images_bind_by_hash_and_are_served_only_to_nodes() {
    let f = Fixture::new();
    let src = f.state.catalog.with_extension("src");
    std::fs::write(&src, b"QFI\xfb a small qcow2").unwrap();
    let image = {
        let conn = f.state.db.lock().unwrap();
        catalog::import(&conn, &f.state.catalog, "ubuntu", Arch::X86_64, &src).unwrap()
    };
    std::fs::remove_file(&src).unwrap();
    let (_, vm) = f.create("vm").await;
    assert_eq!(f.instance_row("vm").spec.boot_disk.sha256.as_deref(), Some(image.sha256.as_str()));
    assert_eq!(vm["disk_gb"], 10);
    let (_, images) = f.call("GET", "/v1/images", &f.user, Value::Null).await;
    assert_eq!(images[0]["sha256"], json!(image.sha256));
    let path = format!("/nodes/{}/images/{}", f.node, image.sha256);
    assert_eq!(f.call("GET", &path, &f.user, Value::Null).await.0, StatusCode::UNAUTHORIZED);
    let req = Request::builder().uri(&path).header("authorization", format!("Bearer {}", f.secret)).body(Body::empty()).unwrap();
    let resp = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(to_bytes(resp.into_body(), 1024).await.unwrap().as_ref(), b"QFI\xfb a small qcow2");
    let unknown = format!("/nodes/{}/images/{}", f.node, "0".repeat(64));
    assert_eq!(f.call("GET", &unknown, &f.secret, Value::Null).await.0, StatusCode::NOT_FOUND);
    assert_eq!(f.call("GET", &format!("/nodes/{}/images/..%2Fkiln.db", f.node), &f.secret, Value::Null).await.0, StatusCode::NOT_FOUND);
    // An image the catalogue doesn't know stays a node-local name.
    f.call("POST", &format!("/v1/nodes/{}/policy", f.node), &f.admin, json!({"cpu_ratio": 4.0, "memory_ratio": 4.0, "disk_ratio": 4.0})).await;
    let (status, _) = f.call("POST", "/v1/projects/default/instances", &f.user,
        json!({"name":"local","vcpus":1,"memory_mb":128,"arch":"x86_64","image":"debian-12","disk_gb":1})).await;
    assert_eq!(status, StatusCode::CREATED);
    let local = f.instance_row("local").spec.boot_disk;
    assert_eq!((local.image.as_str(), local.sha256), ("debian-12", None));
}
