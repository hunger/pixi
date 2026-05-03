//! `Install` RPC: serve a `pixi global install` request from a daemon
//! that owns a shared `data` and `cache` directory.
//!
//! Per request the server: hashes the authenticated client directory +
//! the requested env name into `<HASH>` (HMAC-SHA-256 keyed by a
//! server-side salt), constructs a `pixi_command_dispatcher` over the
//! configured `cache` root, solves the requested specs, lays down the
//! resulting records under `<data>/<HASH>/`, and persists the
//! environment fingerprint so subsequent identical requests
//! short-circuit. When the streaming RPC is invoked with `more=true`,
//! [`run_install`] is given an `mpsc::UnboundedSender<ProgressEvent>`
//! that the [`crate::wire_reporter::WireReporter`] funnels every
//! dispatcher- and rattler-side reporter callback into; the RPC body
//! interleaves those as [`InstallReply::Progress`] events ahead of
//! the terminal [`InstallReply::Success`] / [`InstallReply::Failed`].

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hmac::{Hmac, Mac};
use ordermap::OrderMap;
use pixi_command_dispatcher::{
    BuildEnvironment, CacheDirs, CommandDispatcher, EnvironmentRef, EnvironmentSpec, EphemeralEnv,
    InstallPixiEnvironmentSpec,
    keys::{SolvePixiEnvironmentKey, SolvePixiEnvironmentSpec},
};
use pixi_path::AbsPathBuf;
use pixi_record::UnresolvedPixiRecord;
use pixi_spec::PixiSpec;
use pixi_spec_containers::DependencyMap;
use rattler::install::{Transaction, TransactionOperation};
use rattler_cache::package_cache::PackageCache;
use rattler_conda_types::{
    ChannelConfig, ChannelUrl, HasArtifactIdentificationRefs, MatchSpec, PackageName,
    ParseStrictness, Platform, RepoDataRecord, prefix::Prefix,
};
use rattler_virtual_packages::{VirtualPackageOverrides, VirtualPackages};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zlink::introspect::Type;

/// Length of the HMAC salt in bytes. 16 bytes is wide enough that an
/// attacker can't enumerate the keyspace and narrow enough that a
/// hex-encoded representation fits comfortably in CLI flags and config
/// values.
pub const SALT_LEN: usize = 16;

