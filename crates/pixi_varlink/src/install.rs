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
use std::str::FromStr;
use std::sync::Arc;

use hmac::{Hmac, Mac};
use itertools::Itertools;
use ordermap::OrderMap;
use pixi_command_dispatcher::{
    BuildEnvironment, CacheDirs, CommandDispatcher, EnvironmentRef, EnvironmentSpec, EphemeralEnv,
    InstallPixiEnvironmentSpec,
    keys::{SolvePixiEnvironmentKey, SolvePixiEnvironmentSpec},
};
use pixi_global::{
    ExposedName,
    trampoline::{Configuration, Trampoline},
};
use pixi_path::AbsPathBuf;
use pixi_spec::PixiSpec;
use pixi_spec_containers::DependencyMap;
use rattler_conda_types::{
    ChannelConfig, ChannelUrl, MatchSpec, PackageName, ParseStrictness, Platform, prefix::Prefix,
};
use rattler_shell::activation::prefix_path_entries;
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

    /// Trampoline mappings the client wants exposed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose: Vec<ExposeMapping>,

    /// Force a rebuild + reinstall of every package, ignoring the
    /// fingerprint short-circuit.
    #[serde(default, skip_serializing_if = "is_false")]
    pub force_reinstall: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One requested trampoline mapping.
///
/// The trampoline binary lands at
/// `<prefix>/.trampoline/<exe_name>` (with a `.exe` suffix on
/// Windows) and its JSON at
/// `<prefix>/.trampoline/trampoline_configuration/<exe_name>.json` —
/// both deterministic from `prefix` (returned in
/// [`InstallReply::Success`]) and `exe_name`, so the client can
/// derive both paths without the server returning them. The client
/// only needs to symlink `~/.pixi/bin/<exe_name>` to the binary;
/// invoking it through that symlink resolves `current_exe()` back
/// to the server side, and the trampoline reads its sibling JSON
/// directly.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ExposeMapping {
    /// Name to surface as `~/.pixi/bin/<exe_name>`.
    pub exe_name: String,
    /// `<package>/<binary>` reference into the installed env. The
    /// daemon resolves the binary to `<server-prefix>/bin/<binary>`
    /// (the package name is informational).
    pub source: String,
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
        /// localise (symlink to in step 4 / 6). Trampoline locations
        /// for each [`ExposeMapping`] are derivable: see
        /// [`ExposeMapping`]'s docs.
        prefix: String,
    },
    /// Terminal failure. Final reply in the stream.
    Failed {
        /// Why the install was rejected or could not proceed.
        error: InstallFailure,
    },
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
    /// Catch-all for install failures: solve errors, malformed
    /// match-specs, dispatcher errors, prefix I/O failures.
    InstallFailed {
        /// Free-form reason; safe to surface verbatim to the user.
        reason: String,
    },
}

