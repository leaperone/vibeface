mod agent;
mod codex;
mod local_api;
mod mcp;
mod runtime;
mod store;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use vibeface_protocol::{CommandRequest, CommandStatus};

#[derive(Parser)]
#[command(version, about = "Local Codex bridge and optional Cursor task manager")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    Run(Box<runtime::RunConfig>),
    Call {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        json: String,
    },
    Mcp {
        #[arg(long)]
        socket: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Action::Run(config) => runtime::run(*config).await,
        Action::Call { socket, json } => {
            let request: CommandRequest =
                serde_json::from_str(&json).context("invalid command JSON")?;
            request.validate().map_err(anyhow::Error::msg)?;
            let record = mcp::call_local(&socket, &request).await?;
            println!("{}", serde_json::to_string(&record)?);
            if record.status != CommandStatus::Succeeded {
                anyhow::bail!("command returned {:?}", record.status);
            }
            Ok(())
        }
        Action::Mcp { socket } => mcp::serve_mcp(&socket).await,
    }
}
