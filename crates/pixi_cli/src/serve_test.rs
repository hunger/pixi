//! `pixi serve-test` — quick client-side smoke tests against a running
//! `pixi serve`.
//!
//! Exposes:
//!
//! * `ping <message>` — round-trip a single `Ping` and print the reply.
//! * `install-dry-run <env-name>` — drive the `Install` RPC end-to-end,
//!   print the prefix path the server *would* materialise, and exit. No
//!   solve / fetch / install runs (that lands in step 2).

use std::path::Path;

use clap::Parser;
use futures::StreamExt;
use miette::{IntoDiagnostic, miette};

use crate::GlobalOptions;

/// Talk to a running `pixi serve` over its varlink socket.
#[derive(Parser, Debug)]
pub struct Args {
    #[command(subcommand)]
    pub command: SubCommand,
}

#[derive(Parser, Debug)]
pub enum SubCommand {
    /// Send a single `Ping` and exit when the reply lands.
    Ping(PingArgs),
    /// Drive the `Install` RPC end-to-end and print the server's
    /// prefix-path reply. Step 1 returns a deterministic path
    /// (`<data>/<HASH>/`) without writing anything.
    InstallDryRun(InstallDryRunArgs),
}

/// Arguments for `pixi serve-test ping`.
#[derive(Parser, Debug)]
pub struct PingArgs {
    /// Message to echo. Must be non-empty (the server rejects empty messages).
    #[arg(default_value = "ping")]
    pub message: String,
}

/// Arguments for `pixi serve-test install-dry-run`.
#[derive(Parser, Debug)]
pub struct InstallDryRunArgs {
    /// Environment name to install. Must match `EnvironmentName`
    /// (alphanumeric, `_`, `-`).
    pub env_name: String,

    /// Match-spec to install into the environment. Repeatable; defaults
    /// to a single spec equal to the env name.
    #[arg(long = "spec", value_name = "MATCHSPEC")]
    pub specs: Vec<String>,

    /// Channel URL to query during solve. Repeatable. Defaults to
    /// `https://prefix.dev/conda-forge` when no `--channel` is given.
    #[arg(long = "channel", value_name = "URL")]
    pub channels: Vec<String>,

    /// Platform to solve for (e.g. `linux-64`). Defaults to the daemon's
    /// host platform.
    #[arg(long, value_name = "PLATFORM")]
    pub platform: Option<String>,
}

#[tracing::instrument(level = "info", name = "pixi.serve-test", skip_all)]
pub async fn execute(args: Args, global_options: &GlobalOptions) -> miette::Result<()> {
    let socket = global_options.socket.as_deref().ok_or_else(|| {
        miette!(
            help = "pass --socket <PATH> pointing at a running `pixi serve`",
            "pixi serve-test: --socket is required"
        )
    })?;

    match args.command {
        SubCommand::Ping(args) => ping(socket, args).await,
        SubCommand::InstallDryRun(args) => install_dry_run(socket, args).await,
    }
}

#[tracing::instrument(level = "info", name = "pixi.serve-test.ping", skip_all, fields(message = %args.message))]
async fn ping(socket: &Path, args: PingArgs) -> miette::Result<()> {
    // `connect` performs the Hello/Authenticate handshake against the cwd and
    // only hands back a [`pixi_varlink::Connection`] once the server has
    // accepted us; there is no way to call `ping` against the unauthenticated
    // typestate.
    let cwd = std::env::current_dir().into_diagnostic()?;
    let mut conn = pixi_varlink::connect(socket, &cwd)
        .await
        .into_diagnostic()?;
    match conn.ping(&args.message).await.into_diagnostic()? {
        Ok(reply) => {
            println!("{}", reply.message);
            Ok(())
        }
        Err(err) => Err(miette!("server returned error: {err:?}")),
    }
}

#[tracing::instrument(level = "info", name = "pixi.serve-test.install-dry-run", skip_all, fields(env = %args.env_name))]
async fn install_dry_run(socket: &Path, args: InstallDryRunArgs) -> miette::Result<()> {
    let cwd = std::env::current_dir().into_diagnostic()?;
    let mut conn = pixi_varlink::connect(socket, &cwd)
        .await
        .into_diagnostic()?;

    let specs = if args.specs.is_empty() {
        vec![args.env_name.clone()]
    } else {
        args.specs
    };
    let channels = if args.channels.is_empty() {
        vec!["https://prefix.dev/conda-forge".to_string()]
    } else {
        args.channels
    };

    let request = pixi_varlink::InstallRequest {
        env_name: args.env_name,
        specs,
        channels,
        platform: args.platform,
        expose: Vec::new(),
        force_reinstall: false,
    };

    let stream = conn.install(request).await.into_diagnostic()?;
    let mut stream = std::pin::pin!(stream);
    while let Some(item) = stream.next().await {
        let reply = item
            .into_diagnostic()?
            .map_err(|err| miette!("server returned error: {err:?}"))?;
        match reply {
            pixi_varlink::InstallReply::Progress { event } => {
                tracing::info!(?event, "install progress");
            }
            pixi_varlink::InstallReply::Success { prefix } => {
                println!("{prefix}");
                return Ok(());
            }
            pixi_varlink::InstallReply::Failed { error } => {
                return Err(miette!("install failed: {error:?}"));
            }
        }
    }
    Err(miette!(
        "install stream ended without a Success or Failed reply"
    ))
}
