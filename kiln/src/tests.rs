use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use copper_api::*;
use serde_json::{Value, json};
use tower::ServiceExt;

const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFj/NbTgjOM0L/Sk0uGgwW9NGFVuPaS91tElolgH/nmC alice@laptop";

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
        let admin = auth::issue_token(&conn, "admin", Some(Role::Admin), "cli").unwrap();
        let user = auth::issue_token(&conn, "alice", None, "cli").unwrap();
        for name in ["admin", "alice"] {
            db::create_project(&conn, db::user_by_name(&conn, name).unwrap().unwrap().id, "default").unwrap();
        }
        let node = NodeId::new_v4();
        let secret = "a".repeat(64);
        db::register_node(&conn, &RegisterRequest { node_id: node, hostname: "node".into(), arch: Arch::X86_64,
            capacity: Capacity { vcpus: 1, memory_mb: 1024, disk_gb: 10 } }, &auth::hash(&secret)).unwrap();
        db::set_node_status(&conn, node, db::NodeDbStatus::Approved).unwrap();
        db::touch_node(&conn, node, &Capacity { vcpus: 1, memory_mb: 1024, disk_gb: 10 }).unwrap();
        let catalog = std::env::temp_dir().join(format!("kiln-test-{}", uuid::Uuid::new_v4()));
        let state = AppState::new(conn, crate::network::VmNet::default(), catalog);
        let app = state.router();
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
    /// A browser request: session cookie instead of a bearer token, and the
    /// console's CSRF header unless `csrf` is false.
    async fn browser(&self, method: &str, path: &str, cookie: &str, csrf: bool, body: Value) -> (StatusCode, Value, Option<String>) {
        let mut req = Request::builder().method(method).uri(path).header("content-type", "application/json");
        if !cookie.is_empty() { req = req.header("cookie", cookie) }
        if csrf { req = req.header(CSRF_HEADER, "1") }
        let resp = self.app.clone().oneshot(req.body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()).await.unwrap();
        let status = resp.status();
        let set_cookie = resp.headers().get("set-cookie").map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string());
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() }, set_cookie)
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
        self.call("POST", "/v1/projects/default/instances", &self.user, json!({"name":name,"vcpus":1,"memory_mb":1024,"arch":"x86_64","image":"ubuntu","disk_gb":10,"ssh_keys":[KEY]})).await
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
async fn tunnels_go_through_the_vms_node_and_only_for_its_owner() {
    let f = Fixture::new();
    let (_, vm) = f.create("vm").await;
    let id = vm["id"].as_str().unwrap().to_string();
    let path = "/v1/projects/default/instances/vm/ssh";
    let tunnel = |token: &str, path: &str| {
        Request::builder().uri(path).header("authorization", format!("Bearer {token}"))
            .header("connection", "upgrade").header("upgrade", sessions::CLIENT_PROTOCOL).body(Body::empty()).unwrap()
    };
    let status = |req: Request<Body>| async { f.app.clone().oneshot(req).await.unwrap().status() };
    assert_eq!(f.call("GET", path, &f.user, Value::Null).await.0, StatusCode::UPGRADE_REQUIRED);
    assert_eq!(status(tunnel("nope", path)).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status(tunnel(&f.user, path)).await, StatusCode::CONFLICT); // still starting
    assert_eq!(status(tunnel(&f.user, "/v1/projects/admin:default/instances/vm/ssh")).await, StatusCode::FORBIDDEN);
    assert_eq!(f.report_state(&id, "running").await, StatusCode::OK);
    // Kiln asks the node, which can't reach the VM yet: the caller gets a 503 to retry.
    let waiting = tokio::spawn(f.app.clone().oneshot(tunnel(&f.user, "/v1/projects/default/instances/vm/ports/8080")));
    let (_, offers) = f.call("GET", &format!("/nodes/{}/consoles", f.node), &f.secret, Value::Null).await;
    assert_eq!((offers[0]["instance"].as_str(), offers[0]["port"].as_u64()), (Some(id.as_str()), Some(8080)));
    let refuse = format!("/nodes/{}/consoles/{}/refuse", f.node, offers[0]["session"].as_str().unwrap());
    assert_eq!(f.call("POST", &refuse, "nope", json!({"reason": "x"})).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(f.call("POST", &refuse, &f.secret, json!({"reason": "nothing answers on port 8080 yet"})).await.0, StatusCode::NO_CONTENT);
    let resp = waiting.await.unwrap().unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(&to_bytes(resp.into_body(), 1024).await.unwrap()).unwrap();
    assert_eq!(body["message"], "nothing answers on port 8080 yet");
    assert_eq!(f.call("POST", &refuse, &f.secret, json!({"reason": "again"})).await.0, StatusCode::NOT_FOUND, "answered once");
}

#[tokio::test]
async fn nodes_get_a_subnet_and_vms_a_fixed_address_in_it() {
    let f = Fixture::new();
    f.call("POST", &format!("/v1/nodes/{}/policy", f.node), &f.admin, json!({"cpu_ratio": 4.0, "memory_ratio": 4.0, "disk_ratio": 4.0})).await;
    // A libvirt node says it runs copper networks by reporting on its host side.
    let report = |ready: bool, route: bool| json!({"generation_seen": f.generation(), "instances": [], "capacity": {"vcpus":1,"memory_mb":1024,"disk_gb":10},
        "network_ready": ready, "tailnet_route": route});
    assert_eq!(f.call("POST", &format!("/nodes/{}/observed", f.node), &f.secret, report(false, false)).await.0, StatusCode::OK);
    // The fixture's node was approved directly; its first VM gives it a subnet.
    let (_, a) = f.create("a").await;
    let (_, b) = f.create("b").await;
    assert_eq!((a["addresses"].clone(), b["addresses"].clone()), (json!(["10.42.1.10"]), json!(["10.42.1.11"])), "known before boot");
    let nic = &f.instance_row("a").spec.nics[0];
    assert_eq!((nic.address.as_deref(), nic.bridge.as_str()), (Some("10.42.1.10/24"), ""));
    let (_, desired) = f.call("GET", &format!("/nodes/{}/desired", f.node), &f.secret, Value::Null).await;
    assert_eq!(desired["network"], json!({"subnet": "10.42.1.0/24", "gateway": "10.42.1.1", "space": "10.42.0.0/16"}));
    // A node approved later gets the next one; numbers are never reused.
    let id = NodeId::new_v4();
    let token = "c".repeat(64);
    let register = json!({"node_id": id, "hostname": "thinkcentre", "arch": "x86_64", "capacity": {"vcpus": 4, "memory_mb": 8192, "disk_gb": 100}});
    assert_eq!(f.call("POST", "/nodes/register", &token, register).await.0, StatusCode::OK);
    let (_, node) = f.call("POST", &format!("/v1/nodes/{id}/approve"), &f.admin, Value::Null).await;
    assert_eq!(node["subnet"], "10.42.2.0/24");
    // What the node reports about its network shows on the node, and as
    // `tailnet` on its VMs once the subnet route is live.
    assert_eq!(f.call("GET", "/v1/projects/default/instances/a", &f.user, Value::Null).await.1["tailnet"], false);
    assert_eq!(f.call("POST", &format!("/nodes/{}/observed", f.node), &f.secret, report(true, true)).await.0, StatusCode::OK);
    let (_, nodes) = f.call("GET", "/v1/nodes", &f.admin, Value::Null).await;
    let home = nodes.as_array().unwrap().iter().find(|n| n["id"] == json!(f.node)).unwrap();
    assert_eq!((home["network_ready"].clone(), home["tailnet_route"].clone()), (json!(true), json!(true)));
    assert_eq!(f.call("GET", "/v1/projects/default/instances/a", &f.user, Value::Null).await.1["tailnet"], true);
}

#[tokio::test]
async fn each_node_learns_the_vms_of_its_projects_on_every_node() {
    let f = Fixture::new();
    let big = Capacity { vcpus: 8, memory_mb: 16384, disk_gb: 200 };
    let (a, b) = (f.add_node(big.clone()), f.add_node(big));
    {
        let conn = f.state.db.lock().unwrap();
        db::create_project(&conn, db::user_by_name(&conn, "admin").unwrap().unwrap().id, "lab").unwrap();
    }
    let create = |project: &'static str, name: &'static str, node: NodeId| {
        let body = json!({"name":name,"vcpus":1,"memory_mb":1024,"arch":"x86_64","image":"ubuntu","disk_gb":10,"ssh_keys":[KEY],"node":node});
        let f = &f;
        async move { assert_eq!(f.call("POST", &format!("/v1/projects/{project}/instances"), &f.admin, body).await.0, StatusCode::CREATED) }
    };
    let map = |node: NodeId| {
        let f = &f;
        async move {
            let (_, d) = f.call("GET", &format!("/nodes/{node}/desired"), &format!("{:064x}", node.as_u128()), Value::Null).await;
            let mut projects: Vec<(String, Vec<(String, String)>)> = d["map"]["projects"].as_array().cloned().unwrap_or_default().iter()
                .map(|p| (p["name"].as_str().unwrap().to_string(),
                          p["vms"].as_array().unwrap().iter().map(|v| (v["name"].as_str().unwrap().to_string(), v["address"].as_str().unwrap().to_string())).collect()))
                .collect();
            projects.sort();
            (projects, d["generation"].as_u64().unwrap())
        }
    };
    create("lab", "k3s-1", a).await;
    create("default", "backup", a).await;
    create("lab", "k3s-2", b).await;
    let (on_a, _) = map(a).await;
    let lab = ("lab".to_string(), vec![("k3s-1".to_string(), "10.42.1.10".to_string()), ("k3s-2".to_string(), "10.42.2.10".to_string())]);
    assert_eq!(on_a, vec![("default".to_string(), vec![("backup".to_string(), "10.42.1.11".to_string())]), lab.clone()]);
    let (on_b, before) = map(b).await;
    assert_eq!(on_b, vec![lab], "b sees only the project it hosts, but all of that project");
    // Another lab VM on a changes b's map, so b's long-poll wakes; a new VM in
    // a project b doesn't host leaves b alone.
    create("lab", "k3s-3", a).await;
    let (on_b, after) = map(b).await;
    assert!(after > before);
    assert!(on_b[0].1.contains(&("k3s-3".to_string(), "10.42.1.12".to_string())));
    create("default", "backup-2", a).await;
    assert_eq!(map(b).await.1, after);
}

#[tokio::test]
async fn nodes_learn_each_others_keys_and_where_to_reach_them() {
    let f = Fixture::new();
    let big = Capacity { vcpus: 8, memory_mb: 16384, disk_gb: 200 };
    let (a, b) = (f.add_node(big.clone()), f.add_node(big));
    let secret = |n: NodeId| format!("{:064x}", n.as_u128());
    {
        let conn = f.state.db.lock().unwrap();
        for n in [a, b] { db::ensure_subnet(&conn, n).unwrap(); }
    }
    let generation = |n: NodeId| db::node(&f.state.db.lock().unwrap(), n).unwrap().unwrap().generation;
    let report = |n: NodeId, key: &str, lan: &str| {
        let body = json!({"generation_seen": generation(n), "instances": [], "capacity": {"vcpus":8,"memory_mb":16384,"disk_gb":200},
            "mesh": {"public_key": key, "endpoints": [{"address": lan, "via": "lan"}, {"address": "100.69.49.52:51820", "via": "tailnet"}],
                     "peers": []}});
        let f = &f;
        async move { f.call("POST", &format!("/nodes/{n}/observed"), &secret(n), body).await.0 }
    };
    let peers = |n: NodeId| {
        let f = &f;
        async move { f.call("GET", &format!("/nodes/{n}/desired"), &secret(n), Value::Null).await.1["map"]["peers"].clone() }
    };
    let key_a = format!("{}=", "A".repeat(43));
    let key_b = format!("{}=", "B".repeat(43));
    assert_eq!(report(a, &key_a, "192.168.68.109:51820").await, StatusCode::OK);
    assert_eq!(peers(b).await[0]["public_key"], json!(key_a));
    assert_eq!(peers(b).await[0]["subnet"], json!("10.42.1.0/24"));
    assert_eq!(peers(b).await[0]["endpoints"][0], json!({"address": "192.168.68.109:51820", "via": "lan"}));
    assert_eq!(peers(a).await, json!(null), "b hasn't reported a key yet, and a isn't its own peer");
    assert_eq!(report(b, &key_b, "192.168.68.113:51820").await, StatusCode::OK);
    assert_eq!(peers(a).await[0]["node"], json!(b));
    // Moving changes where it can be reached, which wakes its peers; the same
    // report again doesn't.
    let before = generation(b);
    assert_eq!(report(a, &key_a, "192.168.1.20:51820").await, StatusCode::OK);
    assert!(generation(b) > before);
    let settled = generation(b);
    assert_eq!(report(a, &key_a, "192.168.1.20:51820").await, StatusCode::OK);
    assert_eq!(generation(b), settled);
    // Anything kiln would pass on that isn't a key and addresses is refused.
    assert_eq!(report(a, "not a key", "192.168.1.20:51820").await, StatusCode::BAD_REQUEST);
    assert_eq!(report(a, &key_a, "192.168.1.20").await, StatusCode::BAD_REQUEST);
    // A retired node leaves everyone's mesh.
    assert_eq!(f.call("POST", &format!("/v1/nodes/{a}/retire"), &f.admin, Value::Null).await.0, StatusCode::OK);
    assert_eq!(peers(b).await, json!(null));
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
    let token = auth::issue_token(&conn, "admin", None, "cli").unwrap();
    assert_eq!(db::token_user(&conn, &auth::hash(&token)).unwrap().unwrap().0.role, Role::Admin);
    auth::issue_token(&conn, "admin", Some(Role::User), "cli").unwrap();
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
        let mut v = json!({"name":name,"vcpus":1,"memory_mb":1024,"arch":"x86_64","image":"ubuntu","disk_gb":10,"ssh_keys":[KEY]});
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
        json!({"name":"local","vcpus":1,"memory_mb":128,"arch":"x86_64","image":"debian-12","disk_gb":1,"ssh_keys":[KEY]})).await;
    assert_eq!(status, StatusCode::CREATED);
    let local = f.instance_row("local").spec.boot_disk;
    assert_eq!((local.image.as_str(), local.sha256), ("debian-12", None));
}

