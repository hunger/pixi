use std::path::PathBuf;

use clap::Parser;

/// Start a varlink IPC server for programmatic access to pixi
#[derive(Parser, Debug)]
pub struct Args {
    /// Address to listen on (e.g. /run/user/1000/pixi.sock or tcp:127.0.0.1:5678)
    #[arg(long, default_value_t = default_address())]
    pub address: String,

    /// Cache directory for rattler/conda packages
    #[arg(long, env = "PIXI_CACHE_DIR", default_value_os_t = default_cache_dir())]
    pub cache_dir: PathBuf,

    /// Directory for storing environments
    #[arg(long, env = "PIXI_ENVS_DIR", default_value_os_t = default_envs_dir())]
    pub envs_dir: PathBuf,
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .expect("HOME environment variable not set")
}

fn default_address() -> String {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        format!("{runtime_dir}/pixi.sock")
    } else {
        format!("/tmp/pixi-{}.sock", std::process::id())
    }
}

fn default_cache_dir() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".cache"))
        .join("rattler")
}

fn default_envs_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".local/share"))
        .join("pixi")
}

pub async fn execute(args: Args) -> miette::Result<()> {
    // SAFETY: called before spawning threads; the server is single-threaded at
    // this point. These env vars configure pixi internals (cache, environments).
    unsafe {
        std::env::set_var("PIXI_CACHE_DIR", &args.cache_dir);
        std::env::set_var("PIXI_ENVS_DIR", &args.envs_dir);
    }

    let address = pixi_varlink::client::normalize_address(&args.address);
    eprintln!("Listening on {address}");
    eprintln!("Cache dir: {}", args.cache_dir.display());
    eprintln!("Envs dir: {}", args.envs_dir.display());
    pixi_varlink::run_server(&address).await
}
