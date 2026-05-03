use clap::Parser;
use miette::IntoDiagnostic;
use tokio::fs as tokio_fs;

use crate::GlobalOptions;
use pixi_global::EnvironmentName;

mod add;
pub(crate) mod daemon;
mod edit;
mod expose;
mod global_specs;
pub mod install;
mod list;
mod remove;
mod shortcut;
mod sync;
mod tree;
mod uninstall;
mod update;
mod upgrade;
mod upgrade_all;
mod wire_reporter_client;

#[derive(Debug, Parser)]
pub enum Command {
    #[clap(visible_alias = "a")]
    Add(add::Args),
    Edit(edit::Args),
    #[clap(visible_alias = "i")]
    Install(install::Args),
    Uninstall(uninstall::Args),
    #[clap(visible_alias = "rm")]
    Remove(remove::Args),
    #[clap(visible_alias = "ls")]
    List(list::Args),
    #[clap(visible_alias = "s")]
    Sync(sync::Args),
    #[clap(visible_alias = "e")]
    #[command(subcommand)]
    Expose(expose::SubCommand),
    #[command(subcommand)]
    Shortcut(shortcut::SubCommand),
    Update(update::Args),
    #[command(hide = true)]
    Upgrade(upgrade::Args),
    #[clap(alias = "ua")]
    #[command(hide = true)]
    UpgradeAll(upgrade_all::Args),
    #[clap(visible_alias = "t")]
    Tree(tree::Args),
}

/// Subcommand for global package management actions.
///
/// Install packages on the user level.
/// Into to the `$PIXI_HOME` directory, which defaults to `~/.pixi`.
#[derive(Debug, Parser)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

/// Maps global command enum variants to their function handlers.
///
/// Every subcommand takes `&GlobalOptions` so the dispatch is
/// uniform. The env-mutating subcommands (`install`, `update`,
/// `uninstall`, `add`, `remove`, `sync`) consult the daemon-routing
/// `--socket` flag; the remaining subcommands accept it but ignore
/// it.
pub async fn execute(cmd: Args, global_options: &GlobalOptions) -> miette::Result<()> {
    match cmd.command {
        Command::Add(args) => add::execute(args, global_options).await?,
        Command::Edit(args) => edit::execute(args, global_options).await?,
        Command::Install(args) => install::execute(args, global_options).await?,
        Command::Uninstall(args) => uninstall::execute(args, global_options).await?,
        Command::Remove(args) => remove::execute(args, global_options).await?,
        Command::List(args) => list::execute(args, global_options).await?,
        Command::Sync(args) => sync::execute(args, global_options).await?,
        Command::Expose(subcommand) => expose::execute(subcommand, global_options).await?,
        Command::Shortcut(subcommand) => shortcut::execute(subcommand, global_options).await?,
        Command::Update(args) => update::execute(args, global_options).await?,
        Command::Upgrade(args) => upgrade::execute(args, global_options).await?,
        Command::UpgradeAll(args) => upgrade_all::execute(args, global_options).await?,
        Command::Tree(args) => tree::execute(args, global_options).await?,
    };
    Ok(())
}

/// Reverts the changes made to the project for a specific environment after an error occurred.
async fn revert_environment_after_error(
    env_name: &EnvironmentName,
    project_to_revert_to: &pixi_global::Project,
) -> miette::Result<()> {
    if project_to_revert_to.environment(env_name).is_some() {
        // We don't want to report on changes done by the reversion
        let _ = project_to_revert_to
            .sync_environment(env_name, None)
            .await?;
    } else {
        // clean up if directory exists for the failed new environment
        let env_dir_path = project_to_revert_to.env_root_path().join(env_name.as_str());
        if env_dir_path.exists() {
            tokio_fs::remove_dir_all(&env_dir_path)
                .await
                .into_diagnostic()?;
            tracing::debug!(
                "Cleaned up failed environment directory: {}",
                env_dir_path.display()
            );
        }
    }
    Ok(())
}
