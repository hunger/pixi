//! `pixi serve-test` — quick client-side smoke tests against a running
//! `pixi serve`.
//!
//! Currently exposes a single `ping` subcommand that opens a varlink
//! connection on the socket given via the global `--socket` flag, sends
//! one `Ping` request, prints the echoed reply, and exits.

use clap::Parser;
use miette::{IntoDiagnostic, miette};
use pixi_varlink::EchoProxy;

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

pub async fn execute(args: Args, global_options: &GlobalOptions) -> miette::Result<()> {
    let socket = global_options.socket.as_deref().ok_or_else(|| {
        miette!(
            help = "pass --socket <PATH> pointing at a running `pixi serve`",
            "pixi serve-test: --socket is required"
        )
    })?;

    match args.command {
        SubCommand::Ping(PingArgs { message }) => {
            let mut conn = pixi_varlink::connect(socket).await.into_diagnostic()?;
            match conn.ping(&message).await.into_diagnostic()? {
                Ok(reply) => {
                    println!("{}", reply.message);
                    Ok(())
                }
                Err(err) => Err(miette!("server returned error: {err:?}")),
            }
        }
    }
}
