//! `pixi serve-test` — quick client-side smoke tests against a running
//! `pixi serve`.
//!
//! Exposes:
//!
//! * `ping <message>` — round-trip a single `Ping` and print the reply.
//! * `install <env-name>` — drive the `Install` RPC end-to-end and print
//!   the server's prefix path. With `--inspect`, also assert the
//!   environment-fingerprint marker landed under the prefix.

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
    /// prefix-path reply.
    Install(InstallArgs),
}

/// Arguments for `pixi serve-test ping`.
#[derive(Parser, Debug)]
pub struct PingArgs {
    /// Message to echo. Must be non-empty (the server rejects empty messages).
    #[arg(default_value = "ping")]
    pub message: String,
}

/// Arguments for `pixi serve-test install`.
#[derive(Parser, Debug)]
pub struct InstallArgs {
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

    /// After install, assert that
    /// `<prefix>/conda-meta/.pixi-environment-fingerprint` exists. The
    /// command exits non-zero if the marker is missing — useful as a
    /// post-install sanity check in CI.
    #[arg(long)]
    pub inspect: bool,
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
        SubCommand::Install(args) => install(socket, args).await,
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

#[tracing::instrument(level = "info", name = "pixi.serve-test.install", skip_all, fields(env = %args.env_name, inspect = args.inspect))]
async fn install(socket: &Path, args: InstallArgs) -> miette::Result<()> {
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
        force_reinstall: false,
        extra_records: Vec::new(),
    };

    let stream = conn.install(request).await.into_diagnostic()?;
    let mut stream = std::pin::pin!(stream);
    let mut prefix: Option<String> = None;
    while let Some(item) = stream.next().await {
        let reply = item
            .into_diagnostic()?
            .map_err(|err| miette!("server returned error: {err:?}"))?;
        match reply {
            pixi_varlink::InstallReply::Progress { event } => {
                tracing::info!(?event, "install progress");
            }
            pixi_varlink::InstallReply::ReporterCall { call } => {
                // serve-test invokes the streaming RPC with `more=false`,
                // so the daemon never emits these — log if one shows up
                // so a misconfigured client doesn't lose data silently.
                tracing::info!(?call, "unexpected reporter call on non-streaming path");
            }
            pixi_varlink::InstallReply::Success {
                prefix: p,
                transaction: _,
            } => {
                prefix = Some(p);
                break;
            }
            pixi_varlink::InstallReply::Failed { error } => {
                return Err(miette!("install failed: {error:?}"));
            }
        }
    }
    let prefix =
        prefix.ok_or_else(|| miette!("install stream ended without a Success or Failed reply"))?;

    if args.inspect {
        // Post-install sanity: the fingerprint marker is what tells
        // future installs whether the prefix is up to date. If the
        // server skipped writing it (or hit an I/O error) the next
        // install will needlessly redo the rattler-installer pass, so
        // bail loudly here rather than silently shipping a partial
        // install.
        let marker = Path::new(&prefix)
            .join("conda-meta")
            .join(".pixi-environment-fingerprint");
        if !marker.exists() {
            return Err(miette!(
                "install completed but {} is missing",
                marker.display()
            ));
        }
    }

    println!("{prefix}");
    Ok(())
}
