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
    },
    Start { name: String },
    Stop { name: String },
    /// Delete an instance. Final: the boot disk goes with it.
    Rm { name: String },
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
                println!("{}", x["name"].as_str().unwrap_or("?"));
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
        Cmd::Create { name, vcpus, memory, image, disk, arch } => {
            let arch: Arch = serde_json::from_value(serde_json::Value::String(arch)).context("arch must be x86_64 or aarch64")?;
            let body = serde_json::to_value(CreateInstance { name, vcpus, memory_mb: memory, arch, image, disk_gb: disk })?;
            let i: Instance = c.call(Method::POST, &format!("/v1/projects/{p}/instances"), Some(body)).await?;
            print_instance(&i);
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
        Cmd::Node(NodeCmd::Ls) => {
            let v: Vec<Node> = c.call(Method::GET, "/v1/nodes", None).await?;
            for n in v {
                println!("{} {:<16} {:<8} {}", n.id, n.hostname, serde_json::to_string(&n.arch)?.trim_matches('"'), serde_json::to_string(&n.status)?.trim_matches('"'));
            }
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