/// Drive a real `Install` request end-to-end: build a dispatcher
/// rooted at the server's `cache` dir, solve the requested specs into
/// records, lay them down in `<data>/<HASH>/`, and write trampolines
/// for any [`ExposeMapping`]s the client requested. Persists the
/// fingerprint marker so subsequent identical requests short-circuit.
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
) -> Result<PathBuf, InstallFailure> {
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

    fs_err::create_dir_all(&prefix_path).map_err(|e| InstallFailure::InstallFailed {
        reason: format!("could not create prefix {}: {e}", prefix_path.display()),
    })?;
    let prefix = Prefix::create(&prefix_path).map_err(|e| InstallFailure::InstallFailed {
        reason: format!("could not initialise prefix {}: {e}", prefix_path.display()),
    })?;

    let install_spec = InstallPixiEnvironmentSpec {
        name: request.env_name.clone(),
        records: records_arc.iter().cloned().map(Into::into).collect(),
        prefix,
        installed: None,
        ignore_packages: None,
        build_environment,
        force_reinstall: if request.force_reinstall {
            records_arc.iter().map(|r| r.name().clone()).collect()
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

    build_trampolines(&prefix_path, auth_path, request).await?;

    Ok(prefix_path)
}

/// Generate trampolines for every `request.expose` entry by activating
/// the server prefix and rewriting paths to the client's perspective.
///
/// **Key asymmetry.** The trampoline binaries and their JSON
/// configurations live on the server (`<server-prefix>/.trampoline/`),
/// but the JSONs reference *client-side* paths: `exe` and `CONDA_PREFIX`
/// point at `<auth_path>/<env_name>/...`, where the client will
/// eventually symlink the prefix. When the trampoline runs (invoked
/// via the client's `~/.pixi/bin/<exe>` symlink), it canonicalises
/// `current_exe()` back to the server side, reads its sibling JSON,
/// and execs the configured binary — which lives on the server too,
/// reachable through the client's prefix symlink. So everything ends
/// up on the server at runtime, but the *user-visible* paths stay
/// rooted at the client's `~/.pixi/envs/<env_name>` for activation
/// and shell display.
async fn build_trampolines(
    server_prefix: &Path,
    auth_path: &Path,
    request: &InstallRequest,
) -> Result<(), InstallFailure> {
    if request.expose.is_empty() {
        return Ok(());
    }

    // The client will eventually symlink this directory at
    // `<auth_path>/<env_name>` → `<server_prefix>`; we synthesise the
    // client-side equivalent of every server-side path that ends up in
    // a trampoline JSON.
    let client_prefix = auth_path.join(&request.env_name);

    let prefix = pixi_utils::prefix::Prefix::new(server_prefix.to_path_buf());
    let path_current = std::env::var("PATH").unwrap_or_default();
    let mut activation =
        prefix
            .run_activation()
            .await
            .map_err(|e| InstallFailure::InstallFailed {
                reason: format!("activation failed for {}: {e}", server_prefix.display()),
            })?;
    let path_after = activation
        .remove("PATH")
        .or_else(|| activation.remove("Path"))
        .unwrap_or_else(|| path_current.clone());
    let server_path_diff = compute_path_diff(&path_current, &path_after, server_prefix)?;

    // Rewrite every server-prefix occurrence in the activation env so
    // the trampoline-run subshell sees client-side paths (matters for
    // CONDA_PREFIX in particular, which the user may inspect).
    let server_prefix_str =
        server_prefix
            .to_str()
            .ok_or_else(|| InstallFailure::InstallFailed {
                reason: format!(
                    "server prefix path is not valid UTF-8: {}",
                    server_prefix.display()
                ),
            })?;
    let client_prefix_str =
        client_prefix
            .to_str()
            .ok_or_else(|| InstallFailure::InstallFailed {
                reason: format!(
                    "client prefix path is not valid UTF-8: {}",
                    client_prefix.display()
                ),
            })?;
    let rewrite = |s: &str| s.replace(server_prefix_str, client_prefix_str);
    let env_for_trampoline: HashMap<String, String> = activation
        .iter()
        .map(|(k, v)| (k.clone(), rewrite(v)))
        .collect();
    let path_diff = rewrite(&server_path_diff);

    let trampoline_root = server_prefix.join(".trampoline");
    for mapping in &request.expose {
        let binary = parse_expose_source(&mapping.source)?;
        let exposed = ExposedName::from_str(&mapping.exe_name).map_err(|e| {
            InstallFailure::InstallFailed {
                reason: format!("invalid exposed name {:?}: {e}", mapping.exe_name),
            }
        })?;
        let configuration = Configuration::new(
            client_prefix.join("bin").join(&binary),
            path_diff.clone(),
            env_for_trampoline.clone(),
        );
        Trampoline::new(exposed, trampoline_root.clone(), configuration)
            .save()
            .await
            .map_err(|e| InstallFailure::InstallFailed {
                reason: format!("failed to write trampoline {:?}: {e}", mapping.exe_name),
            })?;
    }
    Ok(())
}

/// Parse the [`ExposeMapping::source`] format and return the binary
/// name. Accepts both `<package>/<binary>` (the package half is
/// informational, kept for compatibility) and a bare `<binary>`. The
/// last `/`-separated segment is taken as the binary name; deeper
/// relative paths (`dotnet/dotnet/dotnet`-style multi-component
/// layouts) aren't supported by the daemon path yet — users with that
/// need run the local install instead.
fn parse_expose_source(source: &str) -> Result<String, InstallFailure> {
    let binary = source.rsplit('/').next().unwrap_or(source);
    if binary.is_empty() {
        return Err(InstallFailure::InstallFailed {
            reason: format!("expose source {source:?} doesn't name a binary"),
        });
    }
    Ok(binary.to_string())
}

/// Replicates `pixi_global::install::path_diff` (which is
/// `pub(crate)`): everything `path_after` adds over `path_before`,
/// plus the prefix's well-known PATH entries even if they happened to
/// be in `path_before`. Joined with the platform PATH separator.
fn compute_path_diff(
    path_before: &str,
    path_after: &str,
    prefix: &Path,
) -> Result<String, InstallFailure> {
    let paths_before: Vec<PathBuf> = std::env::split_paths(path_before).collect();
    let paths_after: Vec<PathBuf> = std::env::split_paths(path_after).collect();
    let prefix_entries = prefix_path_entries(prefix, &Platform::current());
    let diff = paths_after
        .iter()
        .filter(|p| !paths_before.contains(p) || prefix_entries.contains(p))
        .unique();
    std::env::join_paths(diff)
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| InstallFailure::InstallFailed {
            reason: format!("could not join PATH entries: {e}"),
        })
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
/// Mirrors the conservative subset `pixi_global::EnvironmentName`
/// accepts: ASCII alphanumeric plus `-` and `_`, non-empty, length
/// capped to keep generated paths well below `PATH_MAX`. Any deviation
/// from this shape is `InvalidEnvName`.
pub fn validate_env_name(name: &str) -> Result<(), InstallFailure> {
    /// Cap kept conservative; `EnvironmentName` upstream is untyped
    /// `String`, and 64 bytes leaves room for the `<HASH>/` parent
    /// without bumping into Windows' `MAX_PATH` for paths a localising
    /// client may eventually concatenate.
    const MAX_LEN: usize = 64;
    if name.is_empty() {
        return Err(InstallFailure::InvalidEnvName {
            name: name.to_string(),
            reason: "empty".to_string(),
        });
    }
    if name.len() > MAX_LEN {
        return Err(InstallFailure::InvalidEnvName {
            name: name.to_string(),
            reason: format!("longer than {MAX_LEN} bytes"),
        });
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-'))
    {
        return Err(InstallFailure::InvalidEnvName {
            name: name.to_string(),
            reason: format!("contains invalid character {bad:?}"),
        });
    }
    Ok(())
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

    #[test]
    fn validate_env_name_accepts_typical_names() {
        for ok in ["foo", "foo-bar", "foo_bar", "Foo", "f00", "a"] {
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

    #[test]
    fn validate_env_name_rejects_path_separators() {
        for bad in ["foo/bar", "../etc", ".", "..", "with space", "tab\t"] {
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
}
