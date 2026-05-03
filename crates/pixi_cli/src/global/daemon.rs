//! Shared daemon-routed install flow used by every env-mutating
//! `pixi global` subcommand: `install`, `update`, `add`, `remove`.
//! (Uninstall has its own much smaller path — see
//! `crates/pixi_cli/src/global/uninstall.rs`.)
//!
//! All four subcommands send the same `Install` RPC; what differs
//! is the prologue (install / add / remove mutate the manifest;
//! update reads it as-is) and the post-install reporting (install
//! and add emit per-package `AddedPackage` state changes; update
//! and remove emit a single `UpdatedEnvironment` carrying a rich
//! `EnvironmentUpdate`). This module owns the shared body — wire
//! request, stream pump, localisation, expose-mapping sync — and
//! exposes helpers for the call sites to render the wire
//! transaction summary into the domain-shaped types each path
//! needs.
//!
//! Nothing here mutates the manifest or persists it; the per-call
//! site decides when (and whether) to call `manifest.save()`.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use futures::StreamExt;
use miette::{IntoDiagnostic, miette};
use pixi_config::Config;
use pixi_global::{
    EnvRoot, EnvironmentName, LocaliseMode, Project, StateChanges,
    common::{EnvironmentUpdate, InstallChange},
    project::{ExposedType, GlobalSpec},
};
use pixi_varlink::{
    ExtraRecord, InstallChangeWire, InstallReply, InstallRequest, ReporterClient,
    TransactionSummary,
};
use rattler_conda_types::{PackageName, Platform, Version};

use super::wire_reporter_client::WireReporterClient;

/// Build any source-typed `GlobalSpec`s in `packages` locally and
/// shape them for the wire: produce the
/// [`pixi_varlink::ExtraRecord`]s the daemon will splice into its
/// install transaction, plus the runtime-dep MatchSpec strings the
/// caller should append to `InstallRequest::specs` so the daemon's
/// solve covers the source build's binary closure.
///
/// Source specs are pre-built on the client because the daemon has
/// no view of the client's filesystem (path sources) and doesn't
/// run the client's `BackendOverride` (url/git source builds).
/// Building locally hands those constraints to the dispatcher the
/// user already configured for local installs.
///
/// Returns `(extra_records, runtime_dep_strings)`. Both are empty
/// when `packages` contains no source specs — callers can call
/// this unconditionally.
pub(crate) async fn build_source_specs_via_local_dispatcher(
    project: &Project,
    env_name: &EnvironmentName,
    packages: &[GlobalSpec],
) -> miette::Result<(Vec<ExtraRecord>, Vec<String>)> {
    let source_specs: Vec<GlobalSpec> = packages
        .iter()
        .filter(|spec| spec.spec().is_source())
        .cloned()
        .collect();
    if source_specs.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let built = project
        .build_source_specs_for_daemon(env_name, &source_specs)
        .await?;

    let mut extras = Vec::with_capacity(built.len());
    let mut runtime_deps = Vec::new();
    for (record, artifact_path) in built {
        let path_str = artifact_path
            .to_str()
            .ok_or_else(|| {
                miette!(
                    "non-utf8 source-build artifact path: {}",
                    artifact_path.display()
                )
            })?
            .to_string();
        // Synthesise the binary closure the daemon's solve needs to
        // resolve so the source-built record's runtime deps end up
        // in the install. These come straight off the built
        // record's `package_record.depends` (already MatchSpec
        // strings).
        runtime_deps.extend(record.package_record.depends.iter().cloned());
        let record_json = serde_json::to_string(&record).into_diagnostic()?;
        extras.push(ExtraRecord {
            artifact_path: path_str,
            record_json,
        });
    }
    Ok((extras, runtime_deps))
}

