//! `cu`: command line client for the copper user API.

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use copper_api::*;
use reqwest::{Method, StatusCode};

#[derive(Parser)]
#[command(name = "cu", version)]
struct Cli {
    #[arg(long, env = "CU_URL", default_value = "http://127.0.0.1:8400")]
    url: String,
    #[arg(long, env = "CU_TOKEN")]
    token: String,
    /// Project for instance commands.
    #[arg(long, short, env = "CU_PROJECT", default_value = "default")]
    project: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show who the token belongs to
    Whoami,
    /// Projects
    #[command(subcommand)]
    Project(ProjectCmd),
    /// List instances in the project
    Ls,
    /// Show one instance
    Get { name: String },
    /// Create an instance (desired state: running)
    Create {
        name: String,
        #[arg(long, default_value_t = 1)]
        vcpus: u32,
        #[arg(long, default_value_t = 1024)]
        memory: u64,
        #[arg(long, default_value = "ubuntu-24.04")]
        image: String,
        #[arg(long, default_value_t = 10)]
        disk: u64,
        #[arg(long, default_value = "x86_64")]
        arch: String,
        /// SSH public key (text or path). Repeatable. Defaults to ~/.ssh/*.pub.
        #[arg(long = "ssh-key")]
        ssh_keys: Vec<String>,
        /// Admin: place on exactly this node, or refuse.
        #[arg(long)]
        node: Option<NodeId>,
        /// Admin: spread instances of this group across nodes where possible.
        #[arg(long)]
        spread: Option<String>,
    },
    Start { name: String },
    Stop { name: String },
    /// Resize a stopped instance. Disks only grow.
    Resize {
        name: String,
        #[arg(long)]
        vcpus: Option<u32>,
        /// Memory in MB.
        #[arg(long)]
        memory: Option<u64>,
        /// Boot disk size in GB.
        #[arg(long)]
        disk: Option<u64>,
    },
    /// Base images in kiln's catalogue
    #[command(subcommand)]
    Image(ImageCmd),
    /// Delete an instance. Final: the boot disk goes with it.
    Rm { name: String },
    /// Admin: abandon tracking on a retired node. Does not erase host disks.
    Forget { id: InstanceId },
    /// Admin: nodes
    #[command(subcommand)]
    Node(NodeCmd),
}

#[derive(Subcommand)]
enum ProjectCmd {
    Ls,
    Create { name: String },
}

#[derive(Subcommand)]
enum NodeCmd {
    Ls,
    Approve { id: NodeId },
    Retire { id: NodeId },
    /// Set overcommit ratios: placement admits capacity × ratio.
    Set {
        id: NodeId,
        #[arg(long)]
        cpu_ratio: Option<f64>,
        #[arg(long)]
        memory_ratio: Option<f64>,
        #[arg(long)]
        disk_ratio: Option<f64>,
    },
}

#[derive(Subcommand)]
enum ImageCmd {
    Ls,
}

fn print_node(n: &Node) -> anyhow::Result<()> {
    let arch = serde_json::to_string(&n.arch)?.trim_matches('"').to_string();
    let status = serde_json::to_string(&n.status)?.trim_matches('"').to_string();
    let (cap, used, oc) = (n.capacity.clone().unwrap_or(Capacity { vcpus: 0, memory_mb: 0, disk_gb: 0 }),
        n.allocated.clone().unwrap_or(Capacity { vcpus: 0, memory_mb: 0, disk_gb: 0 }), n.overcommit);
    let total = |v: f64, r: f64| (v * r).floor() as u64;
    println!(
        "{} {:<16} {:<8} {:<8} vcpu {:>3}/{:<3} mem {:>6}/{:<6} MB disk {:>4}/{:<4} GB  ratios {}/{}/{}",
        n.id, n.hostname, arch, status,
        used.vcpus, total(cap.vcpus as f64, oc.cpu),
        used.memory_mb, total(cap.memory_mb as f64, oc.memory),
        used.disk_gb, total(cap.disk_gb as f64, oc.disk),
        oc.cpu, oc.memory, oc.disk,
    );
    Ok(())
}

struct Client {
    http: reqwest::Client,
    url: String,
    token: String,
}

impl Client {
    async fn call<T: serde::de::DeserializeOwned>(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> anyhow::Result<T> {
        let mut req = self.http.request(method, format!("{}{}", self.url, path)).bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.context("connecting to kiln")?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let msg = serde_json::from_str::<ApiError>(&text).map(|e| e.message).unwrap_or(text);
            bail!("{status}: {msg}");
        }
        if status == StatusCode::ACCEPTED || text.is_empty() {
            // No body; caller must ask for `()`.
            return Ok(serde_json::from_str("null")?);
        }
        Ok(serde_json::from_str(&text)?)
    }
}

