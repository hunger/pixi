#![allow(non_camel_case_types, non_snake_case)]

mod dev_prefix_pixi;
// Will be used by methods that need a pixi_api::Interface (Install, Add, etc.)
#[allow(dead_code)]
mod non_interactive;
mod server;

use std::sync::Arc;

use varlink::{AsyncVarlinkService, ListenAsyncConfig, listen_async};

use crate::server::PixiVarlinkService;

/// Start a varlink server listening on the given address.
///
/// Address format: `unix:/path/to/socket` or `tcp:host:port`.
pub async fn run_server(address: &str) -> miette::Result<()> {
    let handler = Arc::new(dev_prefix_pixi::new(Arc::new(PixiVarlinkService)));

    let service = Arc::new(AsyncVarlinkService::new(
        "dev.prefix.pixi",
        "Pixi",
        pixi_consts::consts::PIXI_VERSION,
        "https://pixi.sh",
        vec![handler],
    ));

    tracing::info!("varlink server listening on {address}");

    listen_async(service, address, &ListenAsyncConfig::default())
        .await
        .map_err(|e| miette::miette!("varlink server error: {e}"))
}
