use varlink_stdinterfaces::org_varlink_service_async::VarlinkClientInterface as _;

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

/// Connect to a pixi varlink server and return its version string.
pub async fn version(address: &str) -> miette::Result<String> {
    let connection = varlink::AsyncConnection::with_address(address)
        .await
        .map_err(|e| miette::miette!("failed to connect to {address}: {e}"))?;

    let client = varlink_stdinterfaces::org_varlink_service_async::VarlinkClient::new(connection);
    let info = client
        .get_info()
        .call()
        .await
        .map_err(|e| miette::miette!("GetInfo call failed: {e}"))?;

    Ok(info.version)
}