/// Each entry is either a path to a .pub file or the key text itself. With no
/// entries, every ~/.ssh/*.pub is used.
fn resolve_ssh_keys(entries: Vec<String>) -> anyhow::Result<Vec<String>> {
    let mut keys = Vec::new();
    if entries.is_empty() {
        if let Some(home) = std::env::var_os("HOME") {
            let dir = std::path::Path::new(&home).join(".ssh");
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    if e.path().extension().is_some_and(|x| x == "pub") {
                        keys.push(std::fs::read_to_string(e.path())?.trim().to_string());
                    }
                }
            }
        }
    } else {
        for e in entries {
            let p = std::path::Path::new(&e);
            if p.is_file() {
                keys.push(std::fs::read_to_string(p)?.trim().to_string());
            } else if e.starts_with("ssh-") || e.starts_with("ecdsa-") {
                keys.push(e.trim().to_string());
            } else {
                bail!("{e}: not a public key file or key text");
            }
        }
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

fn print_instance(i: &Instance) {
    let state = serde_json::to_string(&i.state).unwrap().trim_matches('"').to_string();
    let node = i.node.map(|n| n.to_string()[..8].to_string()).unwrap_or_default();
    println!(
        "{:<20} {:<9} {:>2} vcpu {:>6} MB {:>4} GB  {:<28} {}{}",
        i.name,
        state,
        i.vcpus,
        i.memory_mb,
        i.disk_gb,
        i.addresses.join(","),
        node,
        i.error.as_deref().map(|e| format!("  ({e})")).unwrap_or_default()
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let c = Client { http: reqwest::Client::new(), url: cli.url.trim_end_matches('/').to_string(), token: cli.token };
    let p = &cli.project;
    match cli.cmd {
        Cmd::Whoami => {
            let v: serde_json::Value = c.call(Method::GET, "/v1/me", None).await?;
            println!("{} ({})", v["name"].as_str().unwrap_or("?"), v["role"].as_str().unwrap_or("?"));
        }
        Cmd::Project(ProjectCmd::Ls) => {
            let v: Vec<serde_json::Value> = c.call(Method::GET, "/v1/projects", None).await?;
            for x in v {
                println!("{}:{}", x["owner"].as_str().unwrap_or("?"), x["name"].as_str().unwrap_or("?"));
            }
        }
        Cmd::Project(ProjectCmd::Create { name }) => {
            let _: serde_json::Value = c.call(Method::POST, "/v1/projects", Some(serde_json::json!({ "name": name }))).await?;
            println!("created project {name}");
        }
        Cmd::Ls => {
            let v: Vec<Instance> = c.call(Method::GET, &format!("/v1/projects/{p}/instances"), None).await?;
            for i in &v {
                print_instance(i);
            }
        }
        Cmd::Get { name } => {
            let i: Instance = c.call(Method::GET, &format!("/v1/projects/{p}/instances/{name}"), None).await?;
            println!("{}", serde_json::to_string_pretty(&i)?);
        }
        Cmd::Create { name, vcpus, memory, image, disk, arch, ssh_keys, node, spread } => {
            let arch: Arch = serde_json::from_value(serde_json::Value::String(arch)).context("arch must be x86_64 or aarch64")?;
            let ssh_keys = resolve_ssh_keys(ssh_keys)?;
            let body = serde_json::to_value(CreateInstance { name, vcpus, memory_mb: memory, arch, image, disk_gb: disk, ssh_keys, node, spread })?;
            let i: Instance = c.call(Method::POST, &format!("/v1/projects/{p}/instances"), Some(body)).await?;
            print_instance(&i);
        }
        Cmd::Resize { name, vcpus, memory, disk } => {
            let body = serde_json::to_value(ResizeInstance { vcpus, memory_mb: memory, disk_gb: disk })?;
            let i: Instance = c.call(Method::POST, &format!("/v1/projects/{p}/instances/{name}/resize"), Some(body)).await?;
            print_instance(&i);
        }
        Cmd::Image(ImageCmd::Ls) => {
            let v: Vec<Image> = c.call(Method::GET, "/v1/images", None).await?;
            for i in v {
                println!("{:<24} {:<8} {}  {} MB", i.name, serde_json::to_string(&i.arch)?.trim_matches('"'), &i.sha256[..12], i.size_bytes / (1024 * 1024));
            }
        }
        Cmd::Start { name } => {
            let i: Instance = c.call(Method::POST, &format!("/v1/projects/{p}/instances/{name}/start"), None).await?;
            print_instance(&i);
        }
        Cmd::Stop { name } => {
            let i: Instance = c.call(Method::POST, &format!("/v1/projects/{p}/instances/{name}/stop"), None).await?;
            print_instance(&i);
        }
        Cmd::Rm { name } => {
            let _: () = c.call(Method::DELETE, &format!("/v1/projects/{p}/instances/{name}"), None).await?;
            println!("deleting {name}");
        }
        Cmd::Forget { id } => {
            let _: () = c.call(Method::POST, &format!("/v1/instances/{id}/forget"), None).await?;
            println!("forgot {id}; VM and disk cleanup on the retired host is manual");
        }
        Cmd::Node(NodeCmd::Ls) => {
            let v: Vec<Node> = c.call(Method::GET, "/v1/nodes", None).await?;
            for n in v {
                print_node(&n)?;
            }
        }
        Cmd::Node(NodeCmd::Set { id, cpu_ratio, memory_ratio, disk_ratio }) => {
            let body = serde_json::to_value(NodePolicy { cpu_ratio, memory_ratio, disk_ratio })?;
            let n: Node = c.call(Method::POST, &format!("/v1/nodes/{id}/policy"), Some(body)).await?;
            print_node(&n)?;
        }
        Cmd::Node(NodeCmd::Approve { id }) => {
            let n: Node = c.call(Method::POST, &format!("/v1/nodes/{id}/approve"), None).await?;
            println!("{} {} {:?}", n.id, n.hostname, n.status);
        }
        Cmd::Node(NodeCmd::Retire { id }) => {
            let n: Node = c.call(Method::POST, &format!("/v1/nodes/{id}/retire"), None).await?;
            println!("{} {} {:?}", n.id, n.hostname, n.status);
        }
    }
    Ok(())
}
