use std::io::Write;

use clap::Parser;
use miette::IntoDiagnostic;

/// Interact with a remote pixi varlink server
#[derive(Parser, Debug)]
pub struct Args {
    /// Varlink server address (socket path or unix:/path or tcp:host:port)
    #[arg(long, env = "PIXI_REMOTE")]
    pub address: String,

    /// Print the remote server's pixi version
    #[arg(long)]
    pub version: bool,

    #[command(subcommand)]
    pub command: Option<RemoteCommand>,
}

#[derive(Parser, Debug)]
pub enum RemoteCommand {
    /// Information about the system, workspace and environments for the current machine
    Info(crate::info::Args),
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let address = pixi_varlink::client::normalize_address(&args.address);

    if args.version {
        return print_remote_version(&address).await;
    }

    match args.command {
        Some(RemoteCommand::Info(info_args)) => crate::info::execute(info_args).await,
        None => print_remote_version(&address).await,
    }
}

async fn print_remote_version(address: &str) -> miette::Result<()> {
    let remote = pixi_varlink::client::version(address).await?;
    let local = pixi_consts::consts::PIXI_VERSION;

    if local == remote {
        writeln!(std::io::stdout(), "{remote}").into_diagnostic()?;
    } else {
        writeln!(std::io::stdout(), "client: {local}").into_diagnostic()?;
        writeln!(std::io::stdout(), "server: {remote}").into_diagnostic()?;
    }
    Ok(())
}
