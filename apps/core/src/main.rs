mod desktop_parent;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::Arc,
};
use vibeface_core::runtime::Runtime;

#[derive(Parser, Debug)]
#[command(
    name = "vibeface-core",
    version,
    about = "Vibeface personal device network core"
)]
struct CoreArgs {
    /// Data directory owned by this core process.
    #[arg(long, env = "VIBEFACE_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Disable browser access for this run.
    #[arg(long)]
    no_web: bool,
    /// Static App directory. Defaults to a sibling web/ directory when present.
    #[arg(long, env = "VIBEFACE_APP_DIR")]
    app_dir: Option<PathBuf>,
    /// Read one bounded control request from stdin and write one bounded JSON response.
    #[arg(long, hide = true)]
    control: bool,
    /// Stop this desktop-owned Core normally when its parent's private stdin pipe closes.
    #[arg(long, hide = true, conflicts_with = "control")]
    desktop_parent_stdin: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = CoreArgs::parse();
    if args.control {
        let mut input = Vec::new();
        std::io::stdin()
            .take(vibeface_control::MAX_CONTROL_PACKET as u64 + 1)
            .read_to_end(&mut input)?;
        if input.len() > vibeface_control::MAX_CONTROL_PACKET {
            bail!("core control request is too large");
        }
        let request: vibeface_control::ControlRequest =
            serde_json::from_slice(&input).context("invalid core control request")?;
        let response = match vibeface_core::control::handle(request).await {
            Ok(result) => vibeface_control::ControlResponse::success(result),
            Err(error) => vibeface_control::ControlResponse::failure(format!("{error:#}")),
        };
        let mut output = serde_json::to_vec(&response)?;
        if output.len().saturating_add(1) > vibeface_control::MAX_CONTROL_PACKET {
            output = serde_json::to_vec(&vibeface_control::ControlResponse::failure(
                "core control response is too large",
            ))?;
        }
        output.push(b'\n');
        std::io::stdout().write_all(&output)?;
        return Ok(());
    }
    let mut desktop_parent = desktop_parent::DesktopParent::watch(args.desktop_parent_stdin)?;
    let data_dir = args
        .data_dir
        .or_else(|| std::env::var_os("VIBEFACE_DATA_DIR").map(PathBuf::from))
        .context("--data-dir or VIBEFACE_DATA_DIR is required")?;
    vibeface_core::logging::init(Some(&data_dir), false);
    vibeface_core::control::prepare_start(&data_dir)?;
    let app_dir = match args.app_dir {
        Some(path) => {
            anyhow::ensure!(
                path.is_dir(),
                "app directory does not exist or is not a directory: {}",
                path.display()
            );
            Some(path)
        }
        None => {
            let sibling = std::env::current_exe()?
                .parent()
                .map(|parent| parent.join("web"));
            sibling.filter(|path| path.is_dir())
        }
    };
    let mut runtime = Runtime::open(&data_dir).await?;
    runtime.config.web_enabled &= !args.no_web;
    let runtime = Arc::new(runtime);
    let startup = tokio::select! {
        result = runtime.start() => result,
        result = desktop_parent.closed() => {
            runtime.shutdown().await;
            return result;
        }
    };
    if let Err(error) = startup {
        runtime.shutdown().await;
        return Err(error);
    }
    let mut rejoin = false;
    let result = tokio::select! {
        result = vibeface_core::api::serve(runtime.clone(), app_dir) => result,
        result = vibeface_core::maintenance::serve(runtime.clone(), data_dir.clone()) => result,
        _ = runtime.rejoin_requested() => { rejoin = true; Ok(()) }
        result = desktop_parent.closed() => result,
        stop = vibeface_bridge::platform::stop::requested() => stop.map(drop).map_err(Into::into),
    };
    runtime.shutdown().await;
    if rejoin {
        return vibeface_core::control::restart_fresh(&data_dir);
    }
    result
}
