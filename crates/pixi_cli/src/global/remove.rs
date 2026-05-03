use std::path::{Path, PathBuf};
use std::str::FromStr;

use clap::Parser;
use itertools::Itertools;
use miette::Context;
use pixi_config::{Config, ConfigCli};
use pixi_global::project::ExposedType;
use pixi_global::{EnvironmentName, ExposedName, LocaliseMode, Project, StateChange, StateChanges};
use rattler_conda_types::{MatchSpec, PackageName};

use crate::GlobalOptions;
use crate::global::revert_environment_after_error;
use crate::has_specs::HasSpecs;

/// Removes dependencies from an environment
///
/// Use `pixi global uninstall` to remove the whole environment
///
/// Example: `pixi global remove --environment python numpy`
#[derive(Parser, Debug)]
#[clap(arg_required_else_help = true, verbatim_doc_comment)]
pub struct Args {
    /// Specifies the package that should be removed.
    #[arg(num_args = 1.., required = true, value_name = "PACKAGE")]
    packages: Vec<String>,

    /// Specifies the environment that the dependencies need to be removed from.
    #[clap(short, long)]
    environment: Option<EnvironmentName>,

    /// How a daemon-routed remove materialises the prefix at
    /// `~/.pixi/envs/<env>`. See `pixi global install --help` for
    /// the mode list. Ignored when running without `--socket`.
    #[arg(long, value_name = "MODE")]
    localise_mode: Option<String>,

    #[clap(flatten)]
    config: ConfigCli,
}

impl HasSpecs for Args {
    fn packages(&self) -> Vec<&str> {
        self.packages.iter().map(AsRef::as_ref).collect()
    }
}

pub async fn execute(args: Args, global_options: &GlobalOptions) -> miette::Result<()> {
    let Some(env_name) = &args.environment else {
        miette::bail!(
            "`--environment` is required. Try `pixi global uninstall {}` if you want to delete whole environments",
            args.packages.join(" ")
        );
    };
    let config = Config::with_cli_config(&args.config);
    let project_original = Project::discover_or_create()
        .await?
        .with_cli_config(config.clone());

    if project_original.environment(env_name).is_none() {
        miette::bail!(
            "Environment {} doesn't exist. You can create a new environment with `pixi global install`.",
            env_name
        );
    }

    let socket: Option<PathBuf> = global_options
        .socket
        .clone()
        .or_else(|| config.remote.socket.clone());
    let localise_mode =
        super::daemon::resolve_localise_mode(args.localise_mode.as_deref(), &config)?;

    let mut project = project_original.clone();
    let specs = args
        .specs()?
        .into_iter()
        .map(|(_, specs)| specs)
        .collect_vec();

    let result = match socket.as_deref() {
        Some(socket) => {
            apply_changes_via_daemon(
                env_name,
                specs.as_slice(),
                &mut project,
                socket,
                localise_mode,
            )
            .await
        }
        None => apply_changes_local(env_name, specs.as_slice(), &mut project).await,
    };

    match result.wrap_err(format!("Couldn't remove packages from {env_name}")) {
        Ok(state_changes) => {
            state_changes.report();
        }
        Err(err) => {
            if let Err(revert_err) =
                revert_environment_after_error(env_name, &project_original).await
            {
                tracing::warn!("Reverting of the operation failed");
                tracing::info!("Reversion error: {:?}", revert_err);
            }
            return Err(err);
        }
    }
    Ok(())
}

/// Local-path remove: mutate manifest, then defer to
/// [`Project::sync_environment`] (which solves+installs locally
/// and folds removed package names into the
/// [`pixi_global::common::EnvironmentUpdate`] for reporting).
async fn apply_changes_local(
    env_name: &EnvironmentName,
    specs: &[MatchSpec],
    project: &mut Project,
) -> miette::Result<StateChanges> {
    let removed_dependencies = strip_specs_from_manifest(env_name, specs, project).await?;
    let state_changes = project
        .sync_environment(env_name, Some(removed_dependencies))
        .await?;
    project.manifest.save().await?;
    Ok(state_changes)
}

/// Daemon-routed remove: mutate manifest, then drive the install
/// through the shared [`super::daemon::run_install_for_env`]
/// helper. Removed package names are passed as
/// `extra_current_packages` so the resulting
/// [`pixi_global::common::EnvironmentUpdate`]'s
/// `current_packages` set still contains them — keeps the
/// formatter classifying them as top-level changes, matching the
/// local path's
/// [`pixi_global::common::EnvironmentUpdate::add_removed_packages`].
async fn apply_changes_via_daemon(
    env_name: &EnvironmentName,
    specs: &[MatchSpec],
    project: &mut Project,
    socket: &Path,
    localise_mode: LocaliseMode,
) -> miette::Result<StateChanges> {
    let removed_dependencies = strip_specs_from_manifest(env_name, specs, project).await?;

    // `remove` already pruned the matching mappings via
    // `remove_exposed_name` above; `ExposedType::Nothing` tells the
    // helper not to second-guess the manifest's `exposed` list.
    let (helper_changes, environment_update) = super::daemon::run_install_for_env(
        project,
        env_name,
        socket,
        localise_mode,
        false,
        ExposedType::Nothing,
        removed_dependencies,
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
    project.manifest.save().await?;
    Ok(state_changes)
}

/// Drop the requested specs from the manifest's dependency list and
/// the matching exposed-binary mappings, returning the set of names
/// that were actually removed (for reporting).
async fn strip_specs_from_manifest(
    env_name: &EnvironmentName,
    specs: &[MatchSpec],
    project: &mut Project,
) -> miette::Result<Vec<PackageName>> {
    let mut removed_dependencies = Vec::new();
    for spec in specs {
        let package_name = spec.name.as_exact().expect("package name must be exact");
        project
            .manifest
            .remove_dependency(env_name, package_name)
            .map(|removed_name| removed_dependencies.push(removed_name))?;
    }

    let prefix = project.environment_prefix(env_name).await?;
    for spec in specs {
        let name = spec.name.as_exact().expect("package name must be exact");
        if let Ok(record) = prefix.find_designated_package(name).await {
            prefix
                .find_executables(&[record])
                .into_iter()
                .filter_map(|executable| ExposedName::from_str(executable.name.as_str()).ok())
                .for_each(|exposed_name| {
                    project
                        .manifest
                        .remove_exposed_name(env_name, &exposed_name)
                        .ok();
                });
        }
    }

    Ok(removed_dependencies)
}
