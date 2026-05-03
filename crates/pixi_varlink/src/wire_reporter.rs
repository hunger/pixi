//! Server-side reporter that funnels every callback the
//! [`pixi_command_dispatcher`] (and the third-party reporters it
//! creates) makes during an install into structured
//! [`ReporterCall`]s on a channel. The varlink `Install` RPC pumps
//! that channel into its reply stream as
//! [`InstallReply::ReporterCall`](crate::InstallReply::ReporterCall)
//! items, so every callback the local install path would have made
//! reaches the client in order.
//!
//! `WireReporter` itself is `Clone` (the inner sender sits behind an
//! `Arc`), so the dispatcher's factory methods that demand a
//! freshly-boxed reporter for the rattler subsystems get an instance
//! that funnels into the same channel.
#![cfg(unix)]

use std::sync::Arc;

use futures::Stream;
use pixi_build_discovery::JsonRpcBackendSpec;
use pixi_command_dispatcher::{
    BackendSourceBuildReporter, BackendSourceBuildSpec, BuildBackendMetadataInner,
    BuildBackendMetadataReporter, CondaSolveReporter, GitCheckoutReporter,
    InstallPixiEnvironmentSpec, InstantiateBackendReporter, PixiInstallReporter,
    PixiSolveEnvironmentSpec, PixiSolveReporter, SolveCondaEnvironmentSpec, SourceMetadataReporter,
    SourceMetadataReporterSpec, SourceRecordReporter, SourceRecordReporterSpec,
    UrlCheckoutReporter,
};
use pixi_compute_reporters::OperationId;
use pixi_git::resolver::RepositoryReference;
use rattler::install::Transaction;
use rattler_conda_types::{PrefixRecord, RepoDataRecord};
use tokio::sync::mpsc;
use url::Url;

use crate::reporter_wire::{
    CondaSolveEnvWire, InstallEnvWire, PixiSolveEnvWire, ReporterCall, TransactionOpWire,
};

/// Cheap-to-clone fan-in reporter. Every clone funnels into the same
/// `mpsc::UnboundedSender`, so the rattler-side factory methods below
/// can hand out independent boxed install reporters without losing
/// event ordering.
#[derive(Clone)]
pub(crate) struct WireReporter {
    inner: Arc<Inner>,
}

struct Inner {
    tx: mpsc::UnboundedSender<ReporterCall>,
    /// Allocator for the `usize` ids the rattler install reporter
    /// hands back from `on_*_start`. Reporter sub-traits get their
    /// ids from the dispatcher's `OperationRegistry` instead.
    next_install_id: std::sync::atomic::AtomicU64,
}

impl WireReporter {
    pub(crate) fn new(tx: mpsc::UnboundedSender<ReporterCall>) -> Self {
        Self {
            inner: Arc::new(Inner {
                tx,
                next_install_id: std::sync::atomic::AtomicU64::new(1),
            }),
        }
    }

    fn next_install_id(&self) -> usize {
        self.inner
            .next_install_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed) as usize
    }

    fn emit(&self, call: ReporterCall) {
        // The receiver is the streaming `Install` RPC body. If it has
        // gone away (client disconnected, stream cancelled) there is
        // no point retrying — drop the call and let the install
        // continue running for cancellation.
        let _ = self.inner.tx.send(call);
    }

    /// Allocate an [`OperationId`] backed by the dispatcher's registry.
    /// Reporter sub-trait `on_queued` impls call this to mint the id
    /// they return.
    fn allocate_id(&self) -> OperationId {
        // The dispatcher allocates real OperationIds via its
        // `OperationRegistry`; reporters only see the resulting id.
        // For the wire path we don't track parents — emit a synthetic
        // monotonically-increasing id so wire callers can still
        // correlate `on_started` / `on_finished` with `on_queued`.
        OperationId(
            self.inner
                .next_install_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        )
    }
}

// ──────────────────────────────────────────────────────────────────────
// Dispatcher reporter sub-traits.
// ──────────────────────────────────────────────────────────────────────

impl PixiInstallReporter for WireReporter {
    fn on_queued(&self, env: &InstallPixiEnvironmentSpec) -> OperationId {
        let id = self.allocate_id();
        self.emit(ReporterCall::PixiInstallOnQueued {
            env: InstallEnvWire {
                name: env.name.clone(),
            },
            id: id.0,
        });
        id
    }
    fn on_started(&self, install_id: OperationId) {
        self.emit(ReporterCall::PixiInstallOnStarted { id: install_id.0 });
    }
    fn on_finished(&self, install_id: OperationId) {
        self.emit(ReporterCall::PixiInstallOnFinished { id: install_id.0 });
    }