/// Server-side configuration that enables the `Install` RPC. Both `data`
/// and `cache` are mandatory in the calling layer (CLI / config); the
/// service receives them already resolved to absolute, existing paths.
///
/// Carries a per-HASH lock map alongside the static config so the daemon
/// can reject concurrent `Install` calls against the same prefix. The
/// second concurrent caller fast-fails with
/// [`InstallFailure::DuplicateEnvironment`] rather than blocking on the
/// first; a sequential retry after the first finishes hits the engine's
/// fingerprint short-circuit and returns the same prefix without
/// re-running the rattler installer.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Root under which `<HASH>/` install prefixes live.
    pub data: PathBuf,
    /// Shared cache root for repodata, package archives, and
    /// source-build artifacts. Passed to the dispatcher's
    /// [`CacheDirs`] so every install reuses the same cached state.
    pub cache: PathBuf,
    /// HMAC key folded into the env hash. Defaults to all zeros when
    /// the operator doesn't set `--salt` / `serve.salt`.
    pub salt: [u8; SALT_LEN],
    /// Per-HASH mutexes. The lock-map mutex is `parking_lot` (acquired
    /// only for the lookup-or-insert), the per-HASH locks are
    /// `tokio::sync::Mutex` (held across the install's `await` points).
    /// In-memory only; on daemon restart the map starts empty, which is
    /// fine because there are no in-flight installs at that point.
    locks: Arc<parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl ServerConfig {
    /// Convenience: build a `ServerConfig` from already-validated paths
    /// and an optional hex-encoded salt. Returns an error message
    /// suitable for surfacing through `pixi serve` startup if the salt
    /// can't be decoded as 16 hex bytes.
    pub fn from_parts(
        data: PathBuf,
        cache: PathBuf,
        salt_hex: Option<&str>,
    ) -> Result<Self, ServerConfigError> {
        let salt = match salt_hex {
            None => [0u8; SALT_LEN],
            Some(hex_str) => {
                let bytes =
                    hex::decode(hex_str.trim()).map_err(|e| ServerConfigError::InvalidSalt {
                        reason: format!("not valid hex: {e}"),
                    })?;
                if bytes.len() != SALT_LEN {
                    return Err(ServerConfigError::InvalidSalt {
                        reason: format!(
                            "expected {SALT_LEN} bytes ({} hex chars), got {} bytes",
                            SALT_LEN * 2,
                            bytes.len()
                        ),
                    });
                }
                let mut out = [0u8; SALT_LEN];
                out.copy_from_slice(&bytes);
                out
            }
        };
        Ok(Self {
            data,
            cache,
            salt,
            locks: Arc::default(),
        })
    }

    /// Look up or create the per-HASH mutex protecting installs into
    /// `<data>/<hash>/`. The caller `try_lock_owned`s the returned
    /// mutex; if it's already held, the install is rejected with
    /// [`InstallFailure::DuplicateEnvironment`]. Otherwise the guard
    /// is held across the entire install pipeline so the on-disk
    /// state stays coherent.
    fn install_lock(&self, hash: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock();
        locks
            .entry(hash.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

/// Error returned when [`ServerConfig::from_parts`] can't accept the
/// supplied inputs. Surfaced through `pixi serve` startup, not through
/// the wire.
#[derive(Debug, thiserror::Error)]
pub enum ServerConfigError {
    #[error("invalid salt: {reason}")]
    InvalidSalt { reason: String },
}

/// Compute the prefix-naming hash for a given (authenticated client
/// directory, env name) pair.
///
/// HMAC-SHA-256 keyed by `salt`, with message
/// `<auth_path>/<env_name>`. The output is the canonical prefix
/// directory name under `<data>/`. A 16-byte salt is sufficient: an
/// attacker who can't read or write the data dir gains nothing from
/// guessing the hash, and the hash is not a secret either way.
///
/// Why HMAC and not plain SHA-256: this hash binds together two
/// attacker-influenced inputs (the client directory path and the env
/// name). Plain SHA-256 over their concatenation is technically fine,
/// but HMAC's domain-separation properties make this a no-think choice
/// that costs nothing to use.
pub fn env_hash(salt: &[u8; SALT_LEN], auth_path: &Path, env_name: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(salt).expect("HMAC-SHA-256 accepts any key length");
    // The path bytes go in verbatim. On Unix this is the raw OS string;
    // pixi_varlink is cfg(unix)-bound by its other deps so we don't
    // worry about cross-platform UTF-8 conversion here.
    mac.update(auth_path.as_os_str().as_encoded_bytes());
    mac.update(b"/");
    mac.update(env_name.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Wire request for the `Install` RPC.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct InstallRequest {
    /// Name of the environment to install. Must match the
    /// `EnvironmentName` shape (alphanumeric, `_`, `-`).
    pub env_name: String,

    /// MatchSpecs to solve, in the form pixi accepts on the CLI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub specs: Vec<String>,

    /// Channels to query. URL-shaped strings; the daemon will normalise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,

    /// Target platform (e.g. `linux-64`). Unset means the daemon's host
    /// platform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,

    /// Force a rebuild + reinstall of every package, ignoring the
    /// fingerprint short-circuit.
    #[serde(default, skip_serializing_if = "is_false")]
    pub force_reinstall: bool,

    /// Pre-built records the client wants spliced into the daemon's
    /// install. Each entry names a `.conda` file path the daemon
    /// can read off the local filesystem (the client and daemon are
    /// colocated by the daemon's deployment model) plus the
    /// `RepoDataRecord` describing the package. The daemon extracts
    /// the artefact into its package cache, then includes the
    /// carried record in the rattler transaction, exactly as if it
    /// had solved and fetched the package itself.
    ///
    /// Currently used by `pixi global install` / `update` to ship
    /// locally-built source packages — the client builds source
    /// specs against its own dispatcher, then routes the resulting
    /// records (with binary deps still solved server-side) through
    /// here. Empty for installs with no source packages, in which
    /// case the field is elided on the wire.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_records: Vec<ExtraRecord>,
}

/// One package the client built locally, ready to be spliced into
/// the daemon's install pipeline. See [`InstallRequest::extra_records`].
///
/// `record_json` is a serde-encoded
/// `rattler_conda_types::RepoDataRecord`. It rides on the wire as an
/// opaque string so this crate doesn't have to reproduce the full
/// RepoDataRecord type with `zlink::Type` derived; the daemon parses
/// it via `serde_json::from_str` before splicing.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ExtraRecord {
    /// Absolute path to a `.conda` file the daemon process can read.
    /// On rejection, the daemon raises [`InstallFailure::InstallFailed`].
    pub artifact_path: String,
    /// Serde-JSON-encoded `rattler_conda_types::RepoDataRecord`. The
    /// `url` field is informational at this point; the daemon resolves
    /// the artefact via `artifact_path`.
    pub record_json: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Wire request for the `Uninstall` RPC. Removes
/// `<data>/<HASH>/` for the named env on the daemon. Client-side
/// state (`~/.pixi/envs/<env>`, the manifest entry, trampolines,
/// shortcuts, completions) is handled by the client before or
/// after calling this — the daemon owns only its data dir.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct UninstallRequest {
    /// Name of the environment whose `<data>/<HASH>/` prefix to
    /// remove. Must match the [`pixi_global::EnvironmentName`]
    /// shape — see [`validate_env_name`].
    pub env_name: String,
}

/// Terminal reply from `Uninstall`. The variant naming mirrors
/// [`InstallReply`]'s success/failed split so client error
/// handling stays uniform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UninstallReply {
    /// `<data>/<HASH>/` was removed (or was already absent and the
    /// caller didn't ask to fail in that case).
    Success,
    /// The daemon refused or failed the uninstall.
    Failed {
        /// Why the uninstall didn't proceed.
        error: UninstallFailure,
    },
}

/// Why an `Uninstall` RPC failed. Mirrors the shape of
/// [`InstallFailure`] for the variants that are common to both
/// methods (`NotAuthenticated`, `ServerNotConfigured`,
/// `InvalidEnvName`); adds an `EnvNotFound` for "no such
/// `<data>/<HASH>/` to remove" and a catch-all `RemoveFailed` for
/// I/O errors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum UninstallFailure {
    /// The connection has not completed the `Hello` /
    /// `Authenticate` handshake.
    NotAuthenticated,
    /// `pixi serve` was started without `--data` / `--cache`; the
    /// uninstall RPC has nothing to remove.
    ServerNotConfigured {
        /// Operator-facing hint identical to install's.
        hint: String,
    },
    /// `env_name` doesn't match the [`pixi_global::EnvironmentName`]
    /// shape.
    InvalidEnvName {
        /// Echo of the offending name.
        name: String,
        /// Concrete reason from
        /// [`pixi_global::EnvironmentName::from_str`].
        reason: String,
    },
    /// `<data>/<HASH>/` does not exist on the daemon — either the
    /// env was never installed via this daemon or a previous
    /// uninstall already removed it.
    EnvNotFound {
        /// Echo of the env name.
        env_name: String,
    },
    /// Catch-all for I/O / lock failures during removal. The
    /// caller (and any user-facing surface) can show `reason`
    /// verbatim.
    RemoveFailed {
        /// Free-form reason; safe to surface to the user.
        reason: String,
    },
}

/// Drive an `Uninstall` request: take the per-`(client, env)`
/// install lock so a concurrent install can't race the removal,
/// then `remove_dir_all` the matching `<data>/<HASH>/` directory.
///
/// Returns `Ok(())` on success and on
/// [`UninstallFailure::EnvNotFound`] when the prefix was already
/// gone — the second is communicated via the returned error so the
/// client can decide whether to surface "no-op" or "removed";
/// today's CLI dispatch surfaces both as success (idempotent) and
/// only errors on I/O failures.
pub(crate) async fn run_uninstall(
    cfg: &ServerConfig,
    auth_path: &Path,
    request: &UninstallRequest,
) -> Result<(), UninstallFailure> {
    let hash = env_hash(&cfg.salt, auth_path, &request.env_name);
    let prefix_path = cfg.data.join(&hash);

    // Hold the same per-HASH lock the install path uses so a
    // concurrent install can't materialise a half-removed prefix.
    // `try_lock` rather than `lock`: if an install is running, fail
    // loudly with `RemoveFailed` instead of silently waiting.
    let lock = cfg.install_lock(&hash);
    let _guard = lock
        .try_lock_owned()
        .map_err(|_| UninstallFailure::RemoveFailed {
            reason: format!(
                "install in flight for env {:?}; retry once it completes",
                request.env_name
            ),
        })?;

    if !prefix_path.exists() {
        return Err(UninstallFailure::EnvNotFound {
            env_name: request.env_name.clone(),
        });
    }
    fs_err::remove_dir_all(&prefix_path).map_err(|e| UninstallFailure::RemoveFailed {
        reason: format!("could not remove {}: {e}", prefix_path.display()),
    })?;
    Ok(())
}

/// One streaming reply from `Install`.
///
/// `Install` is declared `#[zlink(more)]` so the body can interleave
/// any number of progress / reporter events ahead of the terminal
/// reply. The terminal reply is exactly one of [`Success`](Self::Success)
/// or [`Failed`](Self::Failed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InstallReply {
    /// Free-form progress event reserved for future progress-bar
    /// rendering paths. Currently unused for daemon-routed installs.
    Progress {
        /// The notification.
        event: crate::ProgressEvent,
    },
    /// One marshalled reporter callback the dispatcher (or its
    /// rattler subsystems) made on the server during the install.
    /// Streamed in order; the client unmarshals each into the
    /// corresponding call on its [`crate::ReporterClient`].
    ReporterCall {
        /// The marshalled call.
        call: crate::ReporterCall,
    },
    /// Terminal success. Final reply in the stream.
    Success {
        /// Absolute server-side install prefix path the client should
        /// localise. Once it's reachable at `~/.pixi/envs/<env>` the
        /// client takes over with the local install path's
        /// expose / trampoline / finalise machinery.
        prefix: String,
        /// Per-package summary of the rattler transaction the daemon
        /// just executed. Empty when the fingerprint short-circuit
        /// fired (nothing changed). The client folds this into a
        /// `pixi_global::common::EnvironmentUpdate` so daemon-routed
        /// `update` (and `install` rerun) reports the same per-package
        /// state changes a fully-local install would.
        #[serde(default, skip_serializing_if = "TransactionSummary::is_empty")]
        transaction: TransactionSummary,
    },
    /// Terminal failure. Final reply in the stream.
    Failed {
        /// Why the install was rejected or could not proceed.
        error: InstallFailure,
    },
}

/// Wire-serialisable mirror of `pixi_global::common::InstallChange`.
///
/// Lives in `pixi_varlink` rather than re-exporting the `pixi_global`
/// type because adding a `pixi_varlink → pixi_global` (or vice versa)
/// edge would pull the entire global-CLI surface into the daemon. The
/// `pixi_cli` daemon helper translates between the two.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InstallChangeWire {
    /// Package was installed fresh.
    Installed { version: String },
    /// Top-level upgrade (different base version).
    Upgraded { from: String, to: String },
    /// Build-string-only upgrade (same base version) — surfaced as a
    /// transitive bump in the user-visible report.
    TransitiveUpgraded { from: String, to: String },
    /// Reinstall (same version on both sides — package metadata or
    /// build refresh).
    Reinstalled { from: String, to: String },
    /// Package was removed from the prefix.
    Removed,
}