/// Pick a [`LocaliseMode`] for a daemon-routed install/update from
/// (in priority order) the `--localise-mode` CLI flag, the
/// `PIXI_GLOBAL_LOCALISE` env var, the `[remote] localise = "..."`
/// config key, falling back to [`LocaliseMode::default`] (`Reflink`)
/// when none is set. Each layer takes a string, parsed via the mode's
/// `FromStr`.
pub(crate) fn resolve_localise_mode(
    cli: Option<&str>,
    config: &Config,
) -> miette::Result<LocaliseMode> {
    let raw = cli
        .map(str::to_owned)
        .or_else(|| std::env::var("PIXI_GLOBAL_LOCALISE").ok())
        .or_else(|| config.remote.localise.clone());
    match raw {
        Some(s) => s.parse().map_err(|e: String| miette!("{e}")),
        None => Ok(LocaliseMode::default()),
    }
}

/// Wire-shaped inputs for one daemon-routed install RPC.
///
/// `install` builds this from the user's CLI args plus
/// `prepare_install_manifest`'s output; `update` builds it from the
/// existing manifest's entry for the env. Either way, by the time
/// the params reach [`install_via_daemon`] the manifest already
/// reflects whatever mutations the caller wanted to make — the
/// helper does not re-read the manifest.
#[derive(Debug, Clone, Default)]
pub(crate) struct DaemonRequestParams {
    /// MatchSpec strings for every dep that should be solved.
    /// `--with` packages are pre-merged in by the caller, since the
    /// expose policy that distinguishes them is decided client-side.
    /// When `extra_records` carries source-built packages, this list
    /// also includes their runtime deps (extracted from the built
    /// `RepoDataRecord.package_record.depends`) so the daemon's
    /// solve resolves the binary closure even though the source
    /// specs themselves are not on the wire.
    pub specs: Vec<String>,
    /// Channel URLs (resolved through the project's channel config).
    pub channels: Vec<String>,
    /// Optional target platform; `None` means the daemon's host
    /// platform.
    pub platform: Option<Platform>,
    /// Force the rattler installer to clobber every record, ignoring
    /// the daemon's fingerprint short-circuit.
    pub force_reinstall: bool,
    /// Pre-built records the client wants spliced into the daemon's
    /// install. Currently used to ship locally-built source packages.
    pub extra_records: Vec<ExtraRecord>,
}

/// What [`install_via_daemon`] returns to the caller. The transaction
/// summary is what the daemon shipped over the wire; the caller turns
/// it into either per-package `AddedPackage` state changes (install)
/// or a single `UpdatedEnvironment(EnvironmentUpdate)` (update).
pub(crate) struct DaemonInstallOutput {
    /// Wire-shaped per-package change list. Empty when the daemon's
    /// fingerprint short-circuit fired.
    pub transaction: TransactionSummary,
    /// State changes accumulated by the helper:
    /// [`Project::expose_executables_from_environment`] returns one,
    /// [`Project::sync_exposed_names`] returns nothing.
    pub state_changes: StateChanges,
}

