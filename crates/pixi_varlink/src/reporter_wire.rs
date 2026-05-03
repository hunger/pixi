//! Wire shape for the daemon's reporter stream.
//!
//! [`ReporterCall`] is a 1:1 mirror of every reporter callback the
//! dispatcher and rattler subsystems make during an install: one
//! variant per trait method, with `id` payloads preserved so the
//! client can reconstruct the call sequence in order. The server-side
//! [`WireReporter`](crate::wire_reporter::WireReporter) emits
//! `ReporterCall`s; client-side [`ReporterClient`] consumers turn
//! them back into structured calls in process.
//!
//! Ids are `u64`, matching `pixi_compute_reporters::OperationId(u64)`.
//! For arguments that are heavy or non-(de)serializable on the
//! original trait, this module defines wire-friendly mirror structs
//! that carry just the fields a renderer-side reporter would actually
//! read; anything genuinely opaque is shipped as a debug string.

use serde::{Deserialize, Serialize};

/// Lightweight reporter-facing view of a pixi environment solve, on
/// the wire. Mirrors `pixi_command_dispatcher::PixiSolveEnvironmentSpec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PixiSolveEnvWire {
    pub name: String,
    pub platform: String,
    pub has_direct_conda_dependency: bool,
}

/// Wire view of an `InstallPixiEnvironmentSpec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallEnvWire {
    pub name: String,
}

/// Wire view of a `SolveCondaEnvironmentSpec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CondaSolveEnvWire {
    pub name: Option<String>,
}

/// Per-operation summary of a `Transaction` — what `SyncReporter`
/// reads from each `record_to_install()` / `record_to_remove()` to
/// build a per-package install bar entry. Shipped in
/// [`ReporterCall::InstallOnTransactionStart`] so the client can
/// pre-populate its install bar with one entry per operation,
/// matching the local install path's UX.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionOpWire {
    pub name: String,
    /// Reported package size in bytes (`size` defaults to `1` when
    /// the upstream record didn't carry one).
    pub size: u64,
}

/// One reporter callback's worth of data. Variant names track the
/// trait + method (`PixiSolveOnQueued`, `InstallOnLinkStart`, …) so
/// a code search lands directly on both sides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ReporterCall {
    // ── PixiInstallReporter ──
    PixiInstallOnQueued {
        env: InstallEnvWire,
        id: u64,
    },
    PixiInstallOnStarted {
        id: u64,
    },
    PixiInstallOnFinished {
        id: u64,
    },

    // ── PixiSolveReporter ──
    PixiSolveOnQueued {
        env: PixiSolveEnvWire,
        id: u64,
    },
    PixiSolveOnStarted {
        id: u64,
    },
    PixiSolveOnFinished {
        id: u64,
    },

    // ── CondaSolveReporter ──
    CondaSolveOnQueued {
        env: CondaSolveEnvWire,
        id: u64,
    },
    CondaSolveOnStarted {
        id: u64,
    },
    CondaSolveOnFinished {
        id: u64,
    },

    // ── GitCheckoutReporter ──
    GitCheckoutOnQueued {
        /// `Debug` of the `RepositoryReference`. Heavy and not
        /// natively serialisable; rendering renders the string verbatim.
        repo: String,
        id: u64,
    },
    GitCheckoutOnStarted {
        id: u64,
    },
    GitCheckoutOnFinished {
        id: u64,
    },

    // ── UrlCheckoutReporter ──
    UrlCheckoutOnQueued {
        url: String,
        id: u64,
    },
    UrlCheckoutOnStarted {
        id: u64,
    },
    UrlCheckoutOnFinished {
        id: u64,
    },

    // ── InstantiateBackendReporter ──
    InstantiateBackendOnQueued {
        spec: String,
        id: u64,
    },
    InstantiateBackendOnStarted {
        id: u64,
    },
    InstantiateBackendOnFinished {
        id: u64,
    },

    // ── BuildBackendMetadataReporter ──
    BuildBackendMetadataOnQueued {
        env: String,
        id: u64,
    },
    BuildBackendMetadataOnStarted {
        id: u64,
    },
    BuildBackendMetadataOnFinished {
        id: u64,
        failed: bool,
    },

    // ── SourceRecordReporter ──
    SourceRecordOnQueued {
        spec: String,
        id: u64,
    },
    SourceRecordOnStarted {
        id: u64,
    },
    SourceRecordOnFinished {
        id: u64,
    },

    // ── SourceMetadataReporter ──
    SourceMetadataOnQueued {
        spec: String,
        id: u64,
    },
    SourceMetadataOnStarted {
        id: u64,
    },
    SourceMetadataOnFinished {
        id: u64,
    },

    // ── BackendSourceBuildReporter ──
    BackendSourceBuildOnQueued {
        env: String,
        id: u64,
    },
    BackendSourceBuildOnStarted {
        id: u64,
    },
    BackendSourceBuildOnFinished {
        id: u64,
        failed: bool,
    },

    // ── rattler::install::Reporter ──
    InstallOnTransactionStart {
        /// One entry per `Transaction::operations` slot. Operations
        /// where neither `record_to_install` nor `record_to_remove`
        /// is set still occupy an index so [`InstallOnLinkStart`]'s
        /// `operation` field aligns with this list.
        operations: Vec<Option<TransactionOpWire>>,
    },
    InstallOnTransactionOperationStart {
        operation: usize,
    },
    InstallOnPopulateCacheStart {
        operation: usize,
        package: String,
        id: usize,
    },
    InstallOnValidateStart {
        cache_entry: usize,
        id: usize,
    },
    InstallOnValidateComplete {
        validate_idx: usize,
    },
    InstallOnDownloadStart {
        cache_entry: usize,
        id: usize,
    },
    InstallOnDownloadProgress {
        download_idx: usize,
        progress: u64,
        total: Option<u64>,
    },
    InstallOnDownloadCompleted {
        download_idx: usize,
    },
    InstallOnPopulateCacheComplete {
        cache_entry: usize,
    },
    InstallOnUnlinkStart {
        operation: usize,
        package: String,
        id: usize,
    },
    InstallOnUnlinkComplete {
        index: usize,
    },
    InstallOnLinkStart {
        operation: usize,
        package: String,
        id: usize,
    },
    InstallOnLinkComplete {
        index: usize,
    },
    InstallOnTransactionOperationComplete {
        operation: usize,
    },
    InstallOnTransactionComplete,
    InstallOnPostLinkStart {
        package: String,
        script: String,
        id: usize,
    },
    InstallOnPostLinkComplete {
        index: usize,
        success: bool,
    },
    InstallOnPreUnlinkStart {
        package: String,
        script: String,
        id: usize,
    },
    InstallOnPreUnlinkComplete {
        index: usize,
        success: bool,
    },
}