/// Per-package change record in a [`TransactionSummary`]. The
/// `change` payload is `#[serde(flatten)]`'d so the wire form is one
/// flat object per package, e.g.
/// `{"name":"xz","kind":"installed","version":"5.4.6"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageChange {
    /// Package name (normalised — matches `PackageName::as_normalized`).
    pub name: String,
    /// What happened to this package in the transaction.
    #[serde(flatten)]
    pub change: InstallChangeWire,
}

/// Wire-serialisable summary of the rattler transaction the daemon
/// executed. Mirrors the data carried in
/// `pixi_global::common::EnvironmentUpdate`'s `package_changes` field.
///
/// `current_packages` (the env's direct-dependency names) is *not*
/// shipped: the client's manifest is the source of truth for that and
/// the daemon doesn't have it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TransactionSummary {
    /// Per-package change. Sorted by package name for deterministic
    /// wire output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub package_changes: Vec<PackageChange>,
}

impl TransactionSummary {
    /// Empty summary — the daemon's fingerprint short-circuit fired
    /// and nothing was changed in the prefix.
    pub fn is_empty(&self) -> bool {
        self.package_changes.is_empty()
    }

    /// Build a summary from a rattler `Transaction`. Mirrors the
    /// per-operation mapping in
    /// `pixi_global::common::get_install_changes` so client-side
    /// reporting is byte-for-byte consistent between local and daemon
    /// installs.
    pub fn from_transaction<Old, New>(transaction: &Transaction<Old, New>) -> Self
    where
        Old: HasArtifactIdentificationRefs,
        New: HasArtifactIdentificationRefs,
    {
        let mut package_changes: Vec<PackageChange> = transaction
            .operations
            .iter()
            .map(|op| match op {
                TransactionOperation::Install(p) => PackageChange {
                    name: p.name().as_normalized().to_string(),
                    change: InstallChangeWire::Installed {
                        version: p.version().version().to_string(),
                    },
                },
                TransactionOperation::Change { old, new } => {
                    let old_v = old.version().version();
                    let new_v = new.version().version();
                    let change = if old_v == new_v {
                        InstallChangeWire::TransitiveUpgraded {
                            from: old_v.to_string(),
                            to: new_v.to_string(),
                        }
                    } else {
                        InstallChangeWire::Upgraded {
                            from: old_v.to_string(),
                            to: new_v.to_string(),
                        }
                    };
                    PackageChange {
                        name: new.name().as_normalized().to_string(),
                        change,
                    }
                }
                TransactionOperation::Reinstall { old, new } => PackageChange {
                    name: new.name().as_normalized().to_string(),
                    change: InstallChangeWire::Reinstalled {
                        from: old.version().version().to_string(),
                        to: new.version().version().to_string(),
                    },
                },
                TransactionOperation::Remove(p) => PackageChange {
                    name: p.name().as_normalized().to_string(),
                    change: InstallChangeWire::Removed,
                },
            })
            .collect();
        package_changes.sort_by(|a, b| a.name.cmp(&b.name));
        Self { package_changes }
    }
}