    fn create_install_reporter(&self) -> Option<Box<dyn rattler::install::Reporter>> {
        Some(Box::new(self.clone()))
    }
}

impl PixiSolveReporter for WireReporter {
    fn on_queued(&self, env: &PixiSolveEnvironmentSpec) -> OperationId {
        let id = self.allocate_id();
        self.emit(ReporterCall::PixiSolveOnQueued {
            env: PixiSolveEnvWire {
                name: env.name.clone(),
                platform: env.platform.to_string(),
                has_direct_conda_dependency: env.has_direct_conda_dependency,
            },
            id: id.0,
        });
        id
    }
    fn on_started(&self, solve_id: OperationId) {
        self.emit(ReporterCall::PixiSolveOnStarted { id: solve_id.0 });
    }
    fn on_finished(&self, solve_id: OperationId) {
        self.emit(ReporterCall::PixiSolveOnFinished { id: solve_id.0 });
    }
}

impl CondaSolveReporter for WireReporter {
    fn on_queued(&self, env: &SolveCondaEnvironmentSpec) -> OperationId {
        let id = self.allocate_id();
        self.emit(ReporterCall::CondaSolveOnQueued {
            env: CondaSolveEnvWire {
                name: env.name.clone(),
            },
            id: id.0,
        });
        id
    }
    fn on_started(&self, solve_id: OperationId) {
        self.emit(ReporterCall::CondaSolveOnStarted { id: solve_id.0 });
    }
    fn on_finished(&self, solve_id: OperationId) {
        self.emit(ReporterCall::CondaSolveOnFinished { id: solve_id.0 });
    }
}

impl GitCheckoutReporter for WireReporter {
    fn on_queued(&self, env: &RepositoryReference) -> OperationId {
        let id = self.allocate_id();
        self.emit(ReporterCall::GitCheckoutOnQueued {
            repo: format!("{env:?}"),
            id: id.0,
        });
        id
    }
    fn on_started(&self, checkout_id: OperationId) {
        self.emit(ReporterCall::GitCheckoutOnStarted { id: checkout_id.0 });
    }
    fn on_finished(&self, checkout_id: OperationId) {
        self.emit(ReporterCall::GitCheckoutOnFinished { id: checkout_id.0 });
    }
}

impl UrlCheckoutReporter for WireReporter {
    fn on_queued(&self, env: &Url) -> OperationId {
        let id = self.allocate_id();
        self.emit(ReporterCall::UrlCheckoutOnQueued {
            url: env.to_string(),
            id: id.0,
        });
        id
    }
    fn on_started(&self, checkout_id: OperationId) {
        self.emit(ReporterCall::UrlCheckoutOnStarted { id: checkout_id.0 });
    }
    fn on_finished(&self, checkout_id: OperationId) {
        self.emit(ReporterCall::UrlCheckoutOnFinished { id: checkout_id.0 });
    }
}

impl InstantiateBackendReporter for WireReporter {
    fn on_queued(&self, spec: &JsonRpcBackendSpec) -> OperationId {
        let id = self.allocate_id();
        self.emit(ReporterCall::InstantiateBackendOnQueued {
            spec: format!("{spec:?}"),
            id: id.0,
        });
        id
    }
    fn on_started(&self, id: OperationId) {
        self.emit(ReporterCall::InstantiateBackendOnStarted { id: id.0 });
    }
    fn on_finished(&self, id: OperationId) {
        self.emit(ReporterCall::InstantiateBackendOnFinished { id: id.0 });
    }

    fn create_install_reporter(&self) -> Option<Box<dyn rattler::install::Reporter>> {
        Some(Box::new(self.clone()))
    }
}

impl BuildBackendMetadataReporter for WireReporter {
    fn on_queued(&self, env: &BuildBackendMetadataInner) -> OperationId {
        let id = self.allocate_id();
        self.emit(ReporterCall::BuildBackendMetadataOnQueued {
            env: format!("{env:?}"),
            id: id.0,
        });
        id
    }
    fn on_started(
        &self,
        id: OperationId,
        _backend_output_stream: Box<dyn Stream<Item = String> + Unpin + Send>,
    ) {
        // Backend stdout/stderr is dropped for v1; relaying it is a
        // separate UX problem we'll tackle alongside the renderer.
        self.emit(ReporterCall::BuildBackendMetadataOnStarted { id: id.0 });
    }
    fn on_finished(&self, id: OperationId, failed: bool) {
        self.emit(ReporterCall::BuildBackendMetadataOnFinished { id: id.0, failed });
    }
}

