use std::io::Write;

use miette::IntoDiagnostic;

pub async fn execute(address: &str) -> miette::Result<()> {
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
