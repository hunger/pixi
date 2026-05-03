use std::path::{Path, PathBuf};

use clap::Parser;
use fancy_display::FancyDisplay;
use miette::miette;
use pixi_config::{Config, ConfigCli};
use pixi_global::common::check_all_exposed;
use pixi_global::project::ExposedType;
use pixi_global::{EnvRoot, EnvironmentName, LocaliseMode, Project};
use pixi_global::{StateChange, StateChanges};
use rattler_conda_types::PackageName;

use crate::GlobalOptions;
use crate::global::revert_environment_after_error;

/// Updates environments in the global environment.
#[derive(Parser, Debug, Clone)]
pub struct Args {
    /// Specifies the environments that are to be updated.
    environments: Option<Vec<EnvironmentName>>,

    /// Reinstall every package, ignoring the daemon's fingerprint
    /// short-circuit (or the local installer's equivalent). Useful
    /// when a previous run was interrupted or when on-disk state has
    /// drifted from the lockfile in a way the version solver wouldn't
    /// catch.
    #[arg(action, long)]
    force_reinstall: bool,

    /// How a daemon-routed update materialises the prefix at
    /// `~/.pixi/envs/<env>`: `reflink` (default; walk the server
    /// tree and reflink-copy each file, falling back to a plain
    /// byte copy on non-CoW filesystems), `copy` (always
    /// plain-copy; works anywhere), or `symlink` (single symlink
    /// to the server's `<data>/<HASH>/` — cheapest, but the local
    /// prefix breaks if the server's `data/` is GC'd). Ignored
    /// when running without `--socket`. Overrides
    /// `PIXI_GLOBAL_LOCALISE` and `[remote] localise = "..."`.
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

    // Daemon-routing trigger: CLI `--socket` overrides config; either
    // resolves to a socket path means we send `Install` over varlink
    // instead of running the install locally. The local path is
    // unchanged when neither is set.
    let socket: Option<PathBuf> = global_options
        .socket
        .clone()
        .or_else(|| config.remote.socket.clone());

    // Localise-mode resolution: CLI flag > env var > config > default.
    // Only consulted on the daemon path; ignored otherwise.
    let localise_mode =
        super::daemon::resolve_localise_mode(args.localise_mode.as_deref(), &config)?;

    // Update all environments if the user did not specify any
    let env_names = match &args.environments {
        Some(env_names) => env_names.clone(),
        None => {
            // prune old environments and completions
            let state_changes = project_original.prune_old_environments().await?;
            state_changes.report();
            #[cfg(unix)]
            {
                let completions_dir = pixi_global::completions::CompletionsDir::from_env().await?;
                completions_dir.prune_old_completions()?;
            }
            project_original.environments().keys().cloned().collect()
        }
    };

    // Apply changes to each environment, only revert changes if an error occurs
    let mut last_updated_project = project_original;

    for env_name in env_names {
        let mut project = last_updated_project.clone();

        let result = match socket.as_deref() {
            Some(socket) => {
                apply_changes_via_daemon(
                    &env_name,
                    &mut project,
                    socket,
                    localise_mode,
                    args.force_reinstall,
                )
                .await
            }
            None => apply_changes_local(&env_name, &mut project, args.force_reinstall).await,
        };

        match result {
            Ok(state_changes) => state_changes.report(),
            Err(err) => {
                revert_environment_after_error(&env_name, &last_updated_project).await?;
                return Err(err);
            }
        }
        last_updated_project = project;
    }
    last_updated_project.manifest.save().await?;
    Ok(())
}

/// Local-path update: re-solves and re-installs against the
/// in-process compute engine, then re-syncs expose mappings,
/// shortcuts, and completions. Identical to the pre-Phase D
/// behaviour, plus `--force-reinstall` plumbing.
async fn apply_changes_local(
    env_name: &EnvironmentName,
    project: &mut Project,
    force_reinstall: bool,
) -> miette::Result<StateChanges> {
    // If the environment isn't up-to-date our executable detection afterwards
    // will not work
    if !project.environment_in_sync(env_name).await? {
        let _ = project.install_environment(env_name).await?;
    }

    // See what executables were installed prior to update so we can preserve
    // the user's "expose all" vs "expose subset" intent across the re-install.
    let env_binaries = project.executables_of_direct_dependencies(env_name).await?;
    let exposed_mapping_binaries = &project
        .environment(env_name)
        .ok_or_else(|| miette!("Environment {} not found", env_name.fancy_display()))?
        .exposed;
    let expose_type = if check_all_exposed(&env_binaries, exposed_mapping_binaries) {
        ExposedType::All
    } else {
        ExposedType::Nothing
    };

    // Reinstall the environment
    let environment_update = project
        .install_environment_with_options(env_name, force_reinstall)
        .await?;

    let mut state_changes = StateChanges::default();
    state_changes.insert_change(
        env_name,
        StateChange::UpdatedEnvironment(environment_update),
    );

    project.sync_exposed_names(env_name, expose_type).await?;
    state_changes |= project.sync_shortcuts(env_name).await?;
    state_changes |= project
        .expose_executables_from_environment(env_name)
        .await?;
    state_changes |= project.sync_completions(env_name).await?;

    Ok(state_changes)
}

