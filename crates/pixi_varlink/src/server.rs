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
    Call_ConfirmGlobalInstall, Call_GlobalInstall, Call_Info, EnvironmentInfo, VarlinkInterface,
    WorkspaceInfo,
};

#[allow(dead_code)]
struct PendingInstall {
    client_home: String,
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
    pending: Mutex<HashMap<String, PendingInstall>>,
}

impl PixiVarlinkService {
    pub fn new(nonce: String) -> Self {
        Self {
            nonce,
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn auth_file_path(client_home: &str, challenge: &str) -> PathBuf {
        PathBuf::from(client_home).join(format!(".pixi-server-auth-{challenge}"))
    }

    fn compute_env_name(&self, client_home: &str, environment: Option<&str>) -> String {
        let env = environment.unwrap_or("default");
        let mut hasher = Sha256::new();
        hasher.update(self.nonce.as_bytes());
        hasher.update(client_home.as_bytes());
        hasher.update(self.nonce.as_bytes());
        hasher.update(env.as_bytes());
        hasher.update(self.nonce.as_bytes());
        let hash = hasher.finalize();
        hash.chunks(2)
            .map(|pair| format!("{:02x}", pair[0] ^ pair[1]))
            .collect()
    }
}

async fn perform_global_install(
    env_name: &EnvironmentName,
    pending: &PendingInstall,
) -> miette::Result<()> {
    let mut project = Project::discover_or_create()
        .await?
        .with_cli_config(pixi_config::Config::load_global());

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

    // Parse packages into GlobalSpecs
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

    // Force-reinstall: remove existing environment first
    if pending.force_reinstall && project.environment(env_name).is_some() {
        let _ = project.remove_environment(env_name).await?;
    }

    // Set up channels
    let env_channels = if channels.is_empty() {
        project.config().default_channels()
    } else {
        channels
    };

    // Create the environment if it doesn't exist
    if !project.manifest.parsed.envs.contains_key(env_name) {
        project
            .manifest
            .add_environment(env_name, Some(env_channels))?;
    }

    // Set platform
    if let Some(platform) = platform {
        project.manifest.set_platform(env_name, platform)?;
    }

    // Parse --with as GlobalSpecs for dependencies
    let with_global_specs: Vec<pixi_global::project::GlobalSpec> = with_specs
        .into_iter()
        .map(|spec| {
            pixi_global::project::GlobalSpec::try_from_matchspec_with_name(spec, &channel_config)
        })
        .collect::<Result<_, _>>()
        .map_err(|e| miette::miette!("invalid --with spec: {e}"))?;

    // Add all dependencies
    for spec in specs.iter().chain(with_global_specs.iter()) {
        project.manifest.add_dependency(env_name, spec)?;
    }

    // Set expose mappings
    if !expose_mappings.is_empty() {
        project.manifest.remove_all_exposed_mappings(env_name)?;
        for mapping in &expose_mappings {
            project.manifest.add_exposed_mapping(env_name, mapping)?;
        }
    }

    // Check if already in sync
    if project
        .environment_in_sync_internal(env_name, true)
        .await?
    {
        tracing::info!(env_name = %env_name, "environment already in sync");
        project.manifest.save().await?;
        return Ok(());
    }

    // Install the environment
    let environment_update = project
        .install_environment_with_options(env_name, pending.force_reinstall)
        .await?;

    // Sync exposed names
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

    // Expose executables
    let _ = project
        .expose_executables_from_environment(env_name)
        .await?;

    // Sync completions
    let _ = project.sync_completions(env_name).await?;

    // Log installed packages
    let requested_names: Vec<_> = specs.iter().map(|s| s.name().clone()).collect();
    let changes = environment_update.user_requested_changes(&requested_names);
    for (name, change) in &changes {
        tracing::info!(package = %name.as_normalized(), change = ?change, "installed");
    }

    project.manifest.save().await?;
    Ok(())
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
        client_home: String,
    ) -> varlink::Result<()> {
        tracing::info!(
            client_home = %client_home,
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
        tracing::debug!(challenge = %challenge, client_home = %client_home, "issuing challenge");

        self.pending
            .lock()
            .expect("pending lock poisoned")
            .insert(
                challenge.clone(),
                PendingInstall {
                    client_home,
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

        let auth_file = Self::auth_file_path(&pending.client_home, &challenge);

        if !auth_file.exists() {
            tracing::warn!(
                challenge = %challenge,
                expected = %auth_file.display(),
                "auth file not found",
            );
            return call.reply_authentication_failed(challenge);
        }

        let env_name_str =
            self.compute_env_name(&pending.client_home, pending.environment.as_deref());
        let env_name = EnvironmentName::from_str(&env_name_str).map_err(|e| {
            varlink::error::Error(
                varlink::ErrorKind::InvalidParameter(e.to_string()),
                None,
                None,
            )
        })?;

        tracing::info!(
            challenge = %challenge,
            env_name = %env_name,
            client_home = %pending.client_home,
            "authentication succeeded, installing",
        );

        match perform_global_install(&env_name, &pending).await {
            Ok(()) => {
                tracing::info!(env_name = %env_name, "global install completed");
                call.reply()
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
