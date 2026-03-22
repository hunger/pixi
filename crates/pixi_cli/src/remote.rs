mod global_install;
mod progress;
pub mod serve;
mod version;

use clap::Parser;

/// Interact with a remote pixi varlink server
#[derive(Parser, Debug)]
pub struct Args {
    /// Varlink server address (socket path or unix:/path or tcp:host:port).
    /// Required for client commands (version, info, global). Not used by `serve`.
    #[arg(long, env = "PIXI_REMOTE")]
    pub address: Option<String>,

    #[command(subcommand)]
    pub command: RemoteCommand,
}

#[derive(Parser, Debug)]
pub enum RemoteCommand {
    /// Start a varlink IPC server for programmatic access to pixi
    Serve(serve::Args),
    /// Information about the system, workspace and environments for the current machine
    Info(crate::info::Args),
    /// Subcommand for global package management via the remote server
    Global(GlobalCommand),
    /// Print the remote server's pixi version
    Version,
}

#[derive(Parser, Debug)]
pub struct GlobalCommand {
    #[command(subcommand)]
    pub command: GlobalSubCommand,
}

#[derive(Parser, Debug)]
pub enum GlobalSubCommand {
    /// Install packages globally on the remote server
    Install(crate::global::install::Args),
}

pub async fn execute(args: Args) -> miette::Result<()> {
    match args.command {
        RemoteCommand::Serve(serve_args) => serve::execute(args.address, serve_args).await,
        cmd => {
            let address = args
                .address
                .ok_or_else(|| miette::miette!("--address is required for this command"))?;
            let address = pixi_varlink::normalize_address(&address);
            match cmd {
                RemoteCommand::Info(info_args) => crate::info::execute(info_args).await,
                RemoteCommand::Global(global) => match global.command {
                    GlobalSubCommand::Install(install_args) => {
                        global_install::execute(&address, install_args).await
                    }
                },
                RemoteCommand::Version => version::execute(&address).await,
                RemoteCommand::Serve(_) => unreachable!(),
            }
        }
    }
}