/// Drive a daemon-routed install end-to-end up to (and including)
/// expose-mapping sync. The caller supplies the `expose_type` —
/// `install` computes it from `args.expose` / `args.with`,
/// `update` derives it from the localised prefix's binaries via
/// [`detect_existing_expose_policy`], and `add` / `remove` pass
/// [`ExposedType::Nothing`] because the manifest's `exposed` list
/// is already authoritative after their explicit
/// `add_exposed_mapping` / `remove_exposed_name` mutations.
pub(crate) async fn install_via_daemon(
    project: &mut Project,
    env_name: &EnvironmentName,
    params: &DaemonRequestParams,
    socket: &Path,
    localise_mode: LocaliseMode,
    expose_type: ExposedType,
) -> miette::Result<DaemonInstallOutput> {
    let request = InstallRequest {
        env_name: env_name.as_str().to_string(),
        specs: params.specs.clone(),
        channels: params.channels.clone(),
        platform: params.platform.map(|p| p.to_string()),
        force_reinstall: params.force_reinstall,
        extra_records: params.extra_records.clone(),
    };

    // Auth target is `~/.pixi/envs/`: the directory the localised
    // prefix symlink will live under.
    let env_root = EnvRoot::from_env().await?;

    let mut conn = pixi_varlink::connect(socket, env_root.path())
        .await
        .into_diagnostic()
        .map_err(|e| e.wrap_err(format!("could not connect to {}", socket.display())))?;
    let stream = conn.install(request).await.into_diagnostic()?;
    let mut stream = std::pin::pin!(stream);

    // Drive indicatif from the daemon's marshalled reporter stream
    // using the same `MainProgressBar` primitives the local install
    // path uses. The renderer is anchored to the global multi-progress
    // so logging interleaves cleanly with the bars.
    let reporter_client = WireReporterClient::new(pixi_progress::global_multi_progress());
    let mut server_prefix: Option<PathBuf> = None;
    let mut transaction = TransactionSummary::default();
    while let Some(item) = stream.next().await {
        let reply = item
            .into_diagnostic()?
            .map_err(|err| miette!("daemon rejected install: {err:?}"))?;
        match reply {
            InstallReply::ReporterCall { call } => {
                reporter_client.on_call(call);
            }
            InstallReply::Progress { event } => {
                tracing::trace!(target: "pixi::install::reporter", ?event, "install progress");
            }
            InstallReply::Success {
                prefix,
                transaction: tx,
            } => {
                server_prefix = Some(PathBuf::from(prefix));
                transaction = tx;
                break;
            }
            InstallReply::Failed { error } => {
                return Err(miette!("daemon install failed: {error:?}"));
            }
        }
    }
    let server_prefix = server_prefix
        .ok_or_else(|| miette!("daemon ended the install stream without a terminal reply"))?;

    let local_prefix = env_root.path().join(env_name.as_str());
    if params.force_reinstall {
        // Localise refuses to clobber a real directory the user
        // didn't place themselves (or didn't place via a previous
        // walk-mode install — those carry our `.pixi-localise`
        // marker). With `--force-reinstall` the user has asked for
        // a clean slate, so remove whatever's at `local_prefix`
        // before localising. Symlinks are removed with `remove_file`
        // (unlinks the link, never the target); directories with
        // `remove_dir_all`. Modern stdlib's `remove_dir_all` uses
        // `openat`/`O_NOFOLLOW` traversal so a swap-in symlink
        // mid-removal can't redirect us.
        match tokio::fs::symlink_metadata(&local_prefix).await {
            Ok(meta) if meta.file_type().is_symlink() || meta.is_file() => {
                tokio::fs::remove_file(&local_prefix)
                    .await
                    .into_diagnostic()
                    .map_err(|e| {
                        e.wrap_err(format!(
                            "could not remove existing {} for --force-reinstall",
                            local_prefix.display()
                        ))
                    })?;
            }
            Ok(meta) if meta.is_dir() => {
                tokio::fs::remove_dir_all(&local_prefix)
                    .await
                    .into_diagnostic()
                    .map_err(|e| {
                        e.wrap_err(format!(
                            "could not remove existing {} for --force-reinstall",
                            local_prefix.display()
                        ))
                    })?;
            }
            Ok(_) | Err(_) => {
                // Nothing there or stat failed for an unrelated
                // reason. Let `localise_prefix` produce the
                // canonical error if applicable.
            }
        }
    }
    pixi_global::localise_prefix(&server_prefix, &local_prefix, localise_mode)
        .await
        .map_err(|e| miette!("{e}"))?;

    project.sync_exposed_names(env_name, expose_type).await?;
    let mut state_changes = StateChanges::default();
    state_changes |= project
        .expose_executables_from_environment(env_name)
        .await?;

    Ok(DaemonInstallOutput {
        transaction,
        state_changes,
    })
}

