use std::io::Write;
use std::path::PathBuf;

use clap::Parser;
use indicatif::ProgressBar;
use itertools::Itertools;
use miette::IntoDiagnostic;
use pixi_global::list::format_asciiart_section;
use pixi_reporters::main_progress_bar::MainProgressBar;

/// Interact with a remote pixi varlink server
#[derive(Parser, Debug)]
pub struct Args {
    /// Varlink server address (socket path or unix:/path or tcp:host:port).
    /// Required for client commands (version, info, global). Not used by `serve`.
    #[arg(long, env = "PIXI_REMOTE")]
    pub address: Option<String>,

    #[command(subcommand)]
    pub command: RemoteCommand,
}

#[derive(Parser, Debug)]
pub enum RemoteCommand {
    /// Start a varlink IPC server for programmatic access to pixi
    Serve(ServeArgs),
    /// Information about the system, workspace and environments for the current machine
    Info(crate::info::Args),
    /// Subcommand for global package management via the remote server
    Global(RemoteGlobalCommand),
    /// Print the remote server's pixi version
    Version,
}

/// Start a varlink IPC server
#[derive(Parser, Debug)]
pub struct ServeArgs {
    /// Cache directory for rattler/conda packages
    #[arg(long, env = "PIXI_CACHE_DIR", default_value = "~/.cache/rattler")]
    pub cache_dir: PathBuf,

    /// Directory for storing environments (becomes PIXI_HOME)
    #[arg(long, env = "PIXI_ENVS_DIR", default_value = "~/.local/share/pixi")]
    pub envs_dir: PathBuf,
}

fn expand_tilde(path: &PathBuf) -> miette::Result<PathBuf> {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME")
            .map_err(|_| miette::miette!("HOME not set, cannot expand ~ in {s}"))?;
        Ok(PathBuf::from(home).join(rest))
    } else if s == "~" {
        let home = std::env::var("HOME")
            .map_err(|_| miette::miette!("HOME not set, cannot expand ~ in {s}"))?;
        Ok(PathBuf::from(home))
    } else {
        Ok(path.clone())
    }
}

#[derive(Parser, Debug)]
pub struct RemoteGlobalCommand {
    #[command(subcommand)]
    pub command: RemoteGlobalSubCommand,
}

#[derive(Parser, Debug)]
pub enum RemoteGlobalSubCommand {
    /// Install packages globally on the remote server
    Install(crate::global::install::Args),
}

pub async fn execute(args: Args) -> miette::Result<()> {
    match args.command {
        RemoteCommand::Serve(serve_args) => execute_serve(args.address, serve_args).await,
        cmd => {
            let address = args
                .address
                .ok_or_else(|| miette::miette!("--address is required for this command"))?;
            let address = pixi_varlink::client::normalize_address(&address);
            match cmd {
                RemoteCommand::Info(info_args) => crate::info::execute(info_args).await,
                RemoteCommand::Global(global) => execute_global(&address, global).await,
                RemoteCommand::Version => print_remote_version(&address).await,
                RemoteCommand::Serve(_) => unreachable!(),
            }
        }
    }
}

async fn execute_serve(global_address: Option<String>, args: ServeArgs) -> miette::Result<()> {
    let cache_dir = expand_tilde(&args.cache_dir)?;
    let envs_dir = expand_tilde(&args.envs_dir)?;

    // SAFETY: called before spawning threads; the server is single-threaded at
    // this point. These env vars configure pixi internals (cache, environments).
    unsafe {
        std::env::set_var("PIXI_CACHE_DIR", &cache_dir);
        std::env::set_var("PIXI_HOME", &envs_dir);
    }

    let raw_address = global_address.unwrap_or_else(|| {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            format!("{runtime_dir}/pixi.sock")
        } else {
            format!("/tmp/pixi-{}.sock", std::process::id())
        }
    });
    let address = pixi_varlink::client::normalize_address(&raw_address);
    tracing::info!(
        address = %address,
        cache_dir = %cache_dir.display(),
        envs_dir = %envs_dir.display(),
        version = pixi_consts::consts::PIXI_VERSION,
        "starting varlink server",
    );
    pixi_varlink::run_server(&address).await
}

async fn print_remote_version(address: &str) -> miette::Result<()> {
    let remote = pixi_varlink::client::version(address).await?;
    let local = pixi_consts::consts::PIXI_VERSION;

    if local == remote {
        writeln!(std::io::stdout(), "pixi {remote}").into_diagnostic()?;
    } else {
        writeln!(std::io::stdout(), "pixi client: {local}").into_diagnostic()?;
        writeln!(std::io::stdout(), "pixi server: {remote}").into_diagnostic()?;
    }
    Ok(())
}

async fn execute_global(address: &str, cmd: RemoteGlobalCommand) -> miette::Result<()> {
    match cmd.command {
        RemoteGlobalSubCommand::Install(args) => execute_global_install(address, args).await,
    }
}

