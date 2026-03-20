use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use pixi_consts::consts;
use pixi_core::WorkspaceLocator;
use pixi_manifest::{FeaturesExt, HasFeaturesIter};

use crate::dev_prefix_pixi::{
    Call_ConfirmGlobalInstall, Call_GlobalInstall, Call_Info, EnvironmentInfo, VarlinkInterface,
    WorkspaceInfo,
};

/// Pending challenge: maps UUID to the client_home where the auth file should appear.
struct PendingChallenge {
    client_home: PathBuf,
}

pub struct PixiVarlinkService {
    #[allow(dead_code)]
    nonce: String,
    pending: Mutex<HashMap<String, PendingChallenge>>,
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
                PendingChallenge {
                    client_home: PathBuf::from(&client_home),
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

        let auth_file = Self::auth_file_path(
            pending.client_home.to_str().unwrap_or(""),
            &challenge,
        );

        if auth_file.exists() {
            tracing::info!(
                challenge = %challenge,
                path = %auth_file.display(),
                "authentication succeeded",
            );
            call.reply()
        } else {
            tracing::warn!(
                challenge = %challenge,
                expected = %auth_file.display(),
                "auth file not found",
            );
            call.reply_authentication_failed(challenge)
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
