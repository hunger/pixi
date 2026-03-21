use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Mutex;

use async_trait::async_trait;
use pixi_consts::consts;
use pixi_core::WorkspaceLocator;
use pixi_global::{EnvironmentName, Mapping, Project};
use pixi_manifest::{FeaturesExt, HasFeaturesIter};
use rattler_conda_types::{MatchSpec, NamedChannelOrUrl, Platform};
use sha2::{Digest, Sha256};

use crate::dev_prefix_pixi::{
    Call_ConfirmGlobalInstall, Call_GlobalInstall, Call_Info, EnvironmentInfo,
    VarlinkCallError as _, VarlinkInterface, WorkspaceInfo,
};

#[allow(dead_code)]
struct PendingInstall {
    client_envs_dir: String,
    packages: Vec<String>,
    channels: Vec<String>,
    platform: Option<String>,
    environment: Option<String>,
    expose: Vec<String>,
    with: Vec<String>,
    force_reinstall: bool,
    no_shortcuts: bool,
}

pub struct PixiVarlinkService {
    nonce: String,
    base_dir: PathBuf,
    cache_dir: PathBuf,
    pending: Mutex<HashMap<String, PendingInstall>>,
}

impl PixiVarlinkService {
    pub fn new(nonce: String, base_dir: PathBuf, cache_dir: PathBuf) -> Self {
        Self {
            nonce,
            base_dir,
            cache_dir,
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn auth_file_path(client_envs_dir: &str, challenge: &str) -> PathBuf {
        PathBuf::from(client_envs_dir).join(format!(".pixi-server-auth-{challenge}"))
    }

    fn compute_env_name(&self, client_envs_dir: &str, environment: Option<&str>) -> String {
        let env = environment.unwrap_or("default");
        let mut hasher = Sha256::new();
        hasher.update(self.nonce.as_bytes());
        hasher.update(client_envs_dir.as_bytes());
        hasher.update(self.nonce.as_bytes());
        hasher.update(env.as_bytes());
        hasher.update(self.nonce.as_bytes());
        let hash = hasher.finalize();
        hash.chunks(2)
            .map(|pair| format!("{:02x}", pair[0] ^ pair[1]))
            .collect()
    }
}


struct InstallResult {
    packages: Vec<crate::dev_prefix_pixi::InstalledPackage>,
}

async fn perform_global_install(
    base_dir: &PathBuf,
    cache_dir: &PathBuf,
    env_name: &EnvironmentName,
    sha: &str,
    pending: &PendingInstall,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> miette::Result<InstallResult> {
    let sha_home = base_dir.join(sha);

    let mut project = Project::discover_or_create_in(sha_home.clone(), cache_dir.clone())
        .await?
        .with_cli_config(pixi_config::Config::load_global());

    if let Some(tx) = progress_tx {
        project = project.with_reporter_factory(move || {
            Box::new(crate::reporter::VarlinkReporter::new(tx.clone()))
        });
    }

    let channels: Vec<NamedChannelOrUrl> = pending
        .channels
        .iter()
        .map(|c| c.parse())
        .collect::<Result<_, _>>()
        .map_err(|e| miette::miette!("invalid channel: {e}"))?;

    let platform: Option<Platform> = pending
        .platform
        .as_ref()
        .map(|p| Platform::from_str(p))
        .transpose()
        .map_err(|e| miette::miette!("invalid platform: {e}"))?;

    let with_specs: Vec<MatchSpec> = pending
        .with
        .iter()
        .map(|s| MatchSpec::from_str(s, rattler_conda_types::ParseStrictness::Lenient))
        .collect::<Result<_, _>>()
        .map_err(|e| miette::miette!("invalid --with spec: {e}"))?;

    let expose_mappings: Vec<Mapping> = pending
        .expose
        .iter()
        .map(|s| Mapping::from_str(s))
        .collect::<Result<_, _>>()
        .map_err(|e| miette::miette!("invalid --expose mapping: {e}"))?;

    let channel_config = project.global_channel_config().clone();

    let specs: Vec<pixi_global::project::GlobalSpec> = pending
        .packages
        .iter()
        .map(|pkg| {
            let match_spec =
                MatchSpec::from_str(pkg, rattler_conda_types::ParseStrictness::Lenient)
                    .map_err(|e| miette::miette!("invalid package spec '{pkg}': {e}"))?;
            pixi_global::project::GlobalSpec::try_from_matchspec_with_name(
                match_spec,
                &channel_config,
            )
            .map_err(|e| miette::miette!("invalid package spec '{pkg}': {e}"))
        })
        .collect::<miette::Result<_>>()?;

    if pending.force_reinstall && project.environment(env_name).is_some() {
        let _ = project.remove_environment(env_name).await?;
    }

    let env_channels = if channels.is_empty() {
        project.config().default_channels()
    } else {
        channels
    };

    if !project.manifest.parsed.envs.contains_key(env_name) {
        project
            .manifest
            .add_environment(env_name, Some(env_channels))?;
    }

    if let Some(platform) = platform {
        project.manifest.set_platform(env_name, platform)?;
    }

    let with_global_specs: Vec<pixi_global::project::GlobalSpec> = with_specs
        .into_iter()
        .map(|spec| {
            pixi_global::project::GlobalSpec::try_from_matchspec_with_name(spec, &channel_config)
        })
        .collect::<Result<_, _>>()
        .map_err(|e| miette::miette!("invalid --with spec: {e}"))?;

    for spec in specs.iter().chain(with_global_specs.iter()) {
        project.manifest.add_dependency(env_name, spec)?;
    }

    if !expose_mappings.is_empty() {
        project.manifest.remove_all_exposed_mappings(env_name)?;
        for mapping in &expose_mappings {
            project.manifest.add_exposed_mapping(env_name, mapping)?;
        }
    }

    if project
        .environment_in_sync_internal(env_name, true)
        .await?
    {
        tracing::info!(env_name = %env_name, "environment already in sync");
        project.manifest.save().await?;

        return Ok(InstallResult {
            packages: vec![],
        });
    }

    let environment_update = project
        .install_environment_with_options(env_name, pending.force_reinstall)
        .await?;

    let with_package_names: Vec<_> = with_global_specs
        .iter()
        .filter_map(|spec| {
            let ms = MatchSpec::from_str(
                spec.name().as_normalized(),
                rattler_conda_types::ParseStrictness::Lenient,
            )
            .ok()?;
            ms.name.as_exact().cloned()
        })
        .collect();

    let expose_type = if !expose_mappings.is_empty() {
        pixi_global::project::ExposedType::Mappings(expose_mappings)
    } else if with_package_names.is_empty() {
        pixi_global::project::ExposedType::All
    } else {
        pixi_global::project::ExposedType::Ignore(with_package_names)
    };
    project
        .sync_exposed_names(env_name, expose_type)
        .await?;

    let _ = project
        .expose_executables_from_environment(env_name)
        .await?;

    let _ = project.sync_completions(env_name).await?;

    let requested_names: Vec<_> = specs.iter().map(|s| s.name().clone()).collect();
    let changes = environment_update.user_requested_changes(&requested_names);

    let packages: Vec<crate::dev_prefix_pixi::InstalledPackage> = changes
        .iter()
        .filter_map(|(name, change)| {
            let version = match change {
                pixi_global::common::InstallChange::Installed(v) => v.to_string(),
                pixi_global::common::InstallChange::Upgraded(_, v) => v.to_string(),
                pixi_global::common::InstallChange::Reinstalled(_, v) => v.to_string(),
                pixi_global::common::InstallChange::TransitiveUpgraded(_, v) => v.to_string(),
                pixi_global::common::InstallChange::Removed => return None,
            };
            tracing::info!(package = %name.as_normalized(), version = %version, "installed");
            Some(crate::dev_prefix_pixi::InstalledPackage {
                name: name.as_normalized().to_string(),
                version,
            })
        })
        .collect();

    project.manifest.save().await?;

    Ok(InstallResult {
        packages,
    })
}


#[async_trait]
impl VarlinkInterface for PixiVarlinkService {
    async fn global_install(
        &self,
        call: &mut dyn Call_GlobalInstall,
        packages: Vec<String>,
        channels: Vec<String>,
        platform: Option<String>,
        environment: Option<String>,
        expose: Vec<String>,
        with: Vec<String>,
        force_reinstall: bool,
        no_shortcuts: bool,
        client_envs_dir: String,
    ) -> varlink::Result<()> {
        let client_envs_dir = std::fs::canonicalize(&client_envs_dir)
            .map_err(|e| {
                varlink::error::Error(
                    varlink::ErrorKind::InvalidParameter(format!(
                        "cannot canonicalize client_envs_dir '{client_envs_dir}': {e}"
                    )),
                    None,
                    None,
                )
            })?
            .to_string_lossy()
            .to_string();

        tracing::info!(
            client_envs_dir = %client_envs_dir,
            packages = ?packages,
            channels = ?channels,
            platform = platform.as_deref(),
            environment = environment.as_deref(),
            expose = ?expose,
            with = ?with,
            force_reinstall,
            no_shortcuts,
            "GlobalInstall request",
        );

        let challenge = uuid::Uuid::new_v4().to_string();
        tracing::debug!(challenge = %challenge, client_envs_dir = %client_envs_dir, "issuing challenge");

        self.pending
            .lock()
            .expect("pending lock poisoned")
            .insert(
                challenge.clone(),
                PendingInstall {
                    client_envs_dir,
                    packages,
                    channels,
                    platform,
                    environment,
                    expose,
                    with,
                    force_reinstall,
                    no_shortcuts,
                },
            );

        call.reply(challenge)
    }

    async fn confirm_global_install(
        &self,
        call: &mut dyn Call_ConfirmGlobalInstall,
        challenge: String,
    ) -> varlink::Result<()> {
        tracing::debug!(challenge = %challenge, "ConfirmGlobalInstall request");

        let pending = self
            .pending
            .lock()
            .expect("pending lock poisoned")
            .remove(&challenge);

        let Some(pending) = pending else {
            tracing::warn!(challenge = %challenge, "unknown challenge");
            return call.reply_authentication_failed(challenge);
        };

        let auth_file = Self::auth_file_path(&pending.client_envs_dir, &challenge);

        if !auth_file.exists() {
            tracing::warn!(
                challenge = %challenge,
                expected = %auth_file.display(),
                "auth file not found",
            );
            return call.reply_authentication_failed(challenge);
        }

        let sha =
            self.compute_env_name(&pending.client_envs_dir, pending.environment.as_deref());
        let env_name = EnvironmentName::from_str(
            pending.environment.as_deref().unwrap_or("default"),
        )
        .map_err(|e| {
            varlink::error::Error(
                varlink::ErrorKind::InvalidParameter(e.to_string()),
                None,
                None,
            )
        })?;

        tracing::info!(
            challenge = %challenge,
            sha = %sha,
            client_envs_dir = %pending.client_envs_dir,
            "authentication succeeded, installing",
        );

        match perform_global_install(&self.base_dir, &self.cache_dir, &env_name, &sha, &pending, None).await {
            Ok(result) => {
                tracing::info!(
                    env_name = %env_name,
                    packages = result.packages.len(),
                    "global install completed",
                );
                let sha_dir = self.base_dir.join(&sha);
                call.reply(
                    None,
                    Some(sha.clone()),
                    Some(sha_dir.to_string_lossy().to_string()),
                    Some(result.packages),
                )
            }
            Err(err) => {
                tracing::error!(env_name = %env_name, error = %err, "global install failed");
                call.reply_global_install_failed(err.to_string())
            }
        }
    }

    async fn info(
        &self,
        call: &mut dyn Call_Info,
        manifest_path: Option<String>,
    ) -> varlink::Result<()> {
        tracing::debug!(manifest_path = manifest_path.as_deref(), "Info request");

        let mut locator = WorkspaceLocator::for_cli();
        if let Some(path) = &manifest_path {
            locator = locator.with_search_start(
                pixi_core::workspace::DiscoveryStart::ExplicitManifest(path.into()),
            );
        }

        let workspace = match locator.locate() {
            Ok(ws) => {
                tracing::debug!(
                    workspace = ws.display_name(),
                    manifest = %ws.workspace.provenance.path.display(),
                    "workspace found",
                );
                ws
            }
            Err(err) => {
                let path = manifest_path.unwrap_or_else(|| ".".to_string());
                tracing::warn!(path = %path, error = %err, "workspace not found");
                return call.reply_workspace_not_found(path);
            }
        };

        let environments: Vec<EnvironmentInfo> = workspace
            .environments()
            .iter()
            .map(|env| {
                let tasks = env
                    .tasks(Some(env.best_platform()))
                    .ok()
                    .map(|t| t.into_keys().map(|n| n.as_str().to_string()).collect())
                    .unwrap_or_default();

                EnvironmentInfo {
                    name: env.name().as_str().to_string(),
                    features: env
                        .features()
                        .map(|f| f.name.as_str().to_string())
                        .collect(),
                    solve_group: env.solve_group().map(|sg| sg.name().to_string()),
                    platforms: env.platforms().into_iter().map(|p| p.to_string()).collect(),
                    dependencies: env
                        .combined_dependencies(Some(env.best_platform()))
                        .names()
                        .map(|p| p.as_source().to_string())
                        .collect(),
                    pypi_dependencies: env
                        .pypi_dependencies(Some(env.best_platform()))
                        .into_iter()
                        .map(|(name, _)| name.as_source().to_string())
                        .collect(),
                    tasks,
                    prefix: env.dir().to_string_lossy().to_string(),
                }
            })
            .collect();

        tracing::debug!(
            workspace = workspace.display_name(),
            environments = environments.len(),
            "Info reply",
        );

        let version = workspace
            .workspace
            .value
            .workspace
            .version
            .as_ref()
            .map(|v| v.to_string());

        let info = WorkspaceInfo {
            name: workspace.display_name().to_string(),
            manifest_path: workspace
                .workspace
                .provenance
                .path
                .to_string_lossy()
                .to_string(),
            version,
            pixi_version: consts::PIXI_VERSION.to_string(),
            environments,
        };

        call.reply(info)
    }
}

impl PixiVarlinkService {
    /// Like `confirm_global_install` but sends progress through a channel
    /// and returns the final replies for the streaming handler to send.
    pub async fn confirm_global_install_streaming(
        &self,
        challenge: String,
        progress_tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> varlink::Result<Vec<varlink::Reply>> {
        tracing::debug!(challenge = %challenge, "ConfirmGlobalInstall (streaming)");

        let pending = self
            .pending
            .lock()
            .expect("pending lock poisoned")
            .remove(&challenge);

        let Some(pending) = pending else {
            tracing::warn!(challenge = %challenge, "unknown challenge");
            let mut call = crate::dev_prefix_pixi::AsyncCall::new(false, false);
            call.reply_authentication_failed(challenge)?;
            return Ok(call.take_replies());
        };

        let auth_file = Self::auth_file_path(&pending.client_envs_dir, &challenge);

        if !auth_file.exists() {
            tracing::warn!(
                challenge = %challenge,
                expected = %auth_file.display(),
                "auth file not found",
            );
            let mut call = crate::dev_prefix_pixi::AsyncCall::new(false, false);
            call.reply_authentication_failed(challenge)?;
            return Ok(call.take_replies());
        }

        let sha =
            self.compute_env_name(&pending.client_envs_dir, pending.environment.as_deref());
        let env_name = EnvironmentName::from_str(
            pending.environment.as_deref().unwrap_or("default"),
        )
        .map_err(|e| {
            varlink::error::Error(
                varlink::ErrorKind::InvalidParameter(e.to_string()),
                None,
                None,
            )
        })?;

        tracing::info!(
            challenge = %challenge,
            sha = %sha,
            client_envs_dir = %pending.client_envs_dir,
            "authentication succeeded, installing (streaming)",
        );

        let mut call = crate::dev_prefix_pixi::AsyncCall::new(false, false);

        match perform_global_install(&self.base_dir, &self.cache_dir, &env_name, &sha, &pending, Some(progress_tx)).await {
            Ok(result) => {
                tracing::info!(
                    env_name = %env_name,
                    packages = result.packages.len(),
                    "global install completed",
                );
                let sha_dir = self.base_dir.join(&sha);
                Call_ConfirmGlobalInstall::reply(
                    &mut call,
                    None,
                    Some(sha),
                    Some(sha_dir.to_string_lossy().to_string()),
                    Some(result.packages),
                )?;
            }
            Err(err) => {
                tracing::error!(env_name = %env_name, error = %err, "global install failed");
                crate::dev_prefix_pixi::VarlinkCallError::reply_global_install_failed(&mut call, err.to_string())?;
            }
        }

        // Drop the progress sender implicitly (it's moved into the reporter
        // factory and dropped when the install finishes)

        Ok(call.take_replies())
    }
}
