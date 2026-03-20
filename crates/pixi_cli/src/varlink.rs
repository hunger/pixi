use clap::Parser;

/// Start a varlink IPC server for programmatic access to pixi
#[derive(Parser, Debug)]
pub struct Args {
    /// Address to listen on (e.g. unix:/run/user/1000/pixi.sock or tcp:127.0.0.1:5678)
    #[arg(long, default_value_t = default_address())]
    pub address: String,
}

fn default_address() -> String {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        format!("unix:{runtime_dir}/pixi.sock")
    } else {
        format!("unix:/tmp/pixi-{}.sock", std::process::id())
    }
}

/// Normalize the address: bare paths become `unix:` addresses.
fn normalize_address(address: &str) -> String {
    if address.starts_with("unix:") || address.starts_with("tcp:") {
        address.to_string()
    } else if address.starts_with('/') || address.starts_with('.') {
        format!("unix:{address}")
    } else {
        address.to_string()
    }
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let address = normalize_address(&args.address);
    eprintln!("Listening on {address}");
    pixi_varlink::run_server(&address).await
}