// ---- the console's backend ---------------------------------------------------

#[tokio::test]
async fn invites_set_passwords_and_sessions_need_the_csrf_header_to_write() {
    let f = Fixture::new();
    let (status, invite) = f.call("POST", "/v1/invites", &f.admin, json!({"role": "user", "project": "games"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let code = invite["code"].as_str().unwrap();
    assert_eq!(f.call("POST", "/v1/invites", &f.user, json!({})).await.0, StatusCode::FORBIDDEN);
    // Anyone holding the link can see what it is for, and nothing more.
    let (status, shown, _) = f.browser("GET", &format!("/v1/invites/{code}"), "", false, Value::Null).await;
    assert_eq!((status, shown["project"].as_str()), (StatusCode::OK, Some("games")));
    let accept = format!("/v1/invites/{code}/accept");
    assert_eq!(f.browser("POST", &accept, "", false, json!({"name": "sam", "password": "short"})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.browser("POST", &accept, "", false, json!({"name": "alice", "password": "long enough!"})).await.0, StatusCode::CONFLICT);
    let (status, me, cookie) = f.browser("POST", &accept, "", false, json!({"name": "sam", "password": "long enough!"})).await;
    assert_eq!((status, me["name"].as_str()), (StatusCode::OK, Some("sam")));
    let cookie = cookie.unwrap();
    assert_eq!(f.browser("POST", &accept, "", false, json!({"name": "sam2", "password": "long enough!"})).await.0, StatusCode::NOT_FOUND, "links work once");
    // The cookie reads freely but writes only with the header.
    assert_eq!(f.browser("GET", "/v1/projects/games/instances", &cookie, false, Value::Null).await.0, StatusCode::OK);
    assert_eq!(f.browser("POST", "/v1/projects", &cookie, false, json!({"name": "lab"})).await.0, StatusCode::FORBIDDEN);
    assert_eq!(f.browser("POST", "/v1/projects", &cookie, true, json!({"name": "lab"})).await.0, StatusCode::CREATED);
    // Sign out ends the session; signing in again starts a new one.
    assert_eq!(f.browser("DELETE", "/v1/session", &cookie, true, Value::Null).await.0, StatusCode::NO_CONTENT);
    assert_eq!(f.browser("GET", "/v1/me", &cookie, false, Value::Null).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(f.browser("POST", "/v1/session", "", false, json!({"name": "sam", "password": "wrong password"})).await.0, StatusCode::UNAUTHORIZED);
    let (status, _, again) = f.browser("POST", "/v1/session", "", false, json!({"name": "sam", "password": "long enough!"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.browser("GET", "/v1/me", &again.unwrap(), false, Value::Null).await.1["name"], "sam");
}

#[tokio::test]
async fn quotas_cap_creates_and_resizes_but_evict_nothing() {
    let f = Fixture::new();
    f.call("POST", &format!("/v1/nodes/{}/policy", f.node), &f.admin, json!({"cpu_ratio": 8.0, "memory_ratio": 8.0, "disk_ratio": 8.0})).await;
    assert_eq!(f.create("one").await.0, StatusCode::CREATED);
    assert_eq!(f.call("PUT", "/v1/projects/alice:default/quota", &f.user, json!({"vcpus": 1})).await.0, StatusCode::FORBIDDEN);
    let (status, quota) = f.call("PUT", "/v1/projects/alice:default/quota", &f.admin, json!({"vcpus": 1})).await;
    assert_eq!((status, quota["used"]["vcpus"].as_u64()), (StatusCode::OK, Some(1)));
    let (status, body) = f.create("two").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["message"].as_str().unwrap().contains("capped at 1 vcpu"));
    // The dry run's "most you could have" counts the cap, not just the node.
    let check = json!({"name":"two","vcpus":4,"memory_mb":512,"arch":"x86_64","image":"ubuntu","disk_gb":5});
    let (_, fit) = f.call("POST", "/v1/projects/default/instances/check", &f.user, check).await;
    assert_eq!((fit["fits"].as_bool(), fit["largest"]["vcpus"].as_u64()), (Some(false), Some(0)));
    // Resizing within the cap is fine; growing past it is refused.
    let id = f.instance_row("one").id.to_string();
    f.call("POST", "/v1/projects/default/instances/one/stop", &f.user, Value::Null).await;
    f.report_state(&id, "stopped").await;
    assert_eq!(f.call("POST", "/v1/projects/default/instances/one/resize", &f.user, json!({"memory_mb": 512})).await.0, StatusCode::OK);
    assert_eq!(f.call("POST", "/v1/projects/default/instances/one/resize", &f.user, json!({"vcpus": 2})).await.0, StatusCode::CONFLICT);
    let (_, quotas) = f.call("GET", "/v1/quotas", &f.user, Value::Null).await;
    assert_eq!(quotas.as_array().unwrap().len(), 1, "users see only their own projects");
}

#[tokio::test]
async fn events_follow_what_happened_and_stay_inside_their_project() {
    let f = Fixture::new();
    let (_, vm) = f.create("vm").await;
    let id = vm["id"].as_str().unwrap();
    assert_eq!(f.report_state(id, "running").await, StatusCode::OK);
    let (_, inst) = f.call("GET", "/v1/projects/default/instances/vm", &f.user, Value::Null).await;
    assert!(inst["running_since"].as_i64().is_some());
    assert_eq!(inst["owner"], "alice");
    f.call("POST", "/v1/projects/default/instances/vm/stop", &f.user, Value::Null).await;
    f.report_state(id, "stopped").await;
    let (_, events) = f.call("GET", "/v1/events?project=default&instance=vm", &f.user, Value::Null).await;
    let kinds: Vec<&str> = events.as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["stopped", "stop", "running", "created"], "newest first");
    assert_eq!(events[1]["actor"], "alice");
    assert!(events[2]["node"].is_null(), "users never see which node");
    let (_, all) = f.call("GET", "/v1/events", &f.admin, Value::Null).await;
    assert_eq!(all[2]["node"], "node");
    // Admin's own project events stay out of alice's view.
    f.call("POST", "/v1/projects/default/instances", &f.admin, json!({"name":"private","vcpus":1,"memory_mb":128,"arch":"x86_64","image":"ubuntu","disk_gb":1,"ssh_keys":[KEY]})).await;
    let (_, mine) = f.call("GET", "/v1/events", &f.user, Value::Null).await;
    assert!(mine.as_array().unwrap().iter().all(|e| e["instance"] != "private"));
}

#[tokio::test]
async fn fit_checks_explain_refusals_and_show_nodes_to_admins_only() {
    let f = Fixture::new();
    let body = json!({"name":"big","vcpus":4,"memory_mb":1024,"arch":"x86_64","image":"ubuntu","disk_gb":10});
    let (status, fit) = f.call("POST", "/v1/projects/default/instances/check", &f.user, body.clone()).await;
    assert_eq!((status, fit["fits"].as_bool()), (StatusCode::OK, Some(false)));
    assert!(fit["reason"].as_str().unwrap().contains("1 vcpu"));
    assert!(fit["nodes"].is_null());
    let (_, fit) = f.call("POST", "/v1/projects/default/instances/check", &f.admin, body).await;
    assert_eq!(fit["nodes"][0]["hostname"], "node");
    let small = json!({"name":"small","vcpus":1,"memory_mb":512,"arch":"x86_64","image":"ubuntu","disk_gb":5});
    assert_eq!(f.call("POST", "/v1/projects/default/instances/check", &f.user, small).await.1["fits"], true);
    assert_eq!(f.call("GET", "/v1/projects/default/instances", &f.user, Value::Null).await.1, json!([]), "a check creates nothing");
}

#[tokio::test]
async fn a_vm_needs_a_public_ssh_key() {
    let f = Fixture::new();
    let create = "/v1/projects/default/instances";
    let body = |keys: Value| json!({"name":"vm","vcpus":1,"memory_mb":512,"arch":"x86_64","image":"ubuntu","disk_gb":5,"ssh_keys":keys});
    let (status, err) = f.call("POST", create, &f.user, body(json!([]))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err["message"].as_str().unwrap().contains("ssh key"));
    let private = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmU=\n-----END OPENSSH PRIVATE KEY-----";
    assert_eq!(f.call("POST", create, &f.user, body(json!([private]))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.call("POST", create, &f.user, body(json!([format!("{KEY}\nruncmd: [reboot]")]))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.call("POST", create, &f.user, body(json!(["alice@laptop"]))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.call("GET", create, &f.user, Value::Null).await.1, json!([]), "nothing made");
    // The dry run doesn't need one: the console checks a shape before the key is in.
    assert_eq!(f.call("POST", &format!("{create}/check"), &f.user, body(json!([]))).await.0, StatusCode::OK);
    assert_eq!(f.call("POST", create, &f.user, body(json!([KEY]))).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn saved_keys_go_into_vms_that_name_none() {
    let f = Fixture::new();
    let (status, key) = f.call("POST", "/v1/keys", &f.user, json!({"public_key": KEY})).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!((key["name"].as_str(), key["fingerprint"].as_str()), (Some("alice@laptop"), Some("SHA256:ARQPlI2pp175E3pLrJpmwPKuVA/HFV/dmIQTvtceRa8")));
    let (status, err) = f.call("POST", "/v1/keys", &f.user, json!({"public_key": KEY, "name": "again"})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(err["message"].as_str().unwrap().contains("alice@laptop"));
    assert_eq!(f.call("POST", "/v1/keys", &f.user, json!({"public_key": "-----BEGIN OPENSSH PRIVATE KEY-----"})).await.0, StatusCode::BAD_REQUEST);
    // Keys are personal, even from admins.
    assert_eq!(f.call("GET", "/v1/keys", &f.admin, Value::Null).await.1, json!([]));
    let id = key["id"].as_str().unwrap();
    assert_eq!(f.call("DELETE", &format!("/v1/keys/{id}"), &f.admin, Value::Null).await.0, StatusCode::NOT_FOUND);
    // A create with no keys gets the saved ones.
    let body = json!({"name":"vm","vcpus":1,"memory_mb":512,"arch":"x86_64","image":"ubuntu","disk_gb":5});
    assert_eq!(f.call("POST", "/v1/projects/default/instances", &f.user, body).await.0, StatusCode::CREATED);
    assert_eq!(f.instance_row("vm").spec.ssh_keys, vec![KEY.to_string()]);
    let (_, vm) = f.call("GET", "/v1/projects/default/instances/vm", &f.user, Value::Null).await;
    assert_eq!(vm["ssh_keys"], json!([{"fingerprint": "SHA256:ARQPlI2pp175E3pLrJpmwPKuVA/HFV/dmIQTvtceRa8", "comment": "alice@laptop"}]));
    assert_eq!(f.call("DELETE", &format!("/v1/keys/{id}"), &f.user, Value::Null).await.0, StatusCode::NO_CONTENT);
    assert_eq!(f.instance_row("vm").spec.ssh_keys, vec![KEY.to_string()], "a VM keeps what it was given");
}

#[tokio::test]
async fn tokens_are_shown_once_listed_by_owner_and_revocable() {
    let f = Fixture::new();
    let (status, new) = f.call("POST", "/v1/tokens", &f.user, json!({"name": "laptop"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let secret = new["secret"].as_str().unwrap();
    assert_eq!(f.call("GET", "/v1/me", secret, Value::Null).await.1["name"], "alice");
    let (_, mine) = f.call("GET", "/v1/tokens", &f.user, Value::Null).await;
    assert!(mine.as_array().unwrap().iter().all(|t| t["owner"] == "alice" && t.get("secret").is_none()));
    let (_, everyone) = f.call("GET", "/v1/tokens", &f.admin, Value::Null).await;
    let admin_token = everyone.as_array().unwrap().iter().find(|t| t["owner"] == "admin").unwrap()["id"].as_str().unwrap().to_string();
    assert_eq!(f.call("DELETE", &format!("/v1/tokens/{admin_token}"), &f.user, Value::Null).await.0, StatusCode::NOT_FOUND);
    assert_eq!(f.call("DELETE", &format!("/v1/tokens/{}", new["token"]["id"].as_str().unwrap()), &f.user, Value::Null).await.0, StatusCode::NO_CONTENT);
    assert_eq!(f.call("GET", "/v1/me", secret, Value::Null).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn removing_people_never_implies_deleting_their_vms() {
    let f = Fixture::new();
    f.create("vm").await;
    assert_eq!(f.call("DELETE", "/v1/users/alice", &f.admin, Value::Null).await.0, StatusCode::CONFLICT);
    assert_eq!(f.call("DELETE", "/v1/users/admin", &f.admin, Value::Null).await.0, StatusCode::CONFLICT, "not yourself");
    assert_eq!(f.call("POST", "/v1/users/admin/role", &f.admin, json!({"role": "user"})).await.0, StatusCode::CONFLICT, "keep one admin");
    let (_, users) = f.call("GET", "/v1/users", &f.admin, Value::Null).await;
    assert_eq!(users.as_array().unwrap().len(), 2);
    assert_eq!(f.call("GET", "/v1/users", &f.user, Value::Null).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_silent_node_is_logged_once_and_its_return_too() {
    let f = Fixture::new();
    {
        let conn = f.state.db.lock().unwrap();
        conn.execute("UPDATE nodes SET last_seen = last_seen - 120", []).unwrap();
        crate::sweeper::mark_quiet_nodes(&conn).unwrap();
        crate::sweeper::mark_quiet_nodes(&conn).unwrap();
    }
    f.report_state(&InstanceId::new_v4().to_string(), "absent").await;
    let (_, events) = f.call("GET", "/v1/events", &f.admin, Value::Null).await;
    let kinds: Vec<&str> = events.as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["node back", "node quiet"]);
}
