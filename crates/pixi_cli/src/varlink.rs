use clap::Parser;

/// Start a varlink IPC server for programmatic access to pixi
#[derive(Parser, Debug)]
pub struct Args {
    /// Address to listen on (e.g. /run/user/1000/pixi.sock or tcp:127.0.0.1:5678)
    #[arg(long, default_value_t = default_address())]
    pub address: String,
}

fn default_address() -> String {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        format!("{runtime_dir}/pixi.sock")
    } else {
        format!("/tmp/pixi-{}.sock", std::process::id())
    }
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let address = pixi_varlink::client::normalize_address(&args.address);
    eprintln!("Listening on {address}");
    pixi_varlink::run_server(&address).await
}
