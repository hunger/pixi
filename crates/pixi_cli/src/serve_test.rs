//! `pixi serve-test` — quick client-side smoke tests against a running
//! `pixi serve`.
//!
//! Currently exposes a single `ping` subcommand. It opens a varlink
//! connection on the socket given via the global `--socket` flag, completes
//! the `Hello` / `Authenticate` handshake against the current working
//! directory, sends one `Ping` request, prints the echoed reply, and exits.

use std::path::Path;

use clap::Parser;
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
}

/// Arguments for `pixi serve-test ping`.
#[derive(Parser, Debug)]
pub struct PingArgs {
    /// Message to echo. Must be non-empty (the server rejects empty messages).
    #[arg(default_value = "ping")]
    pub message: String,
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