async fn execute_global_install(
    address: &str,
    args: crate::global::install::Args,
) -> miette::Result<()> {
    let envs_dir = pixi_global::EnvRoot::from_env()
        .await?
        .path()
        .to_path_buf();
    tokio::fs::create_dir_all(&envs_dir)
        .await
        .map_err(|e| miette::miette!("failed to create {}: {e}", envs_dir.display()))?;

    let install_args = pixi_varlink::client::GlobalInstallArgs {
        packages: args.packages.specs.clone(),
        channels: args.channels.iter().map(|c| c.to_string()).collect(),
        environment: args.environment_or_default(),
        platform: args.platform.map(|p| p.to_string()),
        expose: args.expose.iter().map(|m| m.to_string()).collect(),
        with: args.with.iter().map(|s| s.to_string()).collect(),
        force_reinstall: args.force_reinstall,
        no_shortcuts: args.no_shortcuts,
        client_envs_dir: envs_dir.to_string_lossy().to_string(),
    };

    let progress = std::cell::RefCell::new(RemoteProgress::new());

    let result = pixi_varlink::client::global_install(address, install_args, &|msg| {
        progress.borrow_mut().on_message(msg);
    })
    .await?;

    progress.borrow_mut().finish();

    pixi_varlink::client::create_symlinks(&result)?;
    print_install_result(&result)
}

fn print_install_result(
    result: &pixi_varlink::client::GlobalInstallResult,
) -> miette::Result<()> {
    let mut message = String::new();

    message.push_str("└──");

    if result.packages.len() == 1 && result.packages[0].name == result.display_env_name {
        let pkg = &result.packages[0];
        message.push_str(&format!(
            " {}: {} ({})",
            console::style(&result.display_env_name).bold(),
            console::style(&pkg.version).blue(),
            console::style("installed").green(),
        ));
    } else {
        message.push_str(&format!(
            " {} ({})",
            console::style(&result.display_env_name).bold(),
            console::style("installed").green(),
        ));

        if !result.packages.is_empty() {
            let deps = result
                .packages
                .iter()
                .map(|p| {
                    format!(
                        "{} {}",
                        console::style(&p.name).green(),
                        console::style(&p.version).blue()
                    )
                })
                .join(", ");
            message.push_str(&format_asciiart_section(
                "packages",
                deps,
                true,
                !result.binaries.is_empty(),
            ));
        }
    }

    if !result.binaries.is_empty() {
        let exposed = result
            .binaries
            .iter()
            .map(|b| b.name.as_str())
            .join(", ");
        message.push_str(&format_asciiart_section("exposes", exposed, true, false));
    }

    writeln!(std::io::stdout(), "{message}").into_diagnostic()?;
    Ok(())
}

/// Drives progress bars from server-streamed messages, using the same
/// [`MainProgressBar`] that `pixi global install` uses for solving.
struct RemoteProgress {
    solve_bar: MainProgressBar<String>,
    install_bar: MainProgressBar<String>,
    solve_id: Option<usize>,
    install_id: Option<usize>,
}

impl RemoteProgress {
    fn new() -> Self {
        let multi = pixi_progress::global_multi_progress();
        let anchor = multi.add(ProgressBar::hidden());
        let solve_bar = MainProgressBar::new(
            multi.clone(),
            pixi_progress::ProgressBarPlacement::Before(anchor.clone()),
            "solving".to_owned(),
        );
        let install_bar = MainProgressBar::new(
            multi,
            pixi_progress::ProgressBarPlacement::Before(anchor),
            "installing".to_owned(),
        );
        Self {
            solve_bar,
            install_bar,
            solve_id: None,
            install_id: None,
        }
    }

    fn on_message(&mut self, msg: &str) {
        // The eprintln flushes stderr which triggers indicatif to redraw.
        eprintln!("[remote progress] {msg}");
        match msg {
            "pixi solve: queued" => {
                let id = self.solve_bar.queued("remote".to_owned());
                self.solve_id = Some(id);
            }
            "solving: started" => {
                if let Some(id) = self.solve_id {
                    self.solve_bar.start(id);
                }
            }
            "solving: finished" => {
                if let Some(id) = self.solve_id {
                    self.solve_bar.finish(id);
                }
            }
            "pixi solve: finished" => {
                self.solve_bar.clear();
            }
            "install: queued" => {
                let id = self.install_bar.queued("remote".to_owned());
                self.install_id = Some(id);
            }
            "install: started" => {
                if let Some(id) = self.install_id {
                    self.install_bar.start(id);
                }
            }
            "install: finished" => {
                if let Some(id) = self.install_id {
                    self.install_bar.finish(id);
                }
            }
            _ => {}
        }
    }

    fn finish(&mut self) {
        self.solve_bar.clear();
        self.install_bar.clear();
    }
}
