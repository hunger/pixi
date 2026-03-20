use std::path::PathBuf;

use crate::dev_prefix_pixi::{self, VarlinkClientInterface as _};
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

/// Arguments for a remote global install request.
pub struct GlobalInstallArgs {
    pub packages: Vec<String>,
    pub channels: Vec<String>,
    pub platform: Option<String>,
    pub environment: Option<String>,
    pub expose: Vec<String>,
    pub with: Vec<String>,
    pub force_reinstall: bool,
    pub no_shortcuts: bool,
    pub client_home: String,
}

fn auth_file_path(client_home: &str, challenge: &str) -> PathBuf {
    PathBuf::from(client_home).join(format!(".pixi-server-auth-{challenge}"))
}

/// Send a global install request with challenge-response authentication.
///
/// 1. Sends the install request; the server returns a challenge UUID.
/// 2. Creates `.pixi-server-auth-<challenge>` in `client_home`.
/// 3. Calls `ConfirmGlobalInstall` so the server verifies the file.
/// 4. Deletes the auth file regardless of outcome.
pub async fn global_install(address: &str, args: GlobalInstallArgs) -> miette::Result<()> {
    let client_home = args.client_home.clone();

    let connection = varlink::AsyncConnection::with_address(address)
        .await
        .map_err(|e| miette::miette!("failed to connect to {address}: {e}"))?;

    let client = dev_prefix_pixi::VarlinkClient::new(connection);

    // Step 1: send install request, get challenge
    let reply = client
        .global_install(
            args.packages,
            args.channels,
            args.platform,
            args.environment,
            args.expose,
            args.with,
            args.force_reinstall,
            args.no_shortcuts,
            args.client_home,
        )
        .call()
        .await
        .map_err(|e| miette::miette!("GlobalInstall failed: {e}"))?;

    let challenge = reply.challenge;
    let auth_file = auth_file_path(&client_home, &challenge);

    // Step 2: create the auth file
    std::fs::File::create(&auth_file)
        .map_err(|e| miette::miette!("failed to create {}: {e}", auth_file.display()))?;

    // Step 3: confirm — always clean up the file afterward
    let result = confirm_and_cleanup(&client, &challenge, &auth_file).await;

    result
}

async fn confirm_and_cleanup(
    client: &dev_prefix_pixi::VarlinkClient,
    challenge: &str,
    auth_file: &PathBuf,
) -> miette::Result<()> {
    // Need a new connection for the second call (varlink is one-call-per-connection)
    // Actually the VarlinkClient shares a connection — let's try it first.
    let result = client
        .confirm_global_install(challenge.to_string())
        .call()
        .await
        .map_err(|e| miette::miette!("ConfirmGlobalInstall failed: {e}"));

    // Always delete the auth file
    if let Err(e) = std::fs::remove_file(auth_file) {
        tracing::warn!(
            path = %auth_file.display(),
            error = %e,
            "failed to remove auth file",
        );
    }

    result?;
    Ok(())
}
