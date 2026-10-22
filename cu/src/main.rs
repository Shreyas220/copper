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
    Start {
        name: String,
        /// Return once it's running (or failed).
        #[arg(long)]
        wait: bool,
    },
    Stop {
        name: String,
        /// Return once it's stopped. Needed before `cu resize`.
        #[arg(long)]
        wait: bool,
    },
    /// What happened lately, newest first: the whole project, or one instance
    Log {
        name: Option<String>,
        #[arg(long, default_value_t = 30)]
        limit: u32,
    },
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
    /// SSH into a running instance, tunnelled through kiln so it works
    /// wherever cu does, the tailnet included
    Ssh {
        name: String,
        /// Login user. Ubuntu cloud images use `ubuntu`.
        #[arg(long, short = 'l', default_value = "ubuntu")]
        user: String,
        /// Remote command to run instead of a shell.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Print an ssh_config entry so ssh, scp and rsync reach an instance as
    /// <name>.copper. The tunnel reads CU_TOKEN from the environment.
    SshConfig {
        name: String,
        #[arg(long, short = 'l', default_value = "ubuntu")]
        user: String,
    },
    /// Relay stdin and stdout to an instance's SSH port (ssh's ProxyCommand)
    #[command(hide = true)]
    Tunnel { name: String },
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

/// Quote for the shell ssh runs ProxyCommand with, and escape ssh's own `%` tokens.
fn proxy_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''").replace('%', "%%"))
}

/// The ProxyCommand that tunnels to `name` through kiln.
fn proxy_command(url: &str, project: &str, name: &str) -> anyhow::Result<String> {
    let exe = std::env::current_exe().context("locating the cu binary")?;
    Ok(format!(
        "{} --url {} --project {} tunnel {}",
        proxy_quote(&exe.to_string_lossy()), proxy_quote(url), proxy_quote(project), proxy_quote(name)
    ))
}