/// Why an `Install` RPC failed.
///
/// Lives inside the streaming reply rather than as a varlink-level
/// `ReplyError` because zlink streaming methods can't currently
/// short-circuit with a typed error and an empty stream gives the
/// client no context. Spelling failure modes out as data also matches
/// the masterplan's `GlobalInstallError` variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum InstallFailure {
    /// The connection has not completed the `Hello` / `Authenticate`
    /// handshake.
    NotAuthenticated,
    /// `pixi serve` was started without `--data` / `--cache` (or
    /// `serve.data` / `serve.cache`); the install RPC has nowhere to
    /// materialise prefixes.
    ServerNotConfigured {
        /// Operator-facing hint (e.g. "pass --data and --cache to
        /// `pixi serve`").
        hint: String,
    },
    /// `env_name` doesn't match the `EnvironmentName` shape.
    InvalidEnvName {
        /// The offending name, echoed back so the client can correct
        /// itself.
        name: String,
        /// Concrete reason (e.g. "empty", "contains '/'").
        reason: String,
    },
    /// Another install for the same `(auth_path, env_name)` pair is
    /// already in flight on this daemon. The second request fails
    /// fast rather than blocking on the first; the client should
    /// wait for the in-flight install to finish and retry.
    DuplicateEnvironment {
        /// Echo of the env name, so the client knows which install
        /// is already in flight when displaying the error.
        env_name: String,
    },
    /// The request carried one or more source specs (path / URL /
    /// git) that the daemon cannot resolve. Path-based source specs
    /// reference filesystem locations on the *client* and the daemon
    /// has no view of them by design; URL/git source specs are
    /// rejected for now too because the daemon doesn't run
    /// client-supplied build backends. The official client
    /// (`pixi global install` / `pixi global update --socket`)
    /// catches these locally with a more actionable message; this
    /// variant is the daemon's defense against non-conforming
    /// clients.
    UnsupportedSourceSpec {
        /// Names of the packages whose specs were source-typed.
        packages: Vec<String>,
    },
    /// Catch-all for install failures: solve errors, malformed
    /// match-specs, dispatcher errors, prefix I/O failures.
    InstallFailed {
        /// Free-form reason; safe to surface verbatim to the user.
        reason: String,
    },
}

/// Drive a real `Install` request end-to-end: build a dispatcher
/// rooted at the server's `cache` dir, solve the requested specs
/// into records, and lay them down in `<data>/<HASH>/`. Persists
/// the fingerprint marker so subsequent identical requests
/// short-circuit. Trampolines and expose mappings are the client's
/// concern — the daemon stops at the materialised prefix.
///
/// On success returns the absolute prefix path. Failures land in
/// [`InstallFailure::InstallFailed`] with the underlying message — the
/// daemon hides nothing from the caller because all failures here are
/// operator-actionable (network, disk, malformed spec).
pub(crate) async fn run_install(
    cfg: &ServerConfig,
    auth_path: &Path,
    request: &InstallRequest,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::ReporterCall>>,
) -> Result<(PathBuf, TransactionSummary), InstallFailure> {
    let hash = env_hash(&cfg.salt, auth_path, &request.env_name);
    let prefix_path = cfg.data.join(&hash);

    // Reject concurrent installs against the same prefix outright
    // rather than serialising them: a user running `pixi global
    // install foo` from two terminals at once should see the second
    // attempt fail fast with an explanation, not silently block on
    // the first. The guard is held across every await below; the
    // next caller after the first completes acquires cleanly and,
    // typically, hits the engine's `EnvironmentFingerprint::read`
    // short-circuit.
    let lock = cfg.install_lock(&hash);
    let _guard = lock
        .try_lock_owned()
        .map_err(|_| InstallFailure::DuplicateEnvironment {
            env_name: request.env_name.clone(),
        })?;

    let channels = parse_channels(&request.channels)?;
    let platform = parse_platform(request.platform.as_deref())?;
    let virtual_packages = VirtualPackages::detect(&VirtualPackageOverrides::default())
        .map_err(|e| InstallFailure::InstallFailed {
            reason: format!("could not detect virtual packages: {e}"),
        })?
        .into_generic_virtual_packages()
        .collect();
    let build_environment = BuildEnvironment::simple(platform, virtual_packages);

    let channel_config = default_channel_config();
    let dependencies = parse_specs(&request.specs, &channel_config)?;
    refuse_source_dependencies(&dependencies)?;

    let cache_root = AbsPathBuf::new(cfg.cache.clone())
        .map_err(|err| InstallFailure::InstallFailed {
            reason: format!("--cache must be absolute: {err}"),
        })?
        .into_assume_dir();

    let mut dispatcher_builder = CommandDispatcher::builder()
        .with_cache_dirs(CacheDirs::new(cache_root))
        .with_channel_config(channel_config);
    if let Some(tx) = progress_tx {
        let reporter = std::sync::Arc::new(crate::wire_reporter::WireReporter::new(tx));
        dispatcher_builder = dispatcher_builder
            .with_pixi_install_reporter(reporter.clone())
            .with_pixi_solve_reporter(reporter.clone())
            .with_conda_solve_reporter(reporter.clone())
            .with_git_checkout_reporter(reporter.clone())
            .with_url_checkout_reporter(reporter.clone())
            .with_instantiate_backend_reporter(reporter.clone())
            .with_build_backend_metadata_reporter(reporter.clone())
            .with_source_metadata_reporter(reporter.clone())
            .with_source_record_reporter(reporter.clone())
            .with_backend_source_build_reporter(reporter);
    }
    let dispatcher = dispatcher_builder.finish();

    let solve_spec = SolvePixiEnvironmentSpec {
        dependencies,
        constraints: DependencyMap::default(),
        dev_sources: OrderMap::new(),
        installed: Arc::from([]),
        installed_source_hints: Default::default(),
        strategy: Default::default(),
        preferred_build_source: Arc::new(BTreeMap::new()),
        env_ref: EnvironmentRef::Ephemeral(EphemeralEnv::new(
            request.env_name.clone(),
            EnvironmentSpec {
                channels: channels.clone(),
                build_environment: build_environment.clone(),
                variants: Default::default(),
                exclude_newer: None,
                channel_priority: Default::default(),
            },
        )),
    };
    let records_arc = dispatcher
        .engine()
        .compute(&SolvePixiEnvironmentKey::new(solve_spec))
        .await
        .map_err(|e| InstallFailure::InstallFailed {
            reason: format!("solve failed: {e}"),
        })?
        .map_err(|e| InstallFailure::InstallFailed {
            reason: format!("solve failed: {e}"),
        })?;

    // Splice client-built records (typically locally-built source
    // packages) into the install. The daemon reads each `.conda`
    // file off the local filesystem and extracts it into its own
    // package cache so the rattler installer finds the package by
    // name+version+build during the transaction below — without
    // ever fetching from the URL on the carried `RepoDataRecord`.
    let extra_unresolved = ingest_extra_records(&request.extra_records, &cfg.cache).await?;

    fs_err::create_dir_all(&prefix_path).map_err(|e| InstallFailure::InstallFailed {
        reason: format!("could not create prefix {}: {e}", prefix_path.display()),
    })?;
    let prefix = Prefix::create(&prefix_path).map_err(|e| InstallFailure::InstallFailed {
        reason: format!("could not initialise prefix {}: {e}", prefix_path.display()),
    })?;

    let install_spec = InstallPixiEnvironmentSpec {
        name: request.env_name.clone(),
        records: records_arc
            .iter()
            .cloned()
            .map(Into::into)
            .chain(extra_unresolved.iter().cloned())
            .collect(),
        prefix,
        installed: None,
        ignore_packages: None,
        build_environment,
        force_reinstall: if request.force_reinstall {
            records_arc
                .iter()
                .map(|r| r.name().clone())
                .chain(extra_unresolved.iter().map(|r| r.name().clone()))
                .collect()
        } else {
            Default::default()
        },
        exclude_newer: None,
        channels,
        variant_configuration: None,
        variant_files: None,
    };
    let install_result = dispatcher
        .install_pixi_environment(install_spec)
        .await
        .map_err(|e| InstallFailure::InstallFailed {
            reason: format!("install failed: {e}"),
        })?;

    // Persist the fingerprint so future requests with the same record
    // set short-circuit through `EnvironmentFingerprint::read` inside
    // the engine's install primitive. Errors here are non-fatal: the
    // install already succeeded, the worst that happens is the next
    // invocation re-runs the rattler installer.
    if let Err(e) = install_result.installed_fingerprint.write(&prefix_path) {
        tracing::warn!(
            path = %prefix_path.display(),
            error = %e,
            "could not persist environment fingerprint"
        );
    }

    let summary = TransactionSummary::from_transaction(&install_result.transaction);
    Ok((prefix_path, summary))
}

