//! `pixi serve` — run the pixi varlink IPC server.
//!
//! The listening socket is selected as follows:
//!
//!   1. The global `--socket <PATH>` flag binds that path explicitly.
//!   2. Otherwise the configuration decides: `remote.socket-activation = true`
//!      adopts the systemd-passed socket, any other value binds
//!      `remote.socket`.
//!   3. If neither a CLI socket nor a configured socket nor explicit
//!      activation is set, the command exits with an error.
//!
//! The configuration file is expected to set `remote.socket` whenever the
//! `[remote]` section is present.
//!
//! `--data` and `--cache` (or the matching `serve.data` / `serve.cache`
//! config keys) enable the `Install` RPC: when both resolve to a path,
//! the daemon takes exclusive `flock`s on `<data>/.pixi-serve.lock` and
//! `<cache>/.pixi-serve.lock` and starts in install-capable mode. With
//! only one of the two set the command refuses to start. `--salt`
//! (32-hex-char) folds operator-supplied entropy into the per-(client,
//! env) hash that names install prefixes; the default is a 16-byte
//! all-zero key.

use std::path::{Path, PathBuf};

use clap::Parser;
use miette::{IntoDiagnostic, miette};
use pixi_config::Config;

use crate::GlobalOptions;

/// Run the pixi varlink IPC server.
#[derive(Parser, Debug)]
pub struct Args {
    /// Root directory under which `pixi serve` materialises per-environment
    /// install prefixes. Required (along with `--cache`) to enable the
    /// `Install` RPC. Overrides `serve.data` from config.
    #[arg(long, value_name = "PATH")]
    pub data: Option<PathBuf>,

    /// Root directory the daemon uses for shared package / repodata /
    /// source-build caches. Required (along with `--data`) to enable the
    /// `Install` RPC. Overrides `serve.cache` from config.
    #[arg(long, value_name = "PATH")]
    pub cache: Option<PathBuf>,

    /// Hex-encoded 16-byte HMAC key used to derive per-environment
    /// install-prefix names. Optional; defaults to a 16-byte all-zero
    /// key. Overrides `serve.salt` from config.
    #[arg(long, value_name = "HEX")]
    pub salt: Option<String>,
}

#[derive(Debug)]
enum Mode {
    Activation,
    Bind(PathBuf),
}

fn select_mode(cli_socket: Option<&Path>, config: &Config) -> miette::Result<Mode> {
    if let Some(path) = cli_socket {
        return Ok(Mode::Bind(path.to_path_buf()));
    }
    if config.remote.socket_activation == Some(true) {
        return Ok(Mode::Activation);
    }
    if let Some(path) = &config.remote.socket {
        return Ok(Mode::Bind(path.clone()));
    }
    Err(miette!(
        help = "pass --socket <PATH>, set `remote.socket` in pixi config, or set `remote.socket-activation = true` to adopt a systemd-passed socket",
        "pixi serve: no socket configured"
    ))
}

/// Resolve the install-related settings.
///
/// Returns `Ok(None)` when neither `data` nor `cache` is set anywhere
/// (echo-only mode), `Ok(Some(config))` when both are set, and an
/// error when exactly one is set — that mismatch is almost always a
/// misconfiguration and silently falling back to echo-only mode would
/// hide it.
fn select_install_config(
    args: &Args,
    config: &Config,
) -> miette::Result<Option<pixi_varlink::ServerConfig>> {
    let data = args.data.clone().or_else(|| config.serve.data.clone());
    let cache = args.cache.clone().or_else(|| config.serve.cache.clone());
    let salt = args.salt.clone().or_else(|| config.serve.salt.clone());

    match (data, cache) {
        (None, None) => Ok(None),
        (Some(_), None) => Err(miette!(
            help = "pass --cache <PATH> or set `serve.cache` in pixi config",
            "pixi serve: --data / `serve.data` is set without --cache / `serve.cache`",
        )),
        (None, Some(_)) => Err(miette!(
            help = "pass --data <PATH> or set `serve.data` in pixi config",
            "pixi serve: --cache / `serve.cache` is set without --data / `serve.data`",
        )),
        (Some(data), Some(cache)) => {
            let cfg = pixi_varlink::ServerConfig::from_parts(data, cache, salt.as_deref())
                .map_err(|e| miette!("pixi serve: {e}"))?;
            Ok(Some(cfg))
        }
    }
}

