use std::path::PathBuf;

use clap::Parser;

/// Start a varlink IPC server
#[derive(Parser, Debug)]
pub struct Args {
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

pub async fn execute(address: Option<String>, args: Args) -> miette::Result<()> {
    let cache_dir = expand_tilde(&args.cache_dir)?;
    let envs_dir = expand_tilde(&args.envs_dir)?;

    // SAFETY: called before spawning threads; the server is single-threaded at
    // this point. These env vars configure pixi internals (cache, environments).
    unsafe {
        std::env::set_var("PIXI_CACHE_DIR", &cache_dir);
        std::env::set_var("PIXI_HOME", &envs_dir);
    }

    let raw_address = address.unwrap_or_else(|| {
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
