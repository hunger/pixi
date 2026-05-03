use std::{
    ops::Not,
    path::{Path, PathBuf},
    str::FromStr,
};

use indexmap::IndexMap;

use clap::Parser;
use fancy_display::FancyDisplay;
use miette::{IntoDiagnostic, Report};
use rattler_conda_types::{MatchSpec, NamedChannelOrUrl, Platform};

use crate::GlobalOptions;
use crate::global::{global_specs::GlobalSpecs, revert_environment_after_error};
use pixi_config::{self, Config, ConfigCli};
use pixi_global::{
    self, EnvChanges, EnvState, EnvironmentName, LocaliseMode, Mapping, Project, StateChange,
    StateChanges,
    common::{NotChangedReason, contains_menuinst_document},
    list::list_all_global_environments,
    project::{ExposedType, GlobalSpec},
};

/// Installs the defined packages in a globally accessible location and exposes their command line applications.
///
/// Example:
///
/// - `pixi global install starship nushell ripgrep bat`
/// - `pixi global install jupyter --with polars`
/// - `pixi global install --expose python3.8=python python=3.8`
/// - `pixi global install --environment science --expose jupyter --expose ipython jupyter ipython polars`
#[derive(Parser, Debug, Clone, Default)]
#[clap(arg_required_else_help = true, verbatim_doc_comment)]
pub struct Args {
    /// Specifies the package that should be installed.
    #[clap(flatten)]
    pub packages: GlobalSpecs,

    /// The channels to consider as a name or a url.
    /// Multiple channels can be specified by using this field multiple times.
    ///
    /// When specifying a channel, it is common that the selected channel also
    /// depends on the `conda-forge` channel.
    ///
    /// By default, if no channel is provided, `conda-forge` is used.
    #[clap(long = "channel", short = 'c', value_name = "CHANNEL")]
    channels: Vec<NamedChannelOrUrl>,

    /// The platform to install the packages for.
    ///
    /// This is useful when you want to install packages for a different platform than the one you are currently on.
    /// This is very often used when you want to install `osx-64` packages on `osx-arm64`.
    #[clap(short, long)]
    platform: Option<Platform>,

    /// Ensures that all packages will be installed in the same environment
    #[clap(short, long)]
    environment: Option<EnvironmentName>,

    /// Add one or more mapping which describe which executables are exposed.
    /// The syntax is `exposed_name=executable_name`, so for example `python3.10=python`.
    /// Alternatively, you can input only an executable_name and `executable_name=executable_name` is assumed.
    #[arg(long)]
    expose: Vec<Mapping>,

    /// Add additional dependencies to the environment.
    /// Their executables will not be exposed.
    #[arg(long)]
    with: Vec<MatchSpec>,

    #[clap(flatten)]
    config: ConfigCli,

    /// Specifies that the environment should be reinstalled.
    #[arg(action, long)]
    pub force_reinstall: bool,

    /// Specifies that no shortcuts should be created for the installed packages.
    #[arg(action, long, alias = "no-shortcut")]
    no_shortcuts: bool,

    /// How a daemon-routed install materialises the prefix at
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

    /// Optional backend override (primarily for testing, not exposed in CLI)
    #[clap(skip)]
    pub backend_override: Option<pixi_build_frontend::BackendOverride>,
}

