use miette::Context;

use crate::dev_prefix_pixi::{self, VarlinkClientInterface as _};

pub use crate::dev_prefix_pixi::{EnvironmentInfo, WorkspaceInfo};

/// Normalize a varlink address: bare paths become `unix:` addresses.
pub fn normalize_address(address: &str) -> String {
    if address.starts_with("unix:") || address.starts_with("tcp:") {
        address.to_string()
    } else if address.starts_with('/') || address.starts_with('.') {
        format!("unix:{address}")
    } else {
        address.to_string()
    }
}

/// Connect to a pixi varlink server and call `Info`.
pub async fn info(
    address: &str,
    manifest_path: Option<String>,
) -> miette::Result<WorkspaceInfo> {
    let connection = varlink::AsyncConnection::with_address(address)
        .await
        .map_err(|e| miette::miette!("failed to connect to {address}: {e}"))?;

    let client = dev_prefix_pixi::VarlinkClient::new(connection);
    let reply = client
        .info(manifest_path)
        .call()
        .await
        .map_err(|e| miette::miette!("{e}"))
        .wrap_err_with(|| format!("varlink call to {address} failed"))?;

    Ok(reply.workspace)
}