#[tracing::instrument(level = "info", name = "pixi.serve", skip_all)]
pub async fn execute(args: Args, global_options: &GlobalOptions) -> miette::Result<()> {
    let config = Config::load_global();
    let mode = select_mode(global_options.socket.as_deref(), &config)?;
    let install_config = select_install_config(&args, &config)?;
    tracing::debug!(
        ?mode,
        install_capable = install_config.is_some(),
        "pixi serve resolved listening mode"
    );

    match (mode, install_config) {
        (Mode::Bind(path), Some(cfg)) => {
            tracing::info!(path = %path.display(), "pixi serve: binding socket (install-capable)");
            pixi_varlink::serve_with_install(path, cfg)
                .await
                .into_diagnostic()
        }
        (Mode::Bind(path), None) => {
            tracing::info!(path = %path.display(), "pixi serve: binding socket (echo only)");
            pixi_varlink::serve(path).await.into_diagnostic()
        }
        (Mode::Activation, install_config) => {
            let listener = pixi_varlink::take_socket_activation_listener()
                .into_diagnostic()?
                .ok_or_else(|| {
                    miette!(
                        help = "set `remote.socket` in pixi config, pass --socket <PATH>, or start pixi via systemd with LISTEN_PID and LISTEN_FDS set",
                        "pixi serve: no socket provided and no inherited socket from systemd"
                    )
                })?;
            tracing::info!(
                install_capable = install_config.is_some(),
                "pixi serve: using systemd socket activation"
            );
            // `serve_on` doesn't support install-capable mode (the
            // listener is already bound, so we can't sequence the
            // flock acquisition before the bind). Activation +
            // install isn't a real-world combination today; fail
            // closed with a clear hint rather than silently starting
            // in echo-only mode.
            if install_config.is_some() {
                return Err(miette!(
                    help = "to enable the Install RPC, run `pixi serve` directly with --data and --cache instead of via socket activation",
                    "pixi serve: --data / --cache is not supported with socket activation"
                ));
            }
            pixi_varlink::serve_on(listener).await.into_diagnostic()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixi_config::{RemoteConfig, ServeConfig};
    use std::path::PathBuf;

    fn empty_args() -> Args {
        Args {
            data: None,
            cache: None,
            salt: None,
        }
    }

    #[test]
    fn cli_socket_wins_over_config_activation() {
        let config = Config {
            remote: RemoteConfig {
                socket_activation: Some(true),
                ..RemoteConfig::default()
            },
            ..Config::default()
        };
        let cli = PathBuf::from("/from/cli.sock");
        match select_mode(Some(&cli), &config).unwrap() {
            Mode::Bind(p) => assert_eq!(p, cli),
            Mode::Activation => panic!("expected Bind"),
        }
    }

    #[test]
    fn config_activation_true_forces_activation() {
        let config = Config {
            remote: RemoteConfig {
                socket: Some(PathBuf::from("/ignored.sock")),
                socket_activation: Some(true),
                ..RemoteConfig::default()
            },
            ..Config::default()
        };
        assert!(matches!(
            select_mode(None, &config).unwrap(),
            Mode::Activation
        ));
    }

    #[test]
    fn config_activation_false_binds_config_socket() {
        let config = Config {
            remote: RemoteConfig {
                socket: Some(PathBuf::from("/from/config.sock")),
                socket_activation: Some(false),
                ..RemoteConfig::default()
            },
            ..Config::default()
        };
        match select_mode(None, &config).unwrap() {
            Mode::Bind(p) => assert_eq!(p, PathBuf::from("/from/config.sock")),
            Mode::Activation => panic!("expected Bind"),
        }
    }

    #[test]
    fn config_socket_only_binds_it() {
        let config = Config {
            remote: RemoteConfig {
                socket: Some(PathBuf::from("/from/config.sock")),
                ..RemoteConfig::default()
            },
            ..Config::default()
        };
        match select_mode(None, &config).unwrap() {
            Mode::Bind(p) => assert_eq!(p, PathBuf::from("/from/config.sock")),
            Mode::Activation => panic!("expected Bind"),
        }
    }

    #[test]
    fn no_socket_anywhere_is_an_error() {
        let err = select_mode(None, &Config::default()).unwrap_err();
        assert!(err.to_string().contains("no socket configured"));
    }

    #[test]
    fn neither_data_nor_cache_set_is_echo_only() {
        assert!(
            select_install_config(&empty_args(), &Config::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn data_without_cache_is_an_error() {
        let args = Args {
            data: Some(PathBuf::from("/d")),
            ..empty_args()
        };
        let err = select_install_config(&args, &Config::default()).unwrap_err();
        assert!(err.to_string().contains("--data"), "{err}");
    }

    #[test]
    fn cache_without_data_is_an_error() {
        let args = Args {
            cache: Some(PathBuf::from("/c")),
            ..empty_args()
        };
        let err = select_install_config(&args, &Config::default()).unwrap_err();
        assert!(err.to_string().contains("--cache"), "{err}");
    }

    #[test]
    fn cli_data_wins_over_config_data() {
        let config = Config {
            serve: ServeConfig {
                data: Some(PathBuf::from("/from/config")),
                cache: Some(PathBuf::from("/from/config-cache")),
                salt: None,
            },
            ..Config::default()
        };
        let args = Args {
            data: Some(PathBuf::from("/from/cli")),
            ..empty_args()
        };
        let cfg = select_install_config(&args, &config).unwrap().unwrap();
        assert_eq!(cfg.data, PathBuf::from("/from/cli"));
        assert_eq!(cfg.cache, PathBuf::from("/from/config-cache"));
    }
}