/// Run the daemon-routed install for the env's *current* manifest
/// state. Used by `update`, `add`, and `remove` after they've made
/// whatever manifest mutations they need; the helper picks up the
/// resulting deps, builds any source-typed entries locally, and
/// drives [`install_via_daemon`].
///
/// `expose_type` is the caller's choice — `update` derives it from
/// the pre-update prefix (auto-expose-all vs subset); `add` /
/// `remove` pass [`ExposedType::Nothing`] because the manifest's
/// existing `exposed` list is already authoritative once they've
/// added or removed mappings.
///
/// `extra_current_packages` lets callers (specifically `remove`)
/// fold names of packages that were just dropped from the manifest
/// into the returned [`EnvironmentUpdate`]'s `current_packages` set,
/// matching what the local
/// [`pixi_global::common::EnvironmentUpdate::add_removed_packages`]
/// step does — so the `report_update_changes` formatter still
/// classifies them as top-level changes.
///
/// Returns the helper's accumulated [`StateChanges`] (from
/// expose-mapping sync) and the rich [`EnvironmentUpdate`] the
/// caller wraps in a [`StateChange::UpdatedEnvironment`].
pub(crate) async fn run_install_for_env(
    project: &mut Project,
    env_name: &EnvironmentName,
    socket: &Path,
    localise_mode: LocaliseMode,
    force_reinstall: bool,
    expose_type: ExposedType,
    extra_current_packages: Vec<PackageName>,
) -> miette::Result<(StateChanges, EnvironmentUpdate)> {
    // Build any source-typed deps via the local dispatcher.
    let source_globals: Vec<GlobalSpec> = project
        .environment(env_name)
        .ok_or_else(|| miette!("Environment {} not found", env_name.as_str()))?
        .dependencies
        .specs
        .iter()
        .filter(|(_, spec)| spec.is_source())
        .map(|(name, spec)| GlobalSpec::new(name.clone(), spec.clone()))
        .collect();
    let (extra_records, source_runtime_deps) =
        build_source_specs_via_local_dispatcher(project, env_name, &source_globals).await?;

    // Build the wire-shipped EnvironmentUpdate's current_packages set:
    // the env's manifest deps post-mutation, plus any caller-supplied
    // extras (used by `remove` to keep removed names visible to the
    // formatter).
    let mut direct_dependencies: Vec<PackageName> = project
        .environment(env_name)
        .ok_or_else(|| miette!("Environment {} not found", env_name.as_str()))?
        .dependencies
        .specs
        .keys()
        .cloned()
        .collect();
    direct_dependencies.extend(extra_current_packages);

    let mut params = manifest_snapshot_for_env(project, env_name, force_reinstall)?;
    params.specs.extend(source_runtime_deps);
    params.extra_records = extra_records;

    let output = install_via_daemon(
        project,
        env_name,
        &params,
        socket,
        localise_mode,
        expose_type,
    )
    .await?;
    let environment_update =
        wire_transaction_to_environment_update(&output.transaction, direct_dependencies)?;

    Ok((output.state_changes, environment_update))
}

/// Capture the pre-update auto-expose policy from the local prefix's
/// binaries against the manifest's exposed list. Used by `update` (and
/// later by `sync`) to preserve the user's "expose all" vs "expose a
/// subset" intent across a re-install. Returns
/// [`ExposedType::All`] when the env isn't materialised locally yet.
pub(crate) async fn detect_existing_expose_policy(
    project: &Project,
    env_name: &EnvironmentName,
) -> miette::Result<ExposedType> {
    let env_root = EnvRoot::from_env().await?;
    let local_prefix = env_root.path().join(env_name.as_str());
    if !local_prefix.exists() {
        return Ok(ExposedType::All);
    }
    let env_binaries = project.executables_of_direct_dependencies(env_name).await?;
    let exposed_mapping_binaries = &project
        .environment(env_name)
        .ok_or_else(|| miette!("Environment {} not found", env_name.as_str()))?
        .exposed;
    Ok(
        if pixi_global::common::check_all_exposed(&env_binaries, exposed_mapping_binaries) {
            ExposedType::All
        } else {
            ExposedType::Nothing
        },
    )
}