/// Daemon-routed update. Always sends an `Install` RPC against the
/// daemon (the daemon's fingerprint short-circuit handles the "nothing
/// changed" case server-side). Manifest specs are read as-is — update
/// never mutates the manifest's dependency list.
async fn apply_changes_via_daemon(
    env_name: &EnvironmentName,
    project: &mut Project,
    socket: &Path,
    localise_mode: LocaliseMode,
    force_reinstall: bool,
) -> miette::Result<StateChanges> {
    // Capture pre-update expose policy from the manifest's existing
    // exposed list against the on-disk env's binaries — *if* the env
    // is already materialised locally. If it isn't (first daemon
    // update for this env), default to `ExposedType::All` so a fresh
    // env auto-exposes everything, matching local update's behaviour
    // when the env has just been materialised by `install_environment`.
    let env_root = EnvRoot::from_env().await?;
    let local_prefix = env_root.path().join(env_name.as_str());
    let expose_type = if local_prefix.exists() {
        let env_binaries = project.executables_of_direct_dependencies(env_name).await?;
        let exposed_mapping_binaries = &project
            .environment(env_name)
            .ok_or_else(|| miette!("Environment {} not found", env_name.fancy_display()))?
            .exposed;
        if check_all_exposed(&env_binaries, exposed_mapping_binaries) {
            ExposedType::All
        } else {
            ExposedType::Nothing
        }
    } else {
        ExposedType::All
    };

    // Capture the env's direct-dependency names *before* the RPC, so
    // we can fold them into the wire-shipped `EnvironmentUpdate` for
    // user-facing reporting.
    let direct_dependencies: Vec<PackageName> = project
        .environment(env_name)
        .ok_or_else(|| miette!("Environment {} not found", env_name.fancy_display()))?
        .dependencies
        .specs
        .keys()
        .cloned()
        .collect();

    // Pre-build any source-typed deps via the local dispatcher.
    // `manifest_snapshot_for_env` already strips source specs from
    // the wire `specs`; this fills the gap by shipping their
    // built-record JSON in `extra_records` and the runtime deps as
    // synthetic MatchSpec strings on `specs` so the daemon's solve
    // covers their binary closure.
    let source_globals: Vec<pixi_global::project::GlobalSpec> = project
        .environment(env_name)
        .ok_or_else(|| miette!("Environment {} not found", env_name.fancy_display()))?
        .dependencies
        .specs
        .iter()
        .filter(|(_, spec)| spec.is_source())
        .map(|(name, spec)| pixi_global::project::GlobalSpec::new(name.clone(), spec.clone()))
        .collect();
    let (extra_records, source_runtime_deps) =
        super::daemon::build_source_specs_via_local_dispatcher(project, env_name, &source_globals)
            .await?;

    let mut params = super::daemon::manifest_snapshot_for_env(project, env_name, force_reinstall)?;
    params.specs.extend(source_runtime_deps);
    params.extra_records = extra_records;

    let output = super::daemon::install_via_daemon(
        project,
        env_name,
        &params,
        socket,
        localise_mode,
        expose_type,
    )
    .await?;

    let mut state_changes = StateChanges::default();
    let environment_update = super::daemon::wire_transaction_to_environment_update(
        &output.transaction,
        direct_dependencies,
    )?;
    state_changes.insert_change(
        env_name,
        StateChange::UpdatedEnvironment(environment_update),
    );
    state_changes |= output.state_changes;

    // Update doesn't add new shortcut entries to the manifest (the
    // manifest's existing list is authoritative). It does re-sync
    // shortcuts and completions to whatever the manifest says, so a
    // stale shortcut from a removed binary is cleaned up.
    state_changes |= project.sync_shortcuts(env_name).await?;
    state_changes |= project.sync_completions(env_name).await?;

    Ok(state_changes)
}
