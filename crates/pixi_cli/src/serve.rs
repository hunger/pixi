//! `pixi serve` — run the pixi varlink IPC server.
//!
//! The listening socket is selected as follows:
//!
//!   1. `--socket <PATH>` on the CLI binds that path explicitly.
//!   2. Otherwise the configuration decides: `remote.socket-activation = true`
//!      adopts the systemd-passed socket, any other value binds
//!      `remote.socket`.
//!   3. If neither a CLI socket nor a configured socket nor explicit
//!      activation is set, the command exits with an error.
//!
//! The configuration file is expected to set `remote.socket` whenever the
//! `[remote]` section is present.

use std::path::PathBuf;

use clap::Parser;
use miette::{IntoDiagnostic, miette};
use pixi_config::Config;

/// Run the pixi varlink IPC server.
#[derive(Parser, Debug)]
pub struct Args {
    /// Path of the Unix domain socket to bind. Overrides the configuration
    /// and disables socket activation. When omitted, `remote.socket` and
    /// `remote.socket-activation` from the configuration are used.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

#[derive(Debug)]
enum Mode {
    Activation,
    Bind(PathBuf),
}

fn select_mode(args: &Args, config: &Config) -> miette::Result<Mode> {
    if let Some(path) = &args.socket {
        return Ok(Mode::Bind(path.clone()));
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

pub async fn execute(args: Args) -> miette::Result<()> {
    match select_mode(&args, &Config::load_global())? {
        Mode::Bind(path) => {
            tracing::info!("pixi serve: binding {}", path.display());
            pixi_varlink::serve(path).await.into_diagnostic()
        }
        Mode::Activation => {
            let listener = pixi_varlink::take_socket_activation_listener()
                .into_diagnostic()?
                .ok_or_else(|| {
                    miette!(
                        help = "set `remote.socket` in pixi config, pass --socket <PATH>, or start pixi via systemd with LISTEN_PID and LISTEN_FDS set",
                        "pixi serve: no socket provided and no inherited socket from systemd"
                    )
                })?;
            tracing::info!("pixi serve: using systemd socket activation");
            pixi_varlink::serve_on(listener).await.into_diagnostic()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixi_config::RemoteConfig;
    use std::path::PathBuf;

    fn args() -> Args {
        Args { socket: None }
    }

    #[test]
    fn cli_socket_wins_over_config_activation() {
        let config = Config {
            remote: RemoteConfig {
                socket: None,
                socket_activation: Some(true),
            },
            ..Config::default()
        };
        let mut a = args();
        a.socket = Some(PathBuf::from("/from/cli.sock"));
        match select_mode(&a, &config).unwrap() {
            Mode::Bind(p) => assert_eq!(p, PathBuf::from("/from/cli.sock")),
            Mode::Activation => panic!("expected Bind"),
        }
    }

    #[test]
    fn config_activation_true_forces_activation() {
        let config = Config {
            remote: RemoteConfig {
                socket: Some(PathBuf::from("/ignored.sock")),
                socket_activation: Some(true),
            },
            ..Config::default()
        };
        assert!(matches!(
            select_mode(&args(), &config).unwrap(),
            Mode::Activation
        ));
    }

    #[test]
    fn config_activation_false_binds_config_socket() {
        let config = Config {
            remote: RemoteConfig {
                socket: Some(PathBuf::from("/from/config.sock")),
                socket_activation: Some(false),
            },
            ..Config::default()
        };
        match select_mode(&args(), &config).unwrap() {
            Mode::Bind(p) => assert_eq!(p, PathBuf::from("/from/config.sock")),
            Mode::Activation => panic!("expected Bind"),
        }
    }

    #[test]
    fn config_socket_only_binds_it() {
        let config = Config {
            remote: RemoteConfig {
                socket: Some(PathBuf::from("/from/config.sock")),
                socket_activation: None,
            },
            ..Config::default()
        };
        match select_mode(&args(), &config).unwrap() {
            Mode::Bind(p) => assert_eq!(p, PathBuf::from("/from/config.sock")),
            Mode::Activation => panic!("expected Bind"),
        }
    }

    #[test]
    fn no_socket_anywhere_is_an_error() {
        let err = select_mode(&args(), &Config::default()).unwrap_err();
        assert!(err.to_string().contains("no socket configured"));
    }
}
