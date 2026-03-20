use pixi_api::Interface;

/// Non-interactive implementation of [`Interface`] for varlink server use.
///
/// Auto-confirms prompts and routes messages to tracing.
#[derive(Default)]
pub struct NonInteractiveInterface;

impl Interface for NonInteractiveInterface {
    async fn is_cli(&self) -> bool {
        false
    }

    async fn confirm(&self, msg: &str) -> miette::Result<bool> {
        tracing::info!("auto-confirming: {msg}");
        Ok(true)
    }

    async fn info(&self, msg: &str) {
        tracing::info!("{msg}");
    }

    async fn success(&self, msg: &str) {
        tracing::info!("{msg}");
    }

    async fn warning(&self, msg: &str) {
        tracing::warn!("{msg}");
    }

    async fn error(&self, msg: &str) {
        tracing::error!("{msg}");
    }
}