pub async fn execute(args: Args, global_options: &GlobalOptions) -> miette::Result<()> {
    let config = Config::with_cli_config(&args.config);

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

    // Load the global config and ensure
    // that the root_dir is relative to the manifest directory
    let mut project_original = pixi_global::Project::discover_or_create()
        .await?
        .with_cli_config(config.clone());

    // Apply backend override if provided (primarily for testing)
    if let Some(backend_override) = args.backend_override.clone() {
        project_original = project_original.with_backend_override(backend_override);
    }
    let channel_config = project_original.global_channel_config().clone();

    let specs = args
        .packages
        .to_global_specs(&channel_config, &project_original.root, &project_original)
        .await?;
    let env_to_specs: IndexMap<EnvironmentName, Vec<GlobalSpec>> = match &args.environment {
        Some(env_name) => IndexMap::from([(env_name.clone(), specs)]),
        None => specs
            .into_iter()
            .map(|spec| {
                (
                    EnvironmentName::from_str(spec.name().as_normalized())
                        .expect("valid environment name"),
                    vec![spec],
                )
            })
            .collect(),
    };

    if !args.expose.is_empty() && env_to_specs.len() != 1 {
        miette::bail!("Can't add exposed mappings with `--exposed` for more than one environment");
    }

    if !args.with.is_empty() && env_to_specs.len() != 1 {
        miette::bail!("Can't add packages with `--with` for more than one environment");
    }

    let mut env_changes = EnvChanges::default();
    let mut last_updated_project = project_original;
    let mut errors: Vec<(EnvironmentName, Report)> = Vec::new();
    // Convert the packages into named global specs

    for (env_name, specs) in &env_to_specs {
        let mut project = last_updated_project.clone();
        let install_result = match socket.as_deref() {
            Some(socket) => {
                setup_environment_via_daemon(
                    env_name,
                    &args,
                    specs,
                    &mut project,
                    socket,
                    localise_mode,
                )
                .await
            }
            None => setup_environment(env_name, &args, specs, &mut project).await,
        };
        match install_result {
            Ok(state_changes) => {
                if state_changes.has_changed() {
                    env_changes
                        .changes
                        .insert(env_name.clone(), EnvState::Installed)
                } else {
                    env_changes.changes.insert(
                        env_name.clone(),
                        EnvState::NotChanged(NotChangedReason::AlreadyInstalled),
                    )
                };
                // Only advance project on success
                last_updated_project = project;
            }
            Err(err) => {
                if let Err(revert_err) =
                    revert_environment_after_error(env_name, &last_updated_project).await
                {
                    tracing::warn!("Reverting of the operation failed");
                    tracing::info!("Reversion error: {:?}", revert_err);
                }
                errors.push((env_name.clone(), err));
            }
        }
    }

    // After installing, we always want to list the changed environments
    list_all_global_environments(
        &last_updated_project,
        Some(env_to_specs.into_keys().collect()),
        Some(&env_changes),
        None,
        false,
    )
    .await?;

    if errors.is_empty() {
        Ok(())
    } else {
        for (env_name, err) in errors {
            tracing::warn!("Couldn't install {}\n{err:?}", env_name.fancy_display());
        }
        Err(miette::miette!("Some environments couldn't be installed."))
    }
}

/// Output of [`prepare_install_manifest`]: the manifest prologue's
/// side effects that both the local and the daemon-routed install
/// paths need afterwards.
struct PreparedInstall {
    /// Accumulated state changes (force-reinstall removal,
    /// added-environment marker).
    state_changes: StateChanges,
    /// Packages to install: `args.packages` plus the
    /// `--with` inclusions converted to [`GlobalSpec`]s. Both paths
    /// derive their downstream artefacts (manifest entries,
    /// match-spec strings, requested-package names) from this.
    packages_to_add: Vec<GlobalSpec>,
    /// Channels resolved from CLI arg or project default. The
    /// daemon path forwards this verbatim on the wire; the local
    /// path doesn't read it after the prologue.
    channels: Vec<NamedChannelOrUrl>,
}