/// Read channels, platform, and dependency specs from the manifest's
/// existing entry for `env_name`, render to wire strings, and emit a
/// [`DaemonRequestParams`] suitable for `update`'s daemon path.
pub(crate) fn manifest_snapshot_for_env(
    project: &Project,
    env_name: &EnvironmentName,
    force_reinstall: bool,
) -> miette::Result<DaemonRequestParams> {
    let environment = project
        .environment(env_name)
        .ok_or_else(|| miette!("Environment {} not found", env_name.as_str()))?;

    let channel_config = project.config().global_channel_config().clone();

    let mut prioritised: Vec<_> = environment.channels.iter().collect();
    prioritised.sort_by(|a, b| {
        b.priority
            .unwrap_or(0)
            .cmp(&a.priority.unwrap_or(0))
            .then_with(|| a.channel.to_string().cmp(&b.channel.to_string()))
    });
    let channels: Vec<String> = prioritised
        .iter()
        .filter_map(|pc| pc.channel.clone().into_base_url(&channel_config).ok())
        .map(|url| url.to_string())
        .collect();

    // Only ship binary specs over the wire — source-typed deps are
    // built locally and surface through `extra_records`. The caller
    // appends the source runtime deps it gets back from
    // [`build_source_specs_via_local_dispatcher`] to the returned
    // `specs`.
    let specs: Vec<String> = environment
        .dependencies
        .specs
        .iter()
        .filter(|(_, spec)| !spec.is_source())
        .map(|(name, spec)| {
            spec.clone()
                .to_match_spec(name, &channel_config)
                .map(|ms| ms.to_string())
                .into_diagnostic()
        })
        .collect::<miette::Result<Vec<_>>>()?;

    Ok(DaemonRequestParams {
        specs,
        channels,
        platform: environment.platform,
        force_reinstall,
        extra_records: Vec::new(),
    })
}

/// Convert the daemon's wire transaction summary into the domain
/// [`EnvironmentUpdate`] the local install/update paths carry. The
/// caller supplies `direct_dependencies` (the env's manifest specs)
/// since the daemon doesn't know the manifest.
pub(crate) fn wire_transaction_to_environment_update(
    summary: &TransactionSummary,
    direct_dependencies: Vec<PackageName>,
) -> miette::Result<EnvironmentUpdate> {
    let mut changes: std::collections::HashMap<PackageName, InstallChange> =
        std::collections::HashMap::with_capacity(summary.package_changes.len());
    for pc in &summary.package_changes {
        let name = PackageName::from_str(&pc.name)
            .map_err(|e| miette!("daemon returned invalid package name {:?}: {e}", pc.name))?;
        let change = match &pc.change {
            InstallChangeWire::Installed { version } => {
                InstallChange::Installed(parse_version(version, &pc.name)?)
            }
            InstallChangeWire::Upgraded { from, to } => InstallChange::Upgraded(
                parse_version(from, &pc.name)?,
                parse_version(to, &pc.name)?,
            ),
            InstallChangeWire::TransitiveUpgraded { from, to } => {
                InstallChange::TransitiveUpgraded(
                    parse_version(from, &pc.name)?,
                    parse_version(to, &pc.name)?,
                )
            }
            InstallChangeWire::Reinstalled { from, to } => InstallChange::Reinstalled(
                parse_version(from, &pc.name)?,
                parse_version(to, &pc.name)?,
            ),
            InstallChangeWire::Removed => InstallChange::Removed,
        };
        changes.insert(name, change);
    }
    Ok(EnvironmentUpdate::new(changes, direct_dependencies))
}

