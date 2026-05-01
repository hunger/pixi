//! `pixi serve` — run the pixi varlink IPC server.
//!
//! Resolution order for the listening socket:
//!   1. `--socket <PATH>` on the command line.
//!   2. `serve.socket` in the user/global pixi config.
//!   3. The systemd socket-activation protocol (`LISTEN_PID` / `LISTEN_FDS`).
//!
//! If none of the above yield a socket, the command exits with an error.

use std::path::PathBuf;

use clap::Parser;
use miette::{IntoDiagnostic, miette};
use pixi_config::Config;

/// Run the pixi varlink IPC server.
#[derive(Parser, Debug)]
pub struct Args {
    /// Path of the Unix domain socket to bind. Overrides `serve.socket` from
    /// the configuration. When omitted, falls back to systemd socket
    /// activation.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let socket_path = args
        .socket
        .or_else(|| Config::load_global().serve.socket.clone());

    match socket_path {
        Some(path) => {
            tracing::info!("pixi serve: binding {}", path.display());
            pixi_varlink::serve(path).await.into_diagnostic()
        }
        None => match pixi_varlink::take_socket_activation_listener().into_diagnostic()? {
            Some(listener) => {
                tracing::info!("pixi serve: using systemd socket activation");
                pixi_varlink::serve_on(listener).await.into_diagnostic()
            }
            None => Err(miette!(
                help = "pass --socket <PATH>, set `serve.socket` in pixi config, or run pixi under systemd socket activation",
                "pixi serve: no socket configured"
            )),
        },
    }
}