/// Mutate `project.manifest` to reflect a fresh install: honour
/// `--force-reinstall`, register the environment with its channels,
/// set the platform if requested, add every dependency (including
/// `--with` inclusions), and replace expose mappings with the
/// explicit `args.expose` list. Identical for the local and
/// daemon-routed install paths — this helper is the shared prologue
/// that runs before each path's distinct "actually install" tail.
async fn prepare_install_manifest(
    env_name: &EnvironmentName,
    args: &Args,
    specs: &[GlobalSpec],
    project: &mut Project,
) -> miette::Result<PreparedInstall> {
    let mut state_changes = StateChanges::new_with_env(env_name.clone());

    if args.force_reinstall && project.environment(env_name).is_some() {
        state_changes |= project.remove_environment(env_name).await?;
    }

    let channels = if args.channels.is_empty() {
        project.config().default_channels()
    } else {
        args.channels.clone()
    };

    if !project.manifest.parsed.envs.contains_key(env_name) {
        project
            .manifest
            .add_environment(env_name, Some(channels.clone()))?;
        state_changes.insert_change(env_name, StateChange::AddedEnvironment);
    }

    if let Some(platform) = args.platform {
        project.manifest.set_platform(env_name, platform)?;
    }

    let converted_with_inclusions = args
        .with
        .iter()
        .map(|spec| {
            GlobalSpec::try_from_matchspec_with_name(
                spec.clone(),
                project.config().global_channel_config(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;

    let packages_to_add: Vec<GlobalSpec> = specs
        .iter()
        .cloned()
        .chain(converted_with_inclusions)
        .collect();

    for spec in &packages_to_add {
        project.manifest.add_dependency(env_name, spec)?;
    }

    if !args.expose.is_empty() {
        project.manifest.remove_all_exposed_mappings(env_name)?;
        for mapping in &args.expose {
            project.manifest.add_exposed_mapping(env_name, mapping)?;
        }
    }

    Ok(PreparedInstall {
        state_changes,
        packages_to_add,
        channels,
    })
}

async fn setup_environment(
    env_name: &EnvironmentName,
    args: &Args,
    specs: &[GlobalSpec],
    project: &mut Project,
) -> miette::Result<StateChanges> {
    let PreparedInstall {
        mut state_changes,
        packages_to_add,
        channels: _,
    } = prepare_install_manifest(env_name, args, specs, project).await?;

    if project.environment_in_sync_internal(env_name, true).await? {
        return Ok(StateChanges::new_with_env(env_name.clone()));
    }

    // Installing the environment to be able to find the bin paths later
    let environment_update = project
        .install_environment_with_options(env_name, args.force_reinstall)
        .await?;

    // Sync exposed name
    let expose_type = expose_type_for_install_args(args)?;
    project.sync_exposed_names(env_name, expose_type).await?;

    // Add shortcuts
    if !args.no_shortcuts {
        let prefix = project.environment_prefix(env_name).await?;
        for spec in specs.iter() {
            let prefix_record = prefix.find_designated_package(spec.name()).await?;
            if contains_menuinst_document(&prefix_record, prefix.root()) {
                project.manifest.add_shortcut(env_name, spec.name())?;
            }
        }
        state_changes |= project.sync_shortcuts(env_name).await?;
    }

    // Figure out added packages and their corresponding versions
    let requested_package_names: Vec<_> = packages_to_add
        .iter()
        .map(|spec| spec.name().clone())
        .collect();
    let user_requested_changes =
        environment_update.user_requested_changes(&requested_package_names);

    // Convert to StateChange::AddedPackage for packages that were installed or upgraded
    state_changes
        .add_packages_from_install_changes(env_name, user_requested_changes, project)
        .await?;

    // Expose executables of the new environment
    state_changes |= project
        .expose_executables_from_environment(env_name)
        .await?;

    // Sync completions
    state_changes |= project.sync_completions(env_name).await?;

    project.manifest.save().await?;
    Ok(state_changes)
}

/// Daemon-routed counterpart to [`setup_environment`].
///
/// Shape matches the local path: prepare the manifest prologue,
/// run the install, localise the prefix, sync the expose mappings,
/// write trampolines, finalise (shortcuts + completions + save).
/// The only difference is *who* runs the install — the daemon at
/// `socket` instead of an in-process dispatcher. The shared body
/// (wire RPC + reporter pump + localise + expose) lives in
/// [`super::daemon::install_via_daemon`]; this function does the
/// install-specific prologue + post-install per-package state-change
/// rendering by walking the daemon's wire-shipped `TransactionSummary`
/// the same way the local install path walks its dispatcher-emitted
/// `EnvironmentUpdate`.
async fn setup_environment_via_daemon(
    env_name: &EnvironmentName,
    args: &Args,
    specs: &[GlobalSpec],
    project: &mut Project,
    socket: &Path,
    localise_mode: LocaliseMode,
) -> miette::Result<StateChanges> {
    let PreparedInstall {
        mut state_changes,
        packages_to_add,
        channels,
    } = prepare_install_manifest(env_name, args, specs, project).await?;

    // Build the wire request: resolve channel URLs via the project's
    // channel config and render specs as match-spec strings.
    let channel_config = project.config().global_channel_config().clone();
    let channel_urls: Vec<String> = channels
        .iter()
        .filter_map(|c| c.clone().into_base_url(&channel_config).ok())
        .map(|url| url.to_string())
        .collect();
    // Source-typed packages (path/url/git) can't be solved or fetched
    // by the daemon directly. Build them via the local dispatcher
    // instead: the daemon receives the resulting `.conda` artefacts
    // as `ExtraRecord`s and extracts them into its package cache,
    // and the runtime deps from each built record's
    // `package_record.depends` get folded into the wire `specs` so
    // the daemon's solve covers their binary closure.
    let (extra_records, source_runtime_deps) =
        super::daemon::build_source_specs_via_local_dispatcher(project, env_name, &packages_to_add)
            .await?;
    let mut spec_strings: Vec<String> = packages_to_add
        .iter()
        .filter(|spec| !spec.spec().is_source())
        .map(|spec| {
            spec.spec()
                .clone()
                .to_match_spec(spec.name(), &channel_config)
                .map(|ms| ms.to_string())
                .into_diagnostic()
        })
        .collect::<miette::Result<Vec<_>>>()?;
    spec_strings.extend(source_runtime_deps);
    let params = super::daemon::DaemonRequestParams {
        specs: spec_strings,
        channels: channel_urls,
        platform: args.platform,
        force_reinstall: args.force_reinstall,
        extra_records,
    };

    let expose_type = expose_type_for_install_args(args)?;

    let output = super::daemon::install_via_daemon(
        project,
        env_name,
        &params,
        socket,
        localise_mode,
        expose_type,
    )
    .await?;
    state_changes |= output.state_changes;

    // Replay the daemon's transaction summary as per-package
    // `AddedPackage` state changes — the same shape the local install
    // path produces from its dispatcher-emitted `EnvironmentUpdate`.
    // Filtered to the user-requested package names (specs +
    // `--with`), so transitive deps stay quiet (matching local).
    let direct_dependencies: Vec<rattler_conda_types::PackageName> = packages_to_add
        .iter()
        .map(|spec| spec.name().clone())
        .collect();
    let environment_update = super::daemon::wire_transaction_to_environment_update(
        &output.transaction,
        direct_dependencies.clone(),
    )?;
    let user_requested = environment_update.user_requested_changes(&direct_dependencies);
    state_changes
        .add_packages_from_install_changes(env_name, user_requested, project)
        .await?;

    state_changes |= project
        .finalise_environment_no_trampolines(env_name, !args.no_shortcuts, specs)
        .await?;

    Ok(state_changes)
}

/// Compute the [`ExposedType`] from `pixi global install` arguments.
/// Mirrors the original [`sync_exposed_names`] helper, but as a pure
/// derivation step the daemon helper can pass to `Project::sync_exposed_names`.
fn expose_type_for_install_args(args: &Args) -> miette::Result<ExposedType> {
    let with_package_names = args
        .with
        .iter()
        .map(|spec| {
            spec.name.as_exact().cloned().ok_or_else(|| {
                miette::miette!("could not find exact package name in MatchSpec {}", spec)
            })
        })
        .collect::<miette::Result<Vec<_>>>()?;
    Ok(if args.expose.is_empty().not() {
        ExposedType::Mappings(args.expose.clone())
    } else if with_package_names.is_empty() {
        ExposedType::All
    } else {
        ExposedType::Ignore(with_package_names)
    })
}