/// Poll until the instance reaches `target`. A failure ends the wait early
/// with the node's error.
async fn wait_for(c: &Client, project: &str, name: &str, target: InstanceState) -> anyhow::Result<Instance> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let i: Instance = c.call(Method::GET, &format!("/v1/projects/{project}/instances/{name}"), None).await?;
        if i.state == target {
            return Ok(i);
        }
        if i.state == InstanceState::Failed {
            bail!("{name} failed: {}", i.error.unwrap_or_default());
        }
        if std::time::Instant::now() > deadline {
            bail!("{name} is still {} after 5 minutes", serde_json::to_string(&i.state)?.trim_matches('"'));
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// `40s`, `12m`, `3h`, `2d`.
fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

async fn running_instance(c: &Client, project: &str, name: &str) -> anyhow::Result<Instance> {
    let i: Instance = c.call(Method::GET, &format!("/v1/projects/{project}/instances/{name}"), None).await?;
    if i.state != InstanceState::Running {
        bail!("{name} is {}; it must be running", serde_json::to_string(&i.state)?.trim_matches('"'));
    }
    Ok(i)
}

/// Upgrade an HTTP request to kiln into a byte stream to the instance's SSH
/// port, then shuttle stdin and stdout across it.
async fn tunnel(c: &Client, project: &str, name: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let resp = c.http
        .get(format!("{}/v1/projects/{project}/instances/{name}/ssh", c.url))
        .bearer_auth(&c.token)
        .header(reqwest::header::CONNECTION, "upgrade")
        .header(reqwest::header::UPGRADE, "copper-tcp")
        .send()
        .await
        .context("connecting to kiln")?;
    let status = resp.status();
    if status != StatusCode::SWITCHING_PROTOCOLS {
        let text = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<ApiError>(&text).map(|e| e.message).unwrap_or(text);
        bail!("{status}: {msg}");
    }
    let (mut from_vm, mut to_vm) = tokio::io::split(resp.upgrade().await.context("opening the tunnel")?);
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut to_vm).await;
        let _ = to_vm.shutdown().await;
    });
    let mut stdout = tokio::io::stdout();
    tokio::io::copy(&mut from_vm, &mut stdout).await?;
    stdout.flush().await?;
    Ok(())
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
            if ssh_keys.is_empty() {
                bail!("no ssh key in ~/.ssh/*.pub; make one with ssh-keygen or pass --ssh-key");
            }
            let body = serde_json::to_value(CreateInstance { name, vcpus, memory_mb: memory, arch, image, disk_gb: disk, ssh_keys, node, spread })?;
            let i: Instance = c.call(Method::POST, &format!("/v1/projects/{p}/instances"), Some(body)).await?;
            print_instance(&i);
        }
        Cmd::Resize { name, vcpus, memory, disk } => {
            let body = serde_json::to_value(ResizeInstance { vcpus, memory_mb: memory, disk_gb: disk })?;
            let i: Instance = c.call(Method::POST, &format!("/v1/projects/{p}/instances/{name}/resize"), Some(body)).await?;
            print_instance(&i);
        }
        Cmd::Ssh { name, user, command } => {
            let i = running_instance(&c, p, &name).await?;
            let mut ssh = std::process::Command::new("ssh");
            // The token reaches the ProxyCommand through the environment,
            // never through arguments other users could see in ps.
            ssh.env("CU_TOKEN", &c.token)
                .arg("-o").arg(format!("ProxyCommand={}", proxy_command(&c.url, p, &name)?))
                // Addresses get reused; the instance id never does.
                .arg("-o").arg(format!("HostKeyAlias=copper-{}", i.id))
                .arg("-o").arg("StrictHostKeyChecking=accept-new")
                .arg("-l").arg(&user)
                .arg(&name)
                .args(&command);
            // Hand the terminal and signals to ssh.
            let err = std::os::unix::process::CommandExt::exec(&mut ssh);
            bail!("running ssh: {err}");
        }
        Cmd::SshConfig { name, user } => {
            let i: Instance = c.call(Method::GET, &format!("/v1/projects/{p}/instances/{name}"), None).await?;
            println!("Host {name}.copper");
            println!("    User {user}");
            println!("    HostKeyAlias copper-{}", i.id);
            println!("    StrictHostKeyChecking accept-new");
            println!("    ProxyCommand {}", proxy_command(&c.url, p, &name)?);
        }
        Cmd::Tunnel { name } => {
            if let Err(e) = tunnel(&c, p, &name).await {
                eprintln!("cu: {e:#}");
                std::process::exit(1);
            }
            // stdin is read on a blocking thread that would hold the runtime open.
            std::process::exit(0);
        }
        Cmd::Image(ImageCmd::Ls) => {
            let v: Vec<Image> = c.call(Method::GET, "/v1/images", None).await?;
            for i in v {
                println!("{:<24} {:<8} {}  {} MB", i.name, serde_json::to_string(&i.arch)?.trim_matches('"'), &i.sha256[..12], i.size_bytes / (1024 * 1024));
            }
        }
        Cmd::Start { name, wait } => {
            let mut i: Instance = c.call(Method::POST, &format!("/v1/projects/{p}/instances/{name}/start"), None).await?;
            if wait {
                i = wait_for(&c, p, &name, InstanceState::Running).await?;
            }
            print_instance(&i);
        }
        Cmd::Stop { name, wait } => {
            let mut i: Instance = c.call(Method::POST, &format!("/v1/projects/{p}/instances/{name}/stop"), None).await?;
            if wait {
                i = wait_for(&c, p, &name, InstanceState::Stopped).await?;
            }
            print_instance(&i);
        }
        Cmd::Log { name, limit } => {
            let mut path = format!("/v1/events?project={p}&limit={limit}");
            if let Some(name) = &name {
                path.push_str(&format!("&instance={name}"));
            }
            let events: Vec<Event> = c.call(Method::GET, &path, None).await?;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
            for e in events {
                let who = e.actor.map(|a| format!("  ({a})")).unwrap_or_default();
                println!("{:>8}  {:<14} {:<20} {}{who}", ago(now - e.at), e.kind, e.instance.unwrap_or_default(), e.detail);
            }
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