impl SourceRecordReporter for WireReporter {
    fn on_queued(&self, spec: &SourceRecordReporterSpec) -> OperationId {
        let id = self.allocate_id();
        self.emit(ReporterCall::SourceRecordOnQueued {
            spec: format!("{spec:?}"),
            id: id.0,
        });
        id
    }
    fn on_started(&self, id: OperationId) {
        self.emit(ReporterCall::SourceRecordOnStarted { id: id.0 });
    }
    fn on_finished(&self, id: OperationId) {
        self.emit(ReporterCall::SourceRecordOnFinished { id: id.0 });
    }
}

impl SourceMetadataReporter for WireReporter {
    fn on_queued(&self, spec: &SourceMetadataReporterSpec) -> OperationId {
        let id = self.allocate_id();
        self.emit(ReporterCall::SourceMetadataOnQueued {
            spec: format!("{spec:?}"),
            id: id.0,
        });
        id
    }
    fn on_started(&self, id: OperationId) {
        self.emit(ReporterCall::SourceMetadataOnStarted { id: id.0 });
    }
    fn on_finished(&self, id: OperationId) {
        self.emit(ReporterCall::SourceMetadataOnFinished { id: id.0 });
    }
}

impl BackendSourceBuildReporter for WireReporter {
    fn on_queued(&self, env: &BackendSourceBuildSpec) -> OperationId {
        let id = self.allocate_id();
        self.emit(ReporterCall::BackendSourceBuildOnQueued {
            // Match what `SyncReporter::on_queued` reads from the
            // spec: the package name as a source-style string. The
            // client's prep bar uses it as the "building <pkg>"
            // entry label.
            package: env.name.as_source().to_string(),
            id: id.0,
        });
        id
    }
    fn on_started(
        &self,
        id: OperationId,
        _backend_output_stream: Box<dyn Stream<Item = String> + Unpin + Send>,
    ) {
        self.emit(ReporterCall::BackendSourceBuildOnStarted { id: id.0 });
    }
    fn on_finished(&self, id: OperationId, failed: bool) {
        self.emit(ReporterCall::BackendSourceBuildOnFinished { id: id.0, failed });
    }
}

// ──────────────────────────────────────────────────────────────────────
// rattler::install::Reporter — the actual install-bar progress.
// ──────────────────────────────────────────────────────────────────────

