use std::path::{Path, PathBuf};

use clap::Parser;
use pixi_config::{Config, ConfigCli};
use pixi_global::project::{ExposedType, GlobalSpec};
use pixi_global::{EnvironmentName, LocaliseMode, Mapping, Project, StateChange, StateChanges};

use crate::GlobalOptions;
use crate::global::global_specs::GlobalSpecs;
use crate::global::revert_environment_after_error;

/// Adds dependencies to an environment
///
/// Example:
///
/// - `pixi global add --environment python numpy`
/// - `pixi global add --environment my_env pytest pytest-cov --expose pytest=pytest`
#[derive(Parser, Debug, Clone)]
#[clap(arg_required_else_help = true, verbatim_doc_comment)]
pub struct Args {
    /// Specifies the package that should be added to the environment.
    #[clap(flatten)]
    packages: GlobalSpecs,

    /// Specifies the environment that the dependencies need to be added to.
    #[clap(short, long, required = true)]
    environment: EnvironmentName,

    /// Add one or more mapping which describe which executables are exposed.
    /// The syntax is `exposed_name=executable_name`, so for example `python3.10=python`.
    /// Alternatively, you can input only an executable_name and `executable_name=executable_name` is assumed.
    #[arg(long)]
    expose: Vec<Mapping>,

    /// How a daemon-routed add materialises the prefix at
    /// `~/.pixi/envs/<env>`. See `pixi global install --help` for
    /// the mode list. Ignored when running without `--socket`.
    #[arg(long, value_name = "MODE")]
    localise_mode: Option<String>,

    #[clap(flatten)]
    config: ConfigCli,
}

pub async fn execute(args: Args, global_options: &GlobalOptions) -> miette::Result<()> {
    let config = Config::with_cli_config(&args.config);
    let project_original = Project::discover_or_create()
        .await?
        .with_cli_config(config.clone());

    if project_original.environment(&args.environment).is_none() {
        miette::bail!(
            "Environment {} doesn't exist. You can create a new environment with `pixi global install`.",
            &args.environment
        );
    }

    let socket: Option<PathBuf> = global_options
        .socket
        .clone()
        .or_else(|| config.remote.socket.clone());
    let localise_mode =
        super::daemon::resolve_localise_mode(args.localise_mode.as_deref(), &config)?;

    let specs = args
        .packages
        .to_global_specs(
            project_original.global_channel_config(),
            &project_original.root,
            &project_original,
        )
        .await?;

    let mut project_modified = project_original.clone();

    let result = match socket.as_deref() {
        Some(socket) => {
            apply_changes_via_daemon(
                &args.environment,
                &specs,
                args.expose.as_slice(),
                &mut project_modified,
                socket,
                localise_mode,
            )
            .await
        }
        None => {
            apply_changes_local(
                &args.environment,
                &specs,
                args.expose.as_slice(),
                &mut project_modified,
            )
            .await
        }
    };

    match result {
        Ok(state_changes) => {
            state_changes.report();
            Ok(())
        }
        Err(err) => {
            if let Err(revert_err) =
                revert_environment_after_error(&args.environment, &project_original).await
            {
                tracing::warn!("Reverting of the operation failed");
                tracing::info!("Reversion error: {:?}", revert_err);
            }
            Err(err)
        }
    }
}

/// Local-path add: mutate manifest, then defer to
/// [`Project::sync_environment`] which solves+installs locally and
/// runs the expose/shortcuts/completions tail.
async fn apply_changes_local(
    env_name: &EnvironmentName,
    specs: &[GlobalSpec],
    expose: &[Mapping],
    project: &mut Project,
) -> miette::Result<StateChanges> {
    let mut state_changes = StateChanges::new_with_env(env_name.clone());

    for spec in specs {
        project.manifest.add_dependency(env_name, spec)?;
    }
    for mapping in expose {
        project.manifest.add_exposed_mapping(env_name, mapping)?;
    }

    let sync_changes = project.sync_environment(env_name, None).await?;

    let requested_package_names: Vec<_> = specs.iter().map(|spec| spec.name().clone()).collect();
    if let Some(changes_for_env) = sync_changes.changes_for_env(env_name) {
        for change in changes_for_env {
            if let StateChange::UpdatedEnvironment(environment_update) = change {
                let user_requested_changes =
                    environment_update.user_requested_changes(&requested_package_names);
                state_changes
                    .add_packages_from_install_changes(env_name, user_requested_changes, project)
                    .await?;
                break;
            }
        }
    }

    state_changes |= sync_changes;
    state_changes |= project.sync_completions(env_name).await?;
    project.manifest.save().await?;
    Ok(state_changes)
}

/// Daemon-routed add: mutate manifest, then drive the install
/// through the shared [`super::daemon::run_install_for_env`]
/// helper. The user-facing report mirrors the local path —
/// `UpdatedEnvironment` plus per-package `AddedPackage` entries
/// for the names the user passed on the CLI.
async fn apply_changes_via_daemon(
    env_name: &EnvironmentName,
    specs: &[GlobalSpec],
    expose: &[Mapping],
    project: &mut Project,
    socket: &Path,
    localise_mode: LocaliseMode,
) -> miette::Result<StateChanges> {
    for spec in specs {
        project.manifest.add_dependency(env_name, spec)?;
    }
    for mapping in expose {
        project.manifest.add_exposed_mapping(env_name, mapping)?;
    }

    // `add` doesn't run `sync_exposed_names` against an
    // [`ExposedType`]: the manifest's `exposed` list already reflects
    // the user's intent (the entries we just `add_exposed_mapping`'d
    // plus whatever was there before). [`ExposedType::Nothing`] tells
    // the helper "don't touch the manifest's exposed list" — the
    // subsequent `expose_executables_from_environment` walk picks up
    // the entries verbatim.
    let (helper_changes, environment_update) = super::daemon::run_install_for_env(
        project,
        env_name,
        socket,
        localise_mode,
        false,
        ExposedType::Nothing,
        Vec::new(),
    )
    .await?;

    let mut state_changes = StateChanges::default();
    let requested_package_names: Vec<_> = specs.iter().map(|spec| spec.name().clone()).collect();
    let user_requested_changes =
        environment_update.user_requested_changes(&requested_package_names);
    state_changes.insert_change(
        env_name,
        StateChange::UpdatedEnvironment(environment_update),
    );
    state_changes
        .add_packages_from_install_changes(env_name, user_requested_changes, project)
        .await?;
    state_changes |= helper_changes;

    // sync_environment (local path) does these inside; daemon path
    // calls them explicitly because the helper stops after expose.
    state_changes |= project.sync_shortcuts(env_name).await?;
    state_changes |= project.sync_completions(env_name).await?;
    project.manifest.save().await?;
    Ok(state_changes)
}