/// A consumer of the daemon's marshalled reporter call stream.
///
/// Implementors receive every call the daemon's dispatcher made, in
/// order, after the wire transport. The default implementation logs
/// each call to `tracing` at `INFO` under target
/// `pixi::install::reporter`.
pub trait ReporterClient: Send + Sync {
    fn on_call(&self, call: ReporterCall) {
        tracing::info!(
            target: "pixi::install::reporter",
            ?call,
            "reporter call"
        );
    }
}

/// Default logging implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoggingReporterClient;

impl ReporterClient for LoggingReporterClient {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a representative slice of variants through JSON.
    /// Catches accidental schema breakage.
    #[test]
    fn round_trip_through_json() {
        let cases = vec![
            ReporterCall::PixiSolveOnQueued {
                env: PixiSolveEnvWire {
                    name: "foo".into(),
                    platform: "linux-64".into(),
                    has_direct_conda_dependency: true,
                },
                id: 7,
            },
            ReporterCall::CondaSolveOnQueued {
                env: CondaSolveEnvWire {
                    name: Some("xz@linux-64".into()),
                },
                id: 8,
            },
            ReporterCall::InstallOnTransactionStart {
                operations: vec![
                    Some(TransactionOpWire {
                        name: "libgomp".into(),
                        size: 312345,
                    }),
                    None,
                    Some(TransactionOpWire {
                        name: "xz".into(),
                        size: 67890,
                    }),
                ],
            },
            ReporterCall::InstallOnPopulateCacheStart {
                operation: 3,
                package: "libgomp".into(),
                id: 42,
            },
            ReporterCall::InstallOnTransactionComplete,
            ReporterCall::InstallOnDownloadProgress {
                download_idx: 1,
                progress: 100,
                total: Some(200),
            },
            ReporterCall::InstallOnDownloadProgress {
                download_idx: 2,
                progress: 100,
                total: None,
            },
        ];
        for call in cases {
            let json = serde_json::to_string(&call).expect("serialize");
            let back: ReporterCall = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, call, "round-trip: {json}");
        }
    }

    /// `InstallOnTransactionComplete` (no fields) serialises without a
    /// `content` field.
    #[test]
    fn unit_variant_shape() {
        let json = serde_json::to_string(&ReporterCall::InstallOnTransactionComplete).unwrap();
        assert_eq!(json, r#"{"method":"install_on_transaction_complete"}"#);
    }
}