impl rattler::install::Reporter for WireReporter {
    fn on_transaction_start(&self, transaction: &Transaction<PrefixRecord, RepoDataRecord>) {
        // Ship one wire entry per `Transaction::operations` slot,
        // mirroring what `SyncReporter::on_transaction_start` does
        // when it queues per-package install bar entries: prefer
        // the install record's name+size, fall back to the unlink
        // record's. Slots with neither (no-ops) ride as `None` so
        // the operation index later wire events carry stays aligned.
        let operations = transaction
            .operations
            .iter()
            .map(|op| {
                op.record_to_install()
                    .map(|r| TransactionOpWire {
                        name: r.package_record.name.as_normalized().to_string(),
                        size: r.package_record.size.unwrap_or(1),
                    })
                    .or_else(|| {
                        op.record_to_remove().map(|r| TransactionOpWire {
                            name: r
                                .repodata_record
                                .package_record
                                .name
                                .as_normalized()
                                .to_string(),
                            size: r.repodata_record.package_record.size.unwrap_or(1),
                        })
                    })
            })
            .collect();
        self.emit(ReporterCall::InstallOnTransactionStart { operations });
    }
    fn on_transaction_operation_start(&self, operation: usize) {
        self.emit(ReporterCall::InstallOnTransactionOperationStart { operation });
    }
    fn on_populate_cache_start(&self, operation: usize, record: &RepoDataRecord) -> usize {
        let id = self.next_install_id();
        self.emit(ReporterCall::InstallOnPopulateCacheStart {
            operation,
            package: record.package_record.name.as_normalized().to_string(),
            id,
        });
        id
    }
    fn on_validate_start(&self, cache_entry: usize) -> usize {
        let id = self.next_install_id();
        self.emit(ReporterCall::InstallOnValidateStart { cache_entry, id });
        id
    }
    fn on_validate_complete(&self, validate_idx: usize) {
        self.emit(ReporterCall::InstallOnValidateComplete { validate_idx });
    }
    fn on_download_start(&self, cache_entry: usize) -> usize {
        let id = self.next_install_id();
        self.emit(ReporterCall::InstallOnDownloadStart { cache_entry, id });
        id
    }
    fn on_download_progress(&self, download_idx: usize, progress: u64, total: Option<u64>) {
        self.emit(ReporterCall::InstallOnDownloadProgress {
            download_idx,
            progress,
            total,
        });
    }
    fn on_download_completed(&self, download_idx: usize) {
        self.emit(ReporterCall::InstallOnDownloadCompleted { download_idx });
    }
    fn on_populate_cache_complete(&self, cache_entry: usize) {
        self.emit(ReporterCall::InstallOnPopulateCacheComplete { cache_entry });
    }
    fn on_unlink_start(&self, operation: usize, record: &PrefixRecord) -> usize {
        let id = self.next_install_id();
        self.emit(ReporterCall::InstallOnUnlinkStart {
            operation,
            package: record
                .repodata_record
                .package_record
                .name
                .as_normalized()
                .to_string(),
            id,
        });
        id
    }
    fn on_unlink_complete(&self, index: usize) {
        self.emit(ReporterCall::InstallOnUnlinkComplete { index });
    }
    fn on_link_start(&self, operation: usize, record: &RepoDataRecord) -> usize {
        let id = self.next_install_id();
        self.emit(ReporterCall::InstallOnLinkStart {
            operation,
            package: record.package_record.name.as_normalized().to_string(),
            id,
        });
        id
    }
    fn on_link_complete(&self, index: usize) {
        self.emit(ReporterCall::InstallOnLinkComplete { index });
    }
    fn on_transaction_operation_complete(&self, operation: usize) {
        self.emit(ReporterCall::InstallOnTransactionOperationComplete { operation });
    }
    fn on_transaction_complete(&self) {
        self.emit(ReporterCall::InstallOnTransactionComplete);
    }
    fn on_post_link_start(&self, package_name: &str, script_path: &str) -> usize {
        let id = self.next_install_id();
        self.emit(ReporterCall::InstallOnPostLinkStart {
            package: package_name.to_string(),
            script: script_path.to_string(),
            id,
        });
        id
    }
    fn on_post_link_complete(&self, index: usize, success: bool) {
        self.emit(ReporterCall::InstallOnPostLinkComplete { index, success });
    }
    fn on_pre_unlink_start(&self, package_name: &str, script_path: &str) -> usize {
        let id = self.next_install_id();
        self.emit(ReporterCall::InstallOnPreUnlinkStart {
            package: package_name.to_string(),
            script: script_path.to_string(),
            id,
        });
        id
    }
    fn on_pre_unlink_complete(&self, index: usize, success: bool) {
        self.emit(ReporterCall::InstallOnPreUnlinkComplete { index, success });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut mpsc::UnboundedReceiver<ReporterCall>) -> Vec<ReporterCall> {
        let mut out = Vec::new();
        while let Ok(call) = rx.try_recv() {
            out.push(call);
        }
        out
    }

    /// Each clone shares the channel, so events from any clone land on
    /// the same receiver.
    #[test]
    fn clones_share_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel::<ReporterCall>();
        let reporter = WireReporter::new(tx);
        let clone = reporter.clone();

        rattler::install::Reporter::on_transaction_complete(&reporter);
        rattler::install::Reporter::on_transaction_complete(&clone);
        let calls = drain(&mut rx);
        assert_eq!(
            calls,
            vec![
                ReporterCall::InstallOnTransactionComplete,
                ReporterCall::InstallOnTransactionComplete,
            ]
        );
    }

    /// Closing the receiver before sending mustn't panic — the install
    /// task continues running after the stream is dropped (e.g. client
    /// disconnects mid-install) and reporter callbacks fire all the way
    /// through.
    #[test]
    fn dropped_receiver_does_not_panic() {
        let (tx, rx) = mpsc::unbounded_channel::<ReporterCall>();
        let reporter = WireReporter::new(tx);
        drop(rx);

        rattler::install::Reporter::on_transaction_complete(&reporter);
        // No assertion — reaching this line without panic is the test.
    }
}
