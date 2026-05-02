//! `Install` RPC: serve a `pixi global install` request from a daemon
//! that owns a shared `data` and `cache` directory.
//!
//! Step 1 scope: the server validates inputs and returns the on-disk
//! prefix path it *would* materialise (`<data>/<HASH>/`, where `HASH` is
//! HMAC-SHA-256 of the authenticated client directory + the requested
//! env name, keyed by a server-side salt). No solve, fetch, or install
//! happens yet; that lands in step 2. Step 7 will add streaming progress
//! events alongside the terminal `Result`.

use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac};
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
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Root under which `<HASH>/` install prefixes live.
    pub data: PathBuf,
    /// Shared cache root for repodata, package archives, source-build
    /// artifacts. Not used in step 1 (no install runs yet); reserved for
    /// step 2.
    pub cache: PathBuf,
    /// HMAC key folded into the env hash. Defaults to all zeros when
    /// the operator doesn't set `--salt` / `remote.salt`.
    pub salt: [u8; SALT_LEN],
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
        Ok(Self { data, cache, salt })
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
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ExposeMapping {
    /// Name to surface as `~/.pixi/bin/<exe_name>`.
    pub exe_name: String,
    /// `<package>/<binary>` reference into the installed env.
    pub source: String,
}

/// One streaming reply from `Install`.
///
/// `Install` is declared `#[zlink(more)]` from day one so progress
/// events can land in step 7 without a wire-incompatible schema
/// change. Step 1 emits exactly one terminal reply (either
/// [`Success`](Self::Success) or [`Failed`](Self::Failed)).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InstallReply {
    /// Streaming progress event. Not emitted in step 1.
    Progress {
        /// The notification.
        event: crate::ProgressEvent,
    },
    /// Terminal success. Final reply in the stream.
    Success {
        /// Absolute server-side install prefix path the client should
        /// localise (symlink to in step 4 / 6).
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
    /// `remote.data` / `remote.cache`); the install RPC has nowhere to
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
    /// Catch-all for install failures from step 2 onwards. Step 1 never
    /// produces this variant.
    InstallFailed {
        /// Free-form reason.
        reason: String,
    },
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
