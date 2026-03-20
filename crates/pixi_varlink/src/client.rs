use std::path::{Path, PathBuf};

use crate::dev_prefix_pixi::{self, ExposedBinary, VarlinkClientInterface as _};
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
    pub client_envs_dir: String,
}

/// Result of a remote global install.
pub struct GlobalInstallResult {
    /// The server-side environment name (SHA-based).
    pub env_name: String,
    /// The absolute path to the environment on the server.
    pub env_path: PathBuf,
    /// Where the client should symlink the environment.
    pub local_env_symlink: PathBuf,
    /// The exposed binaries.
    pub binaries: Vec<InstalledBinary>,
}

/// A binary exposed by the server, with symlink target for the client.
pub struct InstalledBinary {
    pub name: String,
    pub server_path: PathBuf,
    pub local_symlink: PathBuf,
}

fn auth_file_path(client_envs_dir: &str, challenge: &str) -> PathBuf {
    PathBuf::from(client_envs_dir).join(format!(".pixi-server-auth-{challenge}"))
}

/// Send a global install request with challenge-response authentication.
///
/// Returns the install result including environment and binary info. The
/// caller should call [`create_symlinks`] to link everything locally.
pub async fn global_install(
    address: &str,
    args: GlobalInstallArgs,
    on_progress: &dyn Fn(&str),
) -> miette::Result<GlobalInstallResult> {
    let client_envs_dir = args.client_envs_dir.clone();
    let client_env_name = args.environment.clone();

    let connection = varlink::AsyncConnection::with_address(address)
        .await
        .map_err(|e| miette::miette!("failed to connect to {address}: {e}"))?;

    let client = dev_prefix_pixi::VarlinkClient::new(connection);

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
            args.client_envs_dir,
        )
        .call()
        .await
        .map_err(|e| miette::miette!("GlobalInstall failed: {e}"))?;

    let challenge = reply.challenge;
    let auth_file = auth_file_path(&client_envs_dir, &challenge);

    std::fs::File::create(&auth_file)
        .map_err(|e| miette::miette!("failed to create {}: {e}", auth_file.display()))?;

    confirm_and_cleanup(
        &client,
        &challenge,
        &auth_file,
        &client_envs_dir,
        client_env_name.as_deref(),
        on_progress,
    )
    .await
}

async fn confirm_and_cleanup(
    client: &dev_prefix_pixi::VarlinkClient,
    challenge: &str,
    auth_file: &Path,
    client_envs_dir: &str,
    client_env_name: Option<&str>,
    on_progress: &dyn Fn(&str),
) -> miette::Result<GlobalInstallResult> {
    let result = confirm_streaming(client, challenge, on_progress).await;

    // Always delete the auth file
    if let Err(e) = std::fs::remove_file(auth_file) {
        tracing::warn!(
            path = %auth_file.display(),
            error = %e,
            "failed to remove auth file",
        );
    }

    let reply = result?;

    let env_name = reply
        .env_name
        .ok_or_else(|| miette::miette!("server did not return env_name"))?;
    let env_path = reply
        .env_path
        .ok_or_else(|| miette::miette!("server did not return env_path"))?;
    let binaries_raw = reply.binaries.unwrap_or_default();

    let envs_dir = PathBuf::from(client_envs_dir);
    let pixi_bin_dir = envs_dir
        .parent()
        .expect("envs dir should have a parent")
        .join("bin");

    let local_env_name = client_env_name.unwrap_or(&env_name);
    let local_env_symlink = envs_dir.join(local_env_name);

    let binaries = binaries_raw
        .into_iter()
        .map(|bin| to_installed_binary(bin, &pixi_bin_dir))
        .collect();

    Ok(GlobalInstallResult {
        env_name,
        env_path: PathBuf::from(env_path),
        local_env_symlink,
        binaries,
    })
}

async fn confirm_streaming(
    client: &dev_prefix_pixi::VarlinkClient,
    challenge: &str,
    on_progress: &dyn Fn(&str),
) -> miette::Result<dev_prefix_pixi::ConfirmGlobalInstall_Reply> {
    let mut method_call = client.confirm_global_install(challenge.to_string());
    let stream = method_call
        .more()
        .await
        .map_err(|e| miette::miette!("ConfirmGlobalInstall failed: {e}"))?;

    loop {
        let reply = stream
            .recv()
            .await
            .map_err(|e| miette::miette!("ConfirmGlobalInstall recv failed: {e}"))?;

        if stream.continues() {
            if let Some(msg) = &reply.message {
                on_progress(msg);
            }
        } else {
            return Ok(reply);
        }
    }
}

fn to_installed_binary(bin: ExposedBinary, pixi_bin_dir: &Path) -> InstalledBinary {
    InstalledBinary {
        local_symlink: pixi_bin_dir.join(&bin.name),
        server_path: PathBuf::from(&bin.path),
        name: bin.name,
    }
}

#[cfg(unix)]
fn force_symlink(target: &Path, link: &Path) -> miette::Result<()> {
    if link.exists() || link.is_symlink() {
        std::fs::remove_file(link)
            .map_err(|e| miette::miette!("failed to remove {}: {e}", link.display()))?;
    }
    std::os::unix::fs::symlink(target, link).map_err(|e| {
        miette::miette!(
            "failed to symlink {} -> {}: {e}",
            link.display(),
            target.display()
        )
    })
}

/// Create symlinks for the environment and its binaries in the client's
/// `~/.pixi/envs` and `~/.pixi/bin`.
pub fn create_symlinks(result: &GlobalInstallResult) -> miette::Result<()> {
    // Symlink the environment: ~/.pixi/envs/<name> -> server env path
    if let Some(parent) = result.local_env_symlink.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| miette::miette!("failed to create {}: {e}", parent.display()))?;
    }
    force_symlink(&result.env_path, &result.local_env_symlink)?;
    tracing::debug!(
        env_name = %result.env_name,
        symlink = %result.local_env_symlink.display(),
        target = %result.env_path.display(),
        "created environment symlink",
    );

    // Symlink binaries: ~/.pixi/bin/<name> -> server bin path
    if let Some(first) = result.binaries.first() {
        if let Some(bin_dir) = first.local_symlink.parent() {
            std::fs::create_dir_all(bin_dir)
                .map_err(|e| miette::miette!("failed to create {}: {e}", bin_dir.display()))?;
        }
    }
    for bin in &result.binaries {
        force_symlink(&bin.server_path, &bin.local_symlink)?;
        tracing::debug!(
            name = %bin.name,
            symlink = %bin.local_symlink.display(),
            target = %bin.server_path.display(),
            "created binary symlink",
        );
    }

    Ok(())
}
