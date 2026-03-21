#![allow(non_camel_case_types, non_snake_case)]

pub mod client;
mod dev_prefix_pixi;
// Will be used by methods that need a pixi_api::Interface (Install, Add, etc.)
#[allow(dead_code)]
mod non_interactive;
mod reporter;
pub(crate) mod server;
mod streaming_handler;

use std::path::PathBuf;
use std::sync::Arc;

use varlink::{AsyncVarlinkService, ListenAsyncConfig, listen_async};

use crate::server::PixiVarlinkService;
use crate::streaming_handler::StreamingHandler;

const FALLBACK_NONCE: &str = "pixi-varlink-default-nonce";

/// Read the nonce from `$CREDENTIALS_DIRECTORY/nonce`, falling back to a
/// hardcoded default when not running under systemd socket activation.
fn load_nonce() -> String {
    if let Ok(creds_dir) = std::env::var("CREDENTIALS_DIRECTORY") {
        let path = PathBuf::from(creds_dir).join("nonce");
        match std::fs::read_to_string(&path) {
            Ok(nonce) => {
                let nonce = nonce.trim().to_string();
                tracing::info!("loaded nonce from {}", path.display());
                return nonce;
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "failed to read nonce, using fallback",
                );
            }
        }
    } else {
        tracing::debug!("CREDENTIALS_DIRECTORY not set, using fallback nonce");
    }
    FALLBACK_NONCE.to_string()
}

/// Start a varlink server listening on the given address.
///
/// Address format: `unix:/path/to/socket`, `tcp:host:port`, or a bare path.
pub async fn run_server(address: &str, base_dir: PathBuf, cache_dir: PathBuf) -> miette::Result<()> {
    let nonce = load_nonce();
    let pixi_service = Arc::new(PixiVarlinkService::new(nonce, base_dir, cache_dir));
    let streaming = Arc::new(StreamingHandler::new(pixi_service));

    let service = Arc::new(AsyncVarlinkService::new(
        "dev.prefix.pixi",
        "Pixi",
        pixi_consts::consts::PIXI_VERSION,
        "https://pixi.sh",
        vec![streaming],
    ));

    tracing::info!(address = %address, "listening");

    let result = listen_async(service, address, &ListenAsyncConfig::default()).await;

    match &result {
        Ok(()) => tracing::info!("server stopped"),
        Err(e) => tracing::error!(error = %e, "server error"),
    }

    result.map_err(|e| miette::miette!("varlink server error: {e}"))
}
