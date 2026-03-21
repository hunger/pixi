use std::io::Write;

use itertools::Itertools;
use miette::IntoDiagnostic;
use pixi_global::list::format_asciiart_section;

use super::progress::RemoteProgress;

pub async fn execute(
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

    let env_name = args.environment_or_default();

    // If the client doesn't have the env symlink but the server might already
    // have it from a previous run, force a reinstall so the server recreates
    // the bin dir and we get fresh symlinks.
    let client_symlink_missing = env_name
        .as_ref()
        .map(|name| !envs_dir.join(name).is_symlink())
        .unwrap_or(false);
    let force_reinstall = args.force_reinstall || client_symlink_missing;

    let install_args = pixi_varlink::client::GlobalInstallArgs {
        packages: args.packages.specs.clone(),
        channels: args.channels.iter().map(|c| c.to_string()).collect(),
        environment: env_name,
        platform: args.platform.map(|p| p.to_string()),
        expose: args.expose.iter().map(|m| m.to_string()).collect(),
        with: args.with.iter().map(|s| s.to_string()).collect(),
        force_reinstall,
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