/// Extract each `extra_records` artefact into the daemon's package
/// cache and return the parsed `RepoDataRecord`s wrapped as
/// [`UnresolvedPixiRecord::Binary`]s ready to splice into
/// [`InstallPixiEnvironmentSpec::records`].
///
/// The daemon's package cache lives at `<cache>/pkgs/`; once a
/// `.conda` is extracted there by `name-version-build`, the rattler
/// installer's cache lookup hits before any URL fetch fires. That's
/// what makes the carried record's `url` field informational here:
/// the artefact is already in cache by the time the install
/// transaction runs.
async fn ingest_extra_records(
    extras: &[ExtraRecord],
    cache_root: &Path,
) -> Result<Vec<UnresolvedPixiRecord>, InstallFailure> {
    if extras.is_empty() {
        return Ok(Vec::new());
    }
    let pkgs_dir = cache_root.join(pixi_consts::consts::CACHED_PACKAGES);
    let pkg_cache = PackageCache::new(&pkgs_dir);
    let mut out = Vec::with_capacity(extras.len());
    for extra in extras {
        let artifact = Path::new(&extra.artifact_path);
        if !artifact.is_absolute() {
            return Err(InstallFailure::InstallFailed {
                reason: format!(
                    "extra-record artifact path must be absolute, got {:?}",
                    extra.artifact_path
                ),
            });
        }
        pkg_cache
            .get_or_fetch_from_path(artifact, None)
            .await
            .map_err(|e| InstallFailure::InstallFailed {
                reason: format!(
                    "could not extract {:?} into the daemon's package cache: {e}",
                    extra.artifact_path
                ),
            })?;
        let record: RepoDataRecord = serde_json::from_str(&extra.record_json).map_err(|e| {
            InstallFailure::InstallFailed {
                reason: format!("invalid record_json on extra record: {e}"),
            }
        })?;
        out.push(UnresolvedPixiRecord::Binary(Arc::new(record)));
    }
    Ok(out)
}

/// Reject any source-typed (`PixiSpec::Path` / `Url` / `Git`)
/// entries in the parsed dependency map.
///
/// Path-typed source specs reference filesystem locations on the
/// client and the daemon by design has no view of them. URL- and
/// git-typed source specs are also rejected for now: the daemon
/// doesn't run the client's `BackendOverride`, and without the
/// build-backend mock that tests inject, a source build server-side
/// has no realistic chance of matching what the local path produces.
/// The official client catches these locally before sending; this
/// check is the daemon's defense against a non-conforming or
/// older-version client.
fn refuse_source_dependencies(
    dependencies: &DependencyMap<PackageName, PixiSpec>,
) -> Result<(), InstallFailure> {
    let bad: Vec<String> = dependencies
        .iter_specs()
        .filter(|(_, spec)| spec.is_source())
        .map(|(name, _)| name.as_normalized().to_string())
        .collect();
    if bad.is_empty() {
        Ok(())
    } else {
        Err(InstallFailure::UnsupportedSourceSpec { packages: bad })
    }
}

fn parse_channels(channels: &[String]) -> Result<Vec<ChannelUrl>, InstallFailure> {
    let parsed: Result<Vec<ChannelUrl>, _> = channels
        .iter()
        .map(|c| {
            url::Url::parse(c)
                .map(ChannelUrl::from)
                .map_err(|e| (c.clone(), e))
        })
        .collect();
    parsed.map_err(|(channel, err)| InstallFailure::InstallFailed {
        reason: format!("invalid channel URL {channel:?}: {err}"),
    })
}

fn parse_platform(platform: Option<&str>) -> Result<Platform, InstallFailure> {
    match platform {
        None => Ok(Platform::current()),
        Some(p) => p.parse().map_err(|e| InstallFailure::InstallFailed {
            reason: format!("invalid platform {p:?}: {e}"),
        }),
    }
}

