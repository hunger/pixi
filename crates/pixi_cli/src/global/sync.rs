use std::path::{Path, PathBuf};

use clap::Parser;
use fancy_display::FancyDisplay;
use miette::miette;
use pixi_config::{Config, ConfigCli};
use pixi_global::{EnvironmentName, LocaliseMode, Project, StateChange, StateChanges};

use crate::GlobalOptions;

/// Sync global manifest with installed environments
#[derive(Parser, Debug)]
pub struct Args {
    /// How a daemon-routed sync materialises each env's prefix at
    /// `~/.pixi/envs/<env>`. See `pixi global install --help` for
    /// the mode list. Ignored when running without `--socket`.
    #[arg(long, value_name = "MODE")]
    localise_mode: Option<String>,

    #[clap(flatten)]
    config: ConfigCli,
}

pub async fn execute(args: Args, global_options: &GlobalOptions) -> miette::Result<()> {
    let config = Config::with_cli_config(&args.config);
    let project_original = pixi_global::Project::discover_or_create()
        .await?
        .with_cli_config(config.clone());

    // Daemon-routing trigger: `--socket` (CLI) overrides
    // `[remote] socket = "..."` (config).
    let socket: Option<PathBuf> = global_options
        .socket
        .clone()
        .or_else(|| config.remote.socket.clone());
    let localise_mode =
        super::daemon::resolve_localise_mode(args.localise_mode.as_deref(), &config)?;

    let mut has_changed = false;

    // Prune environments that aren't listed in the manifest. The
    // local cleanup removes `~/.pixi/envs/<env>` and the matching
    // bin entries; for daemon-routed sync we then send `Uninstall`
    // for each pruned env so the daemon's `<data>/<HASH>/` is freed
    // too.
    let prune_state = project_original.prune_old_environments().await?;
    if let Some(socket) = socket.as_deref() {
        for pruned in pruned_envs(&prune_state) {
            if let Err(err) = super::daemon::uninstall_via_daemon(socket, &pruned).await {
                tracing::warn!(
                    "Local prune of {} succeeded but the daemon-side prefix could not be \
                     removed: {err:?}",
                    pruned.fancy_display()
                );
            }
        }
    }

    #[cfg(unix)]
    {
        let completions_dir = pixi_global::completions::CompletionsDir::from_env().await?;
        completions_dir.prune_old_completions()?;
    }

    if prune_state.has_changed() {
        has_changed = true;
        prune_state.report();
    }

    if let Err(err) = project_original.remove_broken_files().await {
        tracing::warn!("Couldn't remove broken files\n{err:?}")
    }

    let mut errors: Vec<(EnvironmentName, miette::Report)> = Vec::new();
    let env_names: Vec<EnvironmentName> = project_original.environments().keys().cloned().collect();
    let mut last_updated_project = project_original;
    for env_name in env_names {
        let mut project = last_updated_project.clone();
        let result = match socket.as_deref() {
            Some(socket) => {
                sync_env_via_daemon(&env_name, &mut project, socket, localise_mode).await
            }
            None => project.sync_environment(&env_name, None).await,
        };
        match result {
            Ok(state_change) => {
                if state_change.has_changed() {
                    has_changed = true;
                    state_change.report();
                }
                last_updated_project = project;
            }
            Err(err) => errors.push((env_name, err)),
        }
    }

    if !has_changed {
        eprintln!(
            "{}Nothing to do. The pixi global installation is already up-to-date.",
            console::style(console::Emoji("✔ ", "")).green()
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        for (env_name, err) in errors {
            tracing::warn!(
                "Couldn't sync environment {}\n{err:?}",
                env_name.fancy_display(),
            );
        }
        Err(miette!("Some environments couldn't be synced."))
    }
}

/// Walk the [`StateChanges`] returned by
/// [`Project::prune_old_environments`] and pull out the env names
/// that landed a `RemovedEnvironment` entry. These are the envs the
/// daemon-side `Uninstall` RPC needs to free.
fn pruned_envs(state: &StateChanges) -> Vec<EnvironmentName> {
    state
        .iter()
        .filter(|(_, changes)| {
            changes
                .iter()
                .any(|c| matches!(c, StateChange::RemovedEnvironment))
        })
        .map(|(env_name, _)| env_name.clone())
        .collect()
}

/// Daemon-routed per-env sync. Drives the install through the
/// shared [`super::daemon::run_install_for_env`] helper with the
/// pre-update expose policy preserved (auto-expose-all stays
/// auto-expose-all; a manually-curated subset stays exactly that),
/// then `sync_shortcuts` / `sync_completions` to round out what
/// the local [`Project::sync_environment`] would do.
async fn sync_env_via_daemon(
    env_name: &EnvironmentName,
    project: &mut Project,
    socket: &Path,
    localise_mode: LocaliseMode,
) -> miette::Result<StateChanges> {
    let expose_type = super::daemon::detect_existing_expose_policy(project, env_name).await?;
    let (helper_changes, environment_update) = super::daemon::run_install_for_env(
        project,
        env_name,
        socket,
        localise_mode,
        false,
        expose_type,
        Vec::new(),
    )
    .await?;

    let mut state_changes = StateChanges::default();
    state_changes.insert_change(
        env_name,
        StateChange::UpdatedEnvironment(environment_update),
    );
    state_changes |= helper_changes;
    state_changes |= project.sync_shortcuts(env_name).await?;
    state_changes |= project.sync_completions(env_name).await?;
    Ok(state_changes)
}
