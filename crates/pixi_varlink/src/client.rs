use std::path::{Path, PathBuf};

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
    pub client_envs_dir: String,
}

/// An installed package name and version.
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
}

/// Result of a remote global install.
pub struct GlobalInstallResult {
    /// The SHA identifier for this install.
    pub sha: String,
    /// The absolute path to the SHA directory on the server.
    pub sha_dir: PathBuf,
    /// The client's pixi home (parent of envs dir, e.g. ~/.pixi).
    pub pixi_home: PathBuf,
    /// The client-facing environment name.
    pub display_env_name: String,
    /// Packages that were installed/changed.
    pub packages: Vec<InstalledPackage>,
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

    let sha = reply
        .sha
        .ok_or_else(|| miette::miette!("server did not return sha"))?;
    let sha_dir = PathBuf::from(
        reply
            .sha_dir
            .ok_or_else(|| miette::miette!("server did not return sha_dir"))?,
    );

    let envs_dir = PathBuf::from(client_envs_dir);
    let pixi_home = envs_dir
        .parent()
        .expect("envs dir should have a parent")
        .to_path_buf();

    let display_env_name = client_env_name.unwrap_or(&sha).to_string();

    let packages = reply
        .packages
        .unwrap_or_default()
        .into_iter()
        .map(|p| InstalledPackage {
            name: p.name,
            version: p.version,
        })
        .collect();

    Ok(GlobalInstallResult {
        sha,
        sha_dir,
        pixi_home,
        display_env_name,
        packages,
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

/// Link a file or directory from `target` to `link`, trying:
/// hardlink → reflink → symlink → copy → error.
fn link_entry(target: &Path, link: &Path) -> miette::Result<&'static str> {
    if target.is_file() {
        if std::fs::hard_link(target, link).is_ok() {
            return Ok("hardlink");
        }
        if reflink_copy::reflink(target, link).is_ok() {
            return Ok("reflink");
        }
    }

    #[cfg(unix)]
    if std::os::unix::fs::symlink(target, link).is_ok() {
        return Ok("symlink");
    }

    if target.is_file() {
        std::fs::copy(target, link).map_err(|e| {
            miette::miette!("failed to link {} -> {}: {e}", link.display(), target.display())
        })?;
        return Ok("copy");
    }

    Err(miette::miette!(
        "failed to link {} -> {}",
        link.display(),
        target.display(),
    ))
}

/// Link the server's install into the client's `~/.pixi/`.
///
/// Scans `$sha_dir` and for each top-level subdirectory, links every
/// entry inside it into the matching `$pixi_home/<subdir>/` directory
/// using the best available method (reflink → hardlink → symlink → copy).
///
/// If any link target already exists, all links created in this run
/// are removed before returning an error.
pub fn create_symlinks(result: &GlobalInstallResult) -> miette::Result<()> {
    let mut created: Vec<PathBuf> = Vec::new();

    let outcome = do_create_links(result, &mut created);

    if let Err(err) = &outcome {
        tracing::warn!(error = %err, "rolling back {} links", created.len());
        for link in created.iter().rev() {
            if let Err(e) = std::fs::remove_file(link) {
                tracing::warn!(path = %link.display(), error = %e, "rollback failed");
            }
        }
    }

    outcome
}

fn do_create_links(
    result: &GlobalInstallResult,
    created: &mut Vec<PathBuf>,
) -> miette::Result<()> {
    let pixi_home = &result.pixi_home;
    let sha_dir = &result.sha_dir;

    let top_entries = std::fs::read_dir(sha_dir)
        .map_err(|e| miette::miette!("failed to read {}: {e}", sha_dir.display()))?;

    for top_entry in top_entries {
        let top_entry = top_entry
            .map_err(|e| miette::miette!("failed to read {}: {e}", sha_dir.display()))?;

        if !top_entry.path().is_dir() {
            continue;
        }

        let subdir_name = top_entry.file_name();
        let subdir_str = subdir_name.to_string_lossy();

        // Skip server-internal directories that shouldn't be merged
        if subdir_str == "manifests" {
            continue;
        }
        let server_subdir = top_entry.path();
        let local_subdir = pixi_home.join(&subdir_name);

        std::fs::create_dir_all(&local_subdir)
            .map_err(|e| miette::miette!("failed to create {}: {e}", local_subdir.display()))?;

        let entries = std::fs::read_dir(&server_subdir)
            .map_err(|e| miette::miette!("failed to read {}: {e}", server_subdir.display()))?;

        for entry in entries {
            let entry = entry.map_err(|e| {
                miette::miette!("failed to read {}: {e}", server_subdir.display())
            })?;

            // Under envs/, entries are directories (each env) — symlink them.
            // Under other dirs (bin/, etc.), skip subdirectories like
            // trampoline_configuration which are internal pixi state.
            let ft = entry.file_type().map_err(|e| {
                miette::miette!("failed to stat {}: {e}", entry.path().display())
            })?;
            if ft.is_dir() && subdir_name != "envs" {
                continue;
            }

            let file_name = entry.file_name();
            let target = entry.path();
            let link = local_subdir.join(&file_name);

            if link.exists() || link.is_symlink() {
                return Err(miette::miette!(
                    "{}/{} already exists",
                    subdir_name.to_string_lossy(),
                    file_name.to_string_lossy(),
                ));
            }

            let method = link_entry(&target, &link)?;
            tracing::debug!(
                method,
                link = %link.display(),
                target = %target.display(),
                "linked",
            );
            created.push(link);
        }
    }

    Ok(())
}