fn parse_specs(
    specs: &[String],
    channel_config: &ChannelConfig,
) -> Result<DependencyMap<PackageName, PixiSpec>, InstallFailure> {
    let mut deps = DependencyMap::default();
    for spec_str in specs {
        let match_spec = MatchSpec::from_str(spec_str, ParseStrictness::Lenient).map_err(|e| {
            InstallFailure::InstallFailed {
                reason: format!("invalid match spec {spec_str:?}: {e}"),
            }
        })?;
        let (name_matcher, nameless) = match_spec.into_nameless();
        let name = name_matcher
            .as_exact()
            .ok_or_else(|| InstallFailure::InstallFailed {
                reason: format!("match spec {spec_str:?} must name a package"),
            })?
            .clone();
        deps.insert(
            name,
            PixiSpec::from_nameless_matchspec(nameless, channel_config),
        );
    }
    Ok(deps)
}

fn default_channel_config() -> ChannelConfig {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    ChannelConfig::default_with_root_dir(cwd)
}

/// Validate a candidate environment name.
///
/// Delegates to `pixi_global::EnvironmentName::from_str` so any name
/// the local CLI accepts also flows through the daemon. Failures are
/// re-wrapped as [`InstallFailure::InvalidEnvName`] with the parse
/// error's text as `reason`, so the wire surface stays self-contained.
pub fn validate_env_name(name: &str) -> Result<(), InstallFailure> {
    use std::str::FromStr;

    pixi_global::EnvironmentName::from_str(name)
        .map(|_| ())
        .map_err(|err| InstallFailure::InvalidEnvName {
            name: name.to_string(),
            reason: err.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `env_hash` must be deterministic: same inputs always give the
    /// same digest. (HMAC-SHA-256 is, but this guards against accidental
    /// non-determinism in our wrapper — e.g. if someone reaches for
    /// `format!` with locale-sensitive output.)
    #[test]
    fn env_hash_is_deterministic() {
        let salt = [0xAB; SALT_LEN];
        let a = env_hash(&salt, Path::new("/home/alice/.pixi/envs"), "foo");
        let b = env_hash(&salt, Path::new("/home/alice/.pixi/envs"), "foo");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "32-byte digest renders as 64 hex chars");
    }

    /// Different env names against the same client must produce different
    /// hashes — that's the whole point of folding the env name into the
    /// hash.
    #[test]
    fn env_hash_distinguishes_env_names() {
        let salt = [0u8; SALT_LEN];
        let dir = Path::new("/home/alice/.pixi/envs");
        assert_ne!(env_hash(&salt, dir, "foo"), env_hash(&salt, dir, "bar"));
    }

    /// Different client directories with the same env name must produce
    /// different hashes — a multi-user host shouldn't collide
    /// `~/.pixi/envs/foo` between accounts.
    #[test]
    fn env_hash_distinguishes_client_directories() {
        let salt = [0u8; SALT_LEN];
        assert_ne!(
            env_hash(&salt, Path::new("/home/alice/.pixi/envs"), "foo"),
            env_hash(&salt, Path::new("/home/bob/.pixi/envs"), "foo"),
        );
    }

    /// Different salts against identical (path, name) inputs must
    /// produce different hashes — operators opting into a non-default
    /// salt should see installs land under different prefix names.
    #[test]
    fn env_hash_is_salt_sensitive() {
        let dir = Path::new("/home/alice/.pixi/envs");
        let zero = env_hash(&[0u8; SALT_LEN], dir, "foo");
        let mut other = [0u8; SALT_LEN];
        other[0] = 1;
        assert_ne!(zero, env_hash(&other, dir, "foo"));
    }

    /// `<dir>/foo` and `<dir>foo` must hash differently. Without the `/`
    /// separator the message would be ambiguous: an attacker who can
    /// pick `auth_path` could otherwise rename a path's tail into the
    /// next component.
    #[test]
    fn env_hash_separator_prevents_collisions() {
        let salt = [0u8; SALT_LEN];
        // (auth_path, env_name) pair A: dir + name with separator.
        let a = env_hash(&salt, Path::new("/home/alice/foo"), "bar");
        // pair B: longer auth_path with a tail that, without a separator,
        // would match pair A's combined message bytes.
        let b = env_hash(&salt, Path::new("/home/alice/foobar"), "");
        assert_ne!(
            a, b,
            "the / separator must keep the (path, name) split unambiguous"
        );
    }

    /// Names accepted by `EnvironmentName::from_str` must also be
    /// accepted by `validate_env_name`. The two implementations are
    /// kept in sync by hand because the daemon crate doesn't depend
    /// on `pixi_global`; this test pins parity for the shapes users
    /// actually type.
    #[test]
    fn validate_env_name_accepts_typical_names() {
        for ok in [
            "foo",
            "foo-bar",
            "foo_bar",
            "f00",
            "a",
            // Dot is part of the EnvironmentName regex (e.g. version-suffixed
            // env names like `python3.11`). Earlier versions of the daemon
            // validator rejected it; we now match local behaviour.
            "python3.11",
            "my.env",
        ] {
            assert!(
                validate_env_name(ok).is_ok(),
                "expected {ok:?} to be accepted"
            );
        }
    }

    #[test]
    fn validate_env_name_rejects_empty() {
        let err = validate_env_name("").unwrap_err();
        assert!(matches!(err, InstallFailure::InvalidEnvName { .. }));
    }

    /// Names that contain bytes outside the `[a-z0-9-_.]+` character
    /// class — same set `EnvironmentName::from_str` rejects — must
    /// fail the daemon validator too.
    #[test]
    fn validate_env_name_rejects_invalid_chars() {
        for bad in [
            "foo/bar",    // path separator
            "../etc",     // path traversal (the `/` makes it bad)
            "with space", // whitespace
            "tab\t",      // control char
            "Foo",        // uppercase — local regex is lowercase-only
            "name@host",  // out-of-class punctuation
        ] {
            let err = validate_env_name(bad).unwrap_err();
            assert!(
                matches!(err, InstallFailure::InvalidEnvName { .. }),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn server_config_accepts_default_zero_salt() {
        let cfg = ServerConfig::from_parts(PathBuf::from("/d"), PathBuf::from("/c"), None).unwrap();
        assert_eq!(cfg.salt, [0u8; SALT_LEN]);
    }

    #[test]
    fn server_config_decodes_hex_salt() {
        let cfg = ServerConfig::from_parts(
            PathBuf::from("/d"),
            PathBuf::from("/c"),
            Some("0123456789abcdef0123456789abcdef"),
        )
        .unwrap();
        assert_eq!(
            cfg.salt,
            [
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef
            ]
        );
    }

    #[test]
    fn server_config_rejects_short_salt() {
        let err = ServerConfig::from_parts(PathBuf::from("/d"), PathBuf::from("/c"), Some("00"))
            .unwrap_err();
        let ServerConfigError::InvalidSalt { reason } = err;
        assert!(
            reason.contains("16 bytes"),
            "reason should name the expected length: {reason}"
        );
    }

    /// `install_lock` returns the same `Arc<Mutex<()>>` for the same
    /// hash and distinct ones for different hashes. Combined with
    /// `try_lock_owned` in `run_install`, that gives the daemon
    /// per-(auth_path, env_name) install-rejection: while one
    /// install holds the guard, a second `try_lock` on the same hash
    /// fails (and surfaces as
    /// [`InstallFailure::DuplicateEnvironment`]); different hashes
    /// proceed independently; once the first guard drops, a
    /// sequential retry of the same install acquires cleanly.
    #[tokio::test(flavor = "current_thread")]
    async fn install_lock_rejects_same_hash() {
        let cfg = ServerConfig::from_parts(PathBuf::from("/d"), PathBuf::from("/c"), None).unwrap();

        let lock_a1 = cfg.install_lock("hash-a");
        let lock_a2 = cfg.install_lock("hash-a");
        assert!(
            Arc::ptr_eq(&lock_a1, &lock_a2),
            "same hash must return the same Arc'd mutex"
        );
        let lock_b = cfg.install_lock("hash-b");
        assert!(
            !Arc::ptr_eq(&lock_a1, &lock_b),
            "different hashes must return distinct Arcs"
        );

        let guard_a = lock_a1.clone().try_lock_owned().unwrap();
        assert!(
            lock_a2.try_lock().is_err(),
            "second try_lock on the same hash must fail while the first guard is held"
        );
        assert!(
            lock_b.try_lock().is_ok(),
            "different-hash lock must remain free"
        );

        drop(guard_a);
        assert!(
            lock_a2.try_lock().is_ok(),
            "lock releases once the first guard drops"
        );
    }

    #[test]
    fn server_config_rejects_non_hex_salt() {
        let err = ServerConfig::from_parts(
            PathBuf::from("/d"),
            PathBuf::from("/c"),
            Some("not-hex-at-all-not-hex-at-all-no"),
        )
        .unwrap_err();
        let ServerConfigError::InvalidSalt { reason } = err;
        assert!(reason.contains("not valid hex"), "{reason}");
    }

    /// `InstallReply::Success` round-trips through serde with a
    /// populated transaction summary covering every variant of
    /// [`InstallChangeWire`]. Pins the JSON-on-the-wire shape so a
    /// future client/server skew doesn't silently drop change kinds.
    #[test]
    fn install_reply_success_round_trips_transaction() {
        let summary = TransactionSummary {
            package_changes: vec![
                PackageChange {
                    name: "foo".into(),
                    change: InstallChangeWire::Installed {
                        version: "1.2.3".into(),
                    },
                },
                PackageChange {
                    name: "bar".into(),
                    change: InstallChangeWire::Upgraded {
                        from: "1.0".into(),
                        to: "2.0".into(),
                    },
                },
                PackageChange {
                    name: "baz".into(),
                    change: InstallChangeWire::TransitiveUpgraded {
                        from: "1.0".into(),
                        to: "1.0".into(),
                    },
                },
                PackageChange {
                    name: "qux".into(),
                    change: InstallChangeWire::Reinstalled {
                        from: "1.0".into(),
                        to: "1.0".into(),
                    },
                },
                PackageChange {
                    name: "removed".into(),
                    change: InstallChangeWire::Removed,
                },
            ],
        };
        let original = crate::InstallReply::Success {
            prefix: "/srv/data/abcdef".into(),
            transaction: summary,
        };
        let bytes = serde_json::to_vec(&original).unwrap();
        let parsed: crate::InstallReply = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(original, parsed, "every variant must round-trip");
    }

    /// `InstallReply::Success` with an empty `TransactionSummary`
    /// elides the field on the wire entirely, so older clients (or
    /// the streaming path's daemon-side fingerprint short-circuit)
    /// don't pay the bytes for "nothing changed."
    #[test]
    fn install_reply_success_omits_empty_transaction() {
        let original = crate::InstallReply::Success {
            prefix: "/srv/data/abcdef".into(),
            transaction: TransactionSummary::default(),
        };
        let json: serde_json::Value = serde_json::to_value(&original).unwrap();
        let obj = json.as_object().expect("expected JSON object");
        assert!(
            !obj.contains_key("transaction"),
            "empty transaction should be skipped, got {obj:?}"
        );
        // And it still parses back as the canonical default.
        let bytes = serde_json::to_vec(&original).unwrap();
        let parsed: crate::InstallReply = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(original, parsed);
    }

    /// `refuse_source_dependencies` accepts a binary-only dep map
    /// and rejects any source-typed entry. The daemon never has the
    /// build infrastructure to honour source specs, so this is the
    /// last line of defense against a non-conforming client.
    #[test]
    fn refuse_source_dependencies_accepts_binary_only() {
        let mut deps: DependencyMap<PackageName, PixiSpec> = DependencyMap::default();
        deps.insert(
            PackageName::new_unchecked("foo"),
            PixiSpec::Version(rattler_conda_types::VersionSpec::Any),
        );
        assert!(refuse_source_dependencies(&deps).is_ok());
    }

    #[test]
    fn refuse_source_dependencies_rejects_url_source() {
        use pixi_spec::UrlSpec;
        let mut deps: DependencyMap<PackageName, PixiSpec> = DependencyMap::default();
        deps.insert(
            PackageName::new_unchecked("foo"),
            PixiSpec::Url(UrlSpec {
                url: url::Url::parse("https://example.com/foo.tar.gz").unwrap(),
                md5: None,
                sha256: None,
                subdirectory: Default::default(),
            }),
        );
        let err = refuse_source_dependencies(&deps).unwrap_err();
        match err {
            InstallFailure::UnsupportedSourceSpec { packages } => {
                assert_eq!(packages, vec!["foo".to_string()]);
            }
            other => panic!("expected UnsupportedSourceSpec, got {other:?}"),
        }
    }

    /// `InstallFailure::UnsupportedSourceSpec` round-trips through
    /// serde so the client can match on the typed variant rather
    /// than parsing a free-form `InstallFailed` string.
    #[test]
    fn unsupported_source_spec_round_trips() {
        let original = InstallFailure::UnsupportedSourceSpec {
            packages: vec!["foo".to_string(), "bar".to_string()],
        };
        let bytes = serde_json::to_vec(&original).unwrap();
        let parsed: InstallFailure = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(original, parsed);
    }

    /// `ExtraRecord` and the `extra_records` field on `InstallRequest`
    /// round-trip through serde without losing any bytes. The
    /// daemon parses `record_json` back into a real
    /// `RepoDataRecord`; this test pins the wire shape itself.
    #[test]
    fn install_request_extra_records_round_trip() {
        let original = InstallRequest {
            env_name: "foo".to_string(),
            specs: vec!["bar".to_string()],
            channels: vec!["https://example.com/conda-forge".to_string()],
            platform: None,
            force_reinstall: false,
            extra_records: vec![ExtraRecord {
                artifact_path: "/cache/source-build/foo-1.0-h0_0.conda".to_string(),
                record_json: r#"{"name":"foo","version":"1.0"}"#.to_string(),
            }],
        };
        let bytes = serde_json::to_vec(&original).unwrap();
        let parsed: InstallRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.extra_records.len(), 1);
        assert_eq!(
            parsed.extra_records[0].artifact_path,
            original.extra_records[0].artifact_path
        );
        assert_eq!(
            parsed.extra_records[0].record_json,
            original.extra_records[0].record_json
        );
    }

    /// An empty `extra_records` is elided on the wire so existing
    /// callers that don't ship source builds pay no extra bytes.
    #[test]
    fn install_request_omits_empty_extra_records() {
        let original = InstallRequest {
            env_name: "foo".to_string(),
            specs: vec![],
            channels: vec![],
            platform: None,
            force_reinstall: false,
            extra_records: vec![],
        };
        let json: serde_json::Value = serde_json::to_value(&original).unwrap();
        let obj = json.as_object().expect("expected JSON object");
        assert!(
            !obj.contains_key("extra_records"),
            "empty extra_records should be skipped, got {obj:?}"
        );
    }

    /// `ingest_extra_records` rejects relative artefact paths
    /// upfront. The daemon resolves the path verbatim against its
    /// own filesystem, so an attacker-controllable relative path
    /// would resolve against the daemon's cwd — refuse with a
    /// structured error rather than silently picking up an
    /// unintended file.
    #[tokio::test(flavor = "current_thread")]
    async fn ingest_extra_records_rejects_relative_paths() {
        let extras = vec![ExtraRecord {
            artifact_path: "relative/path.conda".to_string(),
            record_json: r#"{"name":"foo","version":"1.0"}"#.to_string(),
        }];
        let cache_root = std::env::temp_dir();
        let err = ingest_extra_records(&extras, &cache_root)
            .await
            .unwrap_err();
        match err {
            InstallFailure::InstallFailed { reason } => {
                assert!(
                    reason.contains("absolute"),
                    "error should mention 'absolute', got {reason:?}"
                );
            }
            other => panic!("expected InstallFailed, got {other:?}"),
        }
    }

    /// Every `UninstallFailure` variant survives a serde round-trip
    /// through the wire form. Pins the variant set so a future tag
    /// rename or field addition surfaces here before it ships.
    #[test]
    fn uninstall_failure_round_trips_every_variant() {
        for original in [
            UninstallFailure::NotAuthenticated,
            UninstallFailure::ServerNotConfigured {
                hint: "pass --data".to_string(),
            },
            UninstallFailure::InvalidEnvName {
                name: "foo!".to_string(),
                reason: "bad char".to_string(),
            },
            UninstallFailure::EnvNotFound {
                env_name: "foo".to_string(),
            },
            UninstallFailure::RemoveFailed {
                reason: "EACCES".to_string(),
            },
        ] {
            let bytes = serde_json::to_vec(&original).unwrap();
            let parsed: UninstallFailure = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(original, parsed);
        }
    }

    /// `run_uninstall` returns `EnvNotFound` when the prefix
    /// directory doesn't exist. Surfaced as a typed variant so
    /// idempotent client-side handling (treat-as-success) can
    /// pattern-match without parsing a free-form reason string.
    #[tokio::test(flavor = "current_thread")]
    async fn run_uninstall_reports_not_found_when_prefix_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = ServerConfig::from_parts(tmp.path().join("data"), tmp.path().join("cache"), None)
            .unwrap();
        fs_err::create_dir_all(&cfg.data).unwrap();
        let request = UninstallRequest {
            env_name: "ghost".to_string(),
        };
        let err = run_uninstall(&cfg, Path::new("/home/alice/.pixi/envs"), &request)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            UninstallFailure::EnvNotFound {
                env_name: "ghost".to_string()
            }
        );
    }

    /// `run_uninstall` removes the per-HASH prefix when it exists,
    /// and the data dir survives (only the prefix subtree goes).
    #[tokio::test(flavor = "current_thread")]
    async fn run_uninstall_removes_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = ServerConfig::from_parts(tmp.path().join("data"), tmp.path().join("cache"), None)
            .unwrap();
        fs_err::create_dir_all(&cfg.data).unwrap();
        let auth_path = Path::new("/home/alice/.pixi/envs");
        let env_name = "foo";
        let hash = env_hash(&cfg.salt, auth_path, env_name);
        let prefix = cfg.data.join(&hash);
        // Plant a prefix with one file in it.
        fs_err::create_dir_all(prefix.join("conda-meta")).unwrap();
        fs_err::write(prefix.join("conda-meta").join("marker"), b"hi").unwrap();
        let request = UninstallRequest {
            env_name: env_name.to_string(),
        };
        run_uninstall(&cfg, auth_path, &request).await.unwrap();
        assert!(!prefix.exists(), "prefix should be removed");
        assert!(cfg.data.exists(), "data dir should survive");
    }
}
