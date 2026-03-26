use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;

use indicatif::ProgressBar;
use itertools::Itertools;
use miette::IntoDiagnostic;
use pixi_global::list::format_asciiart_section;
use pixi_reporters::main_progress_bar::MainProgressBar;
use pixi_varlink::{ProgressBarKind, ProgressState};

pub async fn execute(address: &str, args: crate::global::install::Args) -> miette::Result<()> {
    let envs_dir = pixi_global::EnvRoot::from_env().await?.path().to_path_buf();
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

    let mp = pixi_progress::global_multi_progress();
    let anchor = mp.add(ProgressBar::hidden());
    let placement = pixi_progress::ProgressBarPlacement::Before(anchor.clone());

    let solve_bar = MainProgressBar::<String>::new(mp.clone(), placement.clone(), "solving".to_owned());
    let install_bar = MainProgressBar::<String>::new(mp.clone(), placement.clone(), "installing".to_owned());
    let permissions_bar = MainProgressBar::<String>::new(mp.clone(), placement.clone(), "fixing permissions".to_owned());

    // Map server-side progress IDs to local MainProgressBar IDs.
    // RefCell because on_progress is &dyn Fn (not FnMut).
    let solve_ids: RefCell<HashMap<i64, usize>> = RefCell::new(HashMap::new());
    let install_ids: RefCell<HashMap<i64, usize>> = RefCell::new(HashMap::new());
    let permissions_ids: RefCell<HashMap<i64, usize>> = RefCell::new(HashMap::new());

    let result = pixi_varlink::client::global_install(address, install_args, &|progress| {
        match progress.progress_bar {
            ProgressBarKind::Global => {}
            ProgressBarKind::CondaSolve | ProgressBarKind::PixiSolve => {
                handle_progress(
                    &solve_bar,
                    &solve_ids,
                    progress,
                    || format!("{:?}", progress.progress_bar),
                );
            }
            ProgressBarKind::PixiInstall => {
                handle_progress(
                    &install_bar,
                    &install_ids,
                    progress,
                    || "install".to_owned(),
                );
            }
            ProgressBarKind::FixPermissions => {
                handle_progress(
                    &permissions_bar,
                    &permissions_ids,
                    progress,
                    || "permissions".to_owned(),
                );
            }
        }
    })
    .await;

    solve_bar.clear();
    install_bar.clear();
    permissions_bar.clear();
    anchor.finish_and_clear();

    let result = result?;

    pixi_varlink::client::create_symlinks(&result)?;
    print_install_result(&result)
}

fn handle_progress(
    bar: &MainProgressBar<String>,
    ids: &RefCell<HashMap<i64, usize>>,
    progress: &pixi_varlink::Progress,
    label: impl FnOnce() -> String,
) {
    match progress.progress_state {
        ProgressState::Queued => {
            let local_id = bar.queued(label());
            ids.borrow_mut().insert(progress.id, local_id);
        }
        ProgressState::Started => {
            let local_id = *ids
                .borrow_mut()
                .entry(progress.id)
                .or_insert_with(|| bar.queued(label()));
            bar.start(local_id);
        }
        ProgressState::Finished => {
            if let Some(&local_id) = ids.borrow().get(&progress.id) {
                bar.finish(local_id);
            }
        }
    }
}

fn print_install_result(result: &pixi_varlink::client::GlobalInstallResult) -> miette::Result<()> {
    // Discover exposed binaries by scanning the server's bin dir
    let bin_dir = result.sha_dir.join("bin");
    let mut exposed_names: Vec<String> = Vec::new();
    #[allow(clippy::disallowed_methods)]
    if let Ok(entries) = std::fs::read_dir(&bin_dir) {
        for entry in entries.flatten() {
            if entry.path().is_file() {
                exposed_names.push(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    exposed_names.sort();

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
                !exposed_names.is_empty(),
            ));
        }
    }

    if !exposed_names.is_empty() {
        let exposed = exposed_names.iter().join(", ");
        message.push_str(&format_asciiart_section("exposes", exposed, true, false));
    }

    writeln!(std::io::stdout(), "{message}").into_diagnostic()?;
    Ok(())
}
