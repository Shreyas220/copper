//! `cu`: command line client for the copper user API.

use clap::{Parser, Subcommand};
use copper_api::*;

#[derive(Parser)]
#[command(name = "cu", version)]
struct Cli {
    #[arg(long, env = "CU_URL", default_value = "http://127.0.0.1:8400")]
    url: String,
    #[arg(long, env = "CU_TOKEN")]
    token: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List instances
    Ls,
    /// Admin: list nodes
    Nodes,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();
    match cli.cmd {
        Cmd::Ls => {
            let v: Vec<Instance> = client.get(format!("{}/v1/instances", cli.url)).send().await?.json().await?;
            for i in v {
                println!("{:<20} {:<10} {:>3} vCPU {:>6} MB  {:?}", i.name, format!("{:?}", i.state).to_lowercase(), i.vcpus, i.memory_mb, i.addresses);
            }
        }
        Cmd::Nodes => {
            let v: Vec<Node> = client.get(format!("{}/v1/nodes", cli.url)).send().await?.json().await?;
            for n in v {
                println!("{:<36} {:<16} {:?} {:?}", n.id, n.hostname, n.arch, n.status);
            }
        }
    }
    Ok(())
}
