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
    /// Subcommand for global package management via the remote server
    Global(RemoteGlobalCommand),
}

#[derive(Parser, Debug)]
pub struct RemoteGlobalCommand {
    #[command(subcommand)]
    pub command: RemoteGlobalSubCommand,
}

#[derive(Parser, Debug)]
pub enum RemoteGlobalSubCommand {
    /// Install packages globally on the remote server
    Install(crate::global::install::Args),
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let address = pixi_varlink::client::normalize_address(&args.address);

    if args.version {
        return print_remote_version(&address).await;
    }

    match args.command {
        Some(RemoteCommand::Info(info_args)) => crate::info::execute(info_args).await,
        Some(RemoteCommand::Global(global)) => execute_global(&address, global).await,
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

async fn execute_global(address: &str, cmd: RemoteGlobalCommand) -> miette::Result<()> {
    match cmd.command {
        RemoteGlobalSubCommand::Install(args) => execute_global_install(address, args).await,
    }
}

async fn execute_global_install(
    address: &str,
    args: crate::global::install::Args,
) -> miette::Result<()> {
    let envs_dir = pixi_global::EnvRoot::from_env()
        .await?
        .path()
        .to_path_buf();
    // Ensure the directory exists before telling the server about it
    tokio::fs::create_dir_all(&envs_dir)
        .await
        .map_err(|e| miette::miette!("failed to create {}: {e}", envs_dir.display()))?;

    let install_args = pixi_varlink::client::GlobalInstallArgs {
        packages: args.packages.specs.clone(),
        channels: args.channels.iter().map(|c| c.to_string()).collect(),
        platform: args.platform.map(|p| p.to_string()),
        environment: args.environment.map(|e| e.to_string()),
        expose: args.expose.iter().map(|m| m.to_string()).collect(),
        with: args.with.iter().map(|s| s.to_string()).collect(),
        force_reinstall: args.force_reinstall,
        no_shortcuts: args.no_shortcuts,
        client_envs_dir: envs_dir.to_string_lossy().to_string(),
    };

    let result = pixi_varlink::client::global_install(address, install_args, &|msg| {
        eprintln!("{msg}");
    })
    .await?;
    pixi_varlink::client::create_symlinks(&result)?;

    eprintln!(
        "{} -> {}",
        result.local_env_symlink.display(),
        result.env_path.display()
    );
    for bin in &result.binaries {
        eprintln!("{} -> {}", bin.local_symlink.display(), bin.server_path.display());
    }
    Ok(())
}