fn parse_version(s: &str, pkg: &str) -> miette::Result<Version> {
    Version::from_str(s)
        .map_err(|e| miette!("daemon returned invalid version {s:?} for package {pkg:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixi_varlink::{InstallChangeWire as Wire, PackageChange};

    fn pc(name: &str, change: Wire) -> PackageChange {
        PackageChange {
            name: name.to_string(),
            change,
        }
    }

    fn pkg(name: &str) -> PackageName {
        PackageName::from_str(name).unwrap()
    }

    #[test]
    fn wire_transaction_round_trips_every_install_change_variant() {
        let summary = TransactionSummary {
            package_changes: vec![
                pc(
                    "foo",
                    Wire::Installed {
                        version: "1.2.3".to_string(),
                    },
                ),
                pc(
                    "bar",
                    Wire::Upgraded {
                        from: "1.0".to_string(),
                        to: "2.0".to_string(),
                    },
                ),
                pc(
                    "baz",
                    Wire::TransitiveUpgraded {
                        from: "1.0".to_string(),
                        to: "1.0".to_string(),
                    },
                ),
                pc(
                    "qux",
                    Wire::Reinstalled {
                        from: "1.0".to_string(),
                        to: "1.0".to_string(),
                    },
                ),
                pc("removed", Wire::Removed),
            ],
        };
        let direct = vec![pkg("foo"), pkg("bar")];

        let env_update = wire_transaction_to_environment_update(&summary, direct.clone()).unwrap();
        let changes = env_update.changes();
        assert_eq!(changes.len(), 5, "every wire entry must round-trip");
        assert!(matches!(
            changes.get(&pkg("foo")),
            Some(InstallChange::Installed(_))
        ));
        assert!(matches!(
            changes.get(&pkg("bar")),
            Some(InstallChange::Upgraded(_, _))
        ));
        assert!(matches!(
            changes.get(&pkg("baz")),
            Some(InstallChange::TransitiveUpgraded(_, _))
        ));
        assert!(matches!(
            changes.get(&pkg("qux")),
            Some(InstallChange::Reinstalled(_, _))
        ));
        assert!(matches!(
            changes.get(&pkg("removed")),
            Some(InstallChange::Removed)
        ));
        assert_eq!(env_update.current_packages(), &direct);
    }

    #[test]
    fn wire_transaction_rejects_invalid_version() {
        let summary = TransactionSummary {
            package_changes: vec![pc(
                "foo",
                Wire::Installed {
                    version: "not-a-version!".to_string(),
                },
            )],
        };
        let err = wire_transaction_to_environment_update(&summary, Vec::new()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("invalid version"),
            "expected error to mention invalid version, got {msg:?}"
        );
    }

    #[test]
    fn wire_transaction_empty_summary_yields_empty_update() {
        let summary = TransactionSummary::default();
        let env_update = wire_transaction_to_environment_update(&summary, Vec::new()).unwrap();
        assert!(env_update.is_empty());
    }

    /// `resolve_localise_mode` honours the priority chain CLI > env >
    /// config > default. Both install and update consult this — we
    /// pin the layering so flag/env/config don't drift.
    #[test]
    fn localise_mode_priority_cli_overrides_config() {
        let mut config = Config::default();
        config.remote.localise = Some("symlink".into());
        let mode = resolve_localise_mode(Some("copy"), &config).unwrap();
        assert!(
            matches!(mode, LocaliseMode::Copy),
            "CLI must override config, got {mode:?}"
        );
    }

    #[test]
    fn localise_mode_default_when_unset() {
        let config = Config::default();
        // SAFETY: tests are single-threaded by default in Rust; this
        // test doesn't read any other env var that could race.
        unsafe {
            std::env::remove_var("PIXI_GLOBAL_LOCALISE");
        }
        let mode = resolve_localise_mode(None, &config).unwrap();
        assert_eq!(mode, LocaliseMode::default());
    }
}
