//! Daemon-routed install renderer.
//!
//! Receives [`ReporterCall`]s the daemon's `WireReporter` marshalled
//! over the varlink stream and drives indicatif using the same
//! primitives the local install path's `TopLevelProgress` does, so
//! the user sees the same bars whether the install ran locally or
//! via the daemon. The renderer consumes the wire types directly;
//! no fake `Transaction` / `RepoDataRecord` round-tripping.
//!
//! Bar coverage matches `TopLevelProgress` for the binary install
//! flow:
//!
//! - **solving**: a [`MainProgressBar<String>`] entry per
//!   pixi-environment solve (and per top-level conda solve that
//!   isn't nested under a pixi solve). Driven by
//!   `PixiSolveOn*` and `CondaSolveOn*` events. The two reporter
//!   streams share the same bar tracker for nested solves —
//!   `CondaSolveOnQueued { reason: SolvePixi(p) }` looks up `p`'s
//!   tracker and reuses it, matching `TopLevelProgress`'s
//!   `CondaSolveReporter` impl.
//! - **fetching repodata**: a [`RepodataReporter`] driven by
//!   `DownloadOn*` events (the gateway and run-exports reporters
//!   both feed download events into this single bar in the local
//!   path; we mirror that).
//! - **preparing packages**: a [`BuildDownloadVerifyReporter`]
//!   driven by `InstallOn{PopulateCache, Validate, Download}*` for
//!   per-package cache prep and by `BackendSourceBuildOn*` for
//!   source builds (both share this bar in `SyncReporter`).
//! - **installing**: a [`MainProgressBar<PackageWithSize>`] populated
//!   from `InstallOnTransactionStart`'s op list and advanced by
//!   `InstallOnLinkStart` / `InstallOnUnlinkStart` /
//!   `InstallOnTransactionOperationComplete`.
//!
//! `GitCheckoutOn*` events drive a one-spinner-per-checkout bar
//! mirroring `GitCheckoutProgress`'s UX (prefix `fetching git
//! dependencies`, message `checking out <url>@<reference>`).
//!
//! Reporter callbacks `TopLevelProgress` doesn't render today —
//! `PixiInstallOn*`, `InstantiateBackendOn*`, `SourceMetadataOn*`,
//! `SourceRecordOn*`, `BuildBackendMetadataOn*`, `UrlCheckoutOn*` —
//! are explicit no-ops here too, to avoid drifting from the local UX.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar};
use pixi_progress::ProgressBarPlacement;
use pixi_reporters::download_verify_reporter::BuildDownloadVerifyReporter;
use pixi_reporters::git::GitCheckoutProgress;
use pixi_reporters::main_progress_bar::MainProgressBar;
use pixi_reporters::sync_reporter::PackageWithSize;
use pixi_varlink::{ReporterCall, ReporterClient};

/// `ReporterClient` that drives indicatif from the daemon's wire
/// stream. Cheap to construct — bars are created lazily on first
/// relevant event so a request that never reaches the install phase
/// doesn't flash an empty bar.
pub struct WireReporterClient {
    multi: MultiProgress,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Lazily-constructed "solving" bar, shared across pixi-solve
    /// and conda-solve events.
    solve_bar: Option<MainProgressBar<String>>,
    /// Lazily-constructed "preparing packages" bar, shared across
    /// per-package cache prep and source builds (mirrors
    /// `SyncReporter`'s composition).
    prep_bar: Option<BuildDownloadVerifyReporter>,
    /// Lazily-constructed "installing" bar, populated up-front when
    /// `InstallOnTransactionStart` arrives so its total reflects
    /// the full operation set immediately.
    install_bar: Option<MainProgressBar<PackageWithSize>>,

    /// Wire-side solve id → tracker id returned by
    /// `MainProgressBar::queued`. Holds entries for both
    /// `PixiSolveOnQueued` and `CondaSolveOnQueued`. Conda solves
    /// nested under a pixi solve inherit the parent's tracker via
    /// aliasing — see the `CondaSolveOnQueued` arm.
    solve_id_map: HashMap<u64, usize>,
    /// Transaction operation index → install-bar tracker id.
    /// `None`-slot operations don't appear here.
    install_op_map: HashMap<usize, usize>,
    /// Per-op cached `(name, size)` extracted from the operations
    /// list at `InstallOnTransactionStart`. Subsequent
    /// `InstallOnPopulateCacheStart` events look up by op index to
    /// queue a prep-bar entry without re-shipping the metadata.
    op_meta: HashMap<usize, (String, Option<u64>)>,
    /// Wire cache-entry id (the `id` returned from the server's
    /// `on_populate_cache_start`) → prep-bar tracker id. Used by
    /// the validate / download / populate-complete events.
    cache_entry_to_prep: HashMap<usize, usize>,
    /// Wire `BackendSourceBuildOnQueued.id` → prep-bar tracker id.
    /// Source builds drive the same prep bar as cache prep.
    source_build_to_prep: HashMap<u64, usize>,

    /// Wire `GitCheckoutOnQueued.id` → cached `(url, reference)`
    /// strings, stashed at `OnQueued` and read again at `OnStarted`
    /// to label the spinner.
    git_pending: HashMap<u64, (String, String)>,
    /// Wire `GitCheckoutOnQueued.id` → live spinner bar. Created on
    /// Started, finished and removed on Finished.
    git_bars: HashMap<u64, ProgressBar>,
}

impl WireReporterClient {
    /// Build a renderer driving the given `MultiProgress`. Production
    /// passes [`pixi_progress::global_multi_progress`]; tests pass a
    /// `MultiProgress` configured with a buffer-backed
    /// [`indicatif::TermLike`] so they can read what was drawn.
    pub fn new(multi: MultiProgress) -> Self {
        Self {
            multi,
            state: Mutex::new(State::default()),
        }
    }

    /// Acquire the state lock with a single canonical poisoned-mutex
    /// message. The renderer's match arms use this everywhere
    /// instead of inline `.lock().expect("…")` so the `expect`
    /// string can't drift across arms.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("renderer state mutex poisoned")
    }

    /// Insert (or reuse) `solve_bar` and return a clone. Pulled out
    /// so both pixi-solve and orphaned conda-solve queues can lazy-
    /// init the same bar without duplicating the constructor closure.
    fn solve_bar(&self, s: &mut State) -> MainProgressBar<String> {
        s.solve_bar
            .get_or_insert_with(|| {
                MainProgressBar::new(
                    self.multi.clone(),
                    ProgressBarPlacement::default(),
                    "solving".to_string(),
                )
            })
            .clone()
    }

    fn install_bar(&self, s: &mut State) -> MainProgressBar<PackageWithSize> {
        s.install_bar
            .get_or_insert_with(|| {
                MainProgressBar::new(
                    self.multi.clone(),
                    ProgressBarPlacement::default(),
                    "installing".to_string(),
                )
            })
            .clone()
    }

    fn prep_bar(&self, s: &mut State) -> BuildDownloadVerifyReporter {
        s.prep_bar
            .get_or_insert_with(|| {
                BuildDownloadVerifyReporter::new(
                    self.multi.clone(),
                    ProgressBarPlacement::default(),
                    "preparing packages".to_string(),
                )
            })
            .clone()
    }
}

impl ReporterClient for WireReporterClient {
    fn on_call(&self, call: ReporterCall) {
        // Per-arm pattern: take the state lock briefly, mutate the
        // wire→tracker maps and clone any bar handle we need, drop
        // the lock, then drive the bar. `MainProgressBar`,
        // `RepodataReporter`, and `BuildDownloadVerifyReporter` all
        // have internal `RwLock`s, so calling them with our state
        // lock held would risk a deadlock if anything ever called
        // back into the renderer.
        match call {
            // ── Solving: pixi-solve owns a tracker id; conda solves
            //    nested under it reuse that tracker so a single env
            //    appears once on the bar, not twice. ─────────────────
            ReporterCall::PixiSolveOnQueued { env, id, .. } => {
                let mut s = self.state();
                let bar = self.solve_bar(&mut s);
                let tracker = bar.queued(format!("{} ({})", env.name, env.platform));
                s.solve_id_map.insert(id, tracker);
            }
            // The new wire format doesn't carry parent context, so
            // PixiSolve drives its own tracker on the same bar
            // (instead of relying on a nested conda solve to advance it).
            ReporterCall::PixiSolveOnStarted { id } => {
                let s = self.state();
                if let (Some(bar), Some(&tracker)) = (&s.solve_bar, s.solve_id_map.get(&id)) {
                    bar.start(tracker);
                }
            }
            ReporterCall::PixiSolveOnFinished { id } => {
                let s = self.state();
                let tracker = s.solve_id_map.get(&id).copied();
                if let (Some(bar), Some(tracker)) = (&s.solve_bar, tracker) {
                    bar.finish(tracker);
                }
            }

            ReporterCall::CondaSolveOnQueued { env, id } => {
                // Top-level conda solve. The wire format no longer
                // carries parent context, so nested conda solves
                // (under a pixi solve) get their own bar entries.
                let mut s = self.state();
                let bar = self.solve_bar(&mut s);
                let label = env.name.unwrap_or_default();
                let tracker = bar.queued(label);
                s.solve_id_map.insert(id, tracker);
            }
            ReporterCall::CondaSolveOnStarted { id } => {
                let s = self.state();
                if let (Some(bar), Some(&tracker)) = (&s.solve_bar, s.solve_id_map.get(&id)) {
                    bar.start(tracker);
                }
            }
            ReporterCall::CondaSolveOnFinished { id } => {
                let s = self.state();
                // Don't `remove` here: a parent pixi-solve may share
                // the tracker, and we don't know whether the parent
                // has emitted its `OnFinished` yet. Leave the entry;
                // `OnFinished` (top-level) clears the whole bar.
                let tracker = s.solve_id_map.get(&id).copied();
                if let (Some(bar), Some(tracker)) = (&s.solve_bar, tracker) {
                    bar.finish(tracker);
                }
            }

            // ── Install transaction: queue per-op entries up-front
            //    so the bar's total reflects the full work set
            //    before any links happen. ─────────────────────────────
            ReporterCall::InstallOnTransactionStart { operations } => {
                let mut s = self.state();
                let bar = self.install_bar(&mut s);
                for (op_idx, op) in operations.into_iter().enumerate() {
                    if let Some(op) = op {
                        // Stash (name, size) so the prep-bar arms
                        // can queue an entry by op index without
                        // re-shipping the metadata.
                        s.op_meta.insert(op_idx, (op.name.clone(), Some(op.size)));
                        let tracker = bar.queued(PackageWithSize {
                            name: op.name,
                            size: op.size,
                        });
                        s.install_op_map.insert(op_idx, tracker);
                    }
                }
            }
            ReporterCall::InstallOnLinkStart { operation, .. }
            | ReporterCall::InstallOnUnlinkStart { operation, .. } => {
                let s = self.state();
                if let (Some(bar), Some(&tracker)) =
                    (&s.install_bar, s.install_op_map.get(&operation))
                {
                    bar.start(tracker);
                }
            }
            ReporterCall::InstallOnTransactionOperationComplete { operation } => {
                let mut s = self.state();
                let tracker = s.install_op_map.remove(&operation);
                if let (Some(bar), Some(tracker)) = (&s.install_bar, tracker) {
                    bar.finish(tracker);
                }
            }
            ReporterCall::InstallOnTransactionComplete => {
                let bar = self
                    .state
                    .lock()
                    .expect("renderer state mutex poisoned")
                    .install_bar
                    .clone();
                if let Some(bar) = bar {
                    bar.clear();
                }
            }

            // ── Per-package cache prep: validate + download +
            //    populate. Drives the same "preparing packages" bar
            //    as source builds (matching `SyncReporter`). ───────────
            ReporterCall::InstallOnPopulateCacheStart {
                operation,
                package,
                id,
            } => {
                let mut s = self.state();
                let size = s.op_meta.get(&operation).and_then(|(_, s)| *s);
                let mut bar = self.prep_bar(&mut s);
                let tracker = bar.on_entry_start_with(&package, size);
                s.cache_entry_to_prep.insert(id, tracker);
            }
            ReporterCall::InstallOnValidateStart { cache_entry, id: _ } => {
                let s = self.state();
                let tracker = s.cache_entry_to_prep.get(&cache_entry).copied();
                let bar = s.prep_bar.clone();
                drop(s);
                if let (Some(mut bar), Some(tracker)) = (bar, tracker) {
                    bar.on_validation_start(tracker);
                }
            }
            ReporterCall::InstallOnValidateComplete { validate_idx } => {
                let s = self.state();
                let tracker = s.cache_entry_to_prep.get(&validate_idx).copied();
                let bar = s.prep_bar.clone();
                drop(s);
                if let (Some(mut bar), Some(tracker)) = (bar, tracker) {
                    bar.on_validation_complete(tracker);
                }
            }
            ReporterCall::InstallOnDownloadStart { cache_entry, id: _ } => {
                let s = self.state();
                let tracker = s.cache_entry_to_prep.get(&cache_entry).copied();
                let bar = s.prep_bar.clone();
                drop(s);
                if let (Some(mut bar), Some(tracker)) = (bar, tracker) {
                    bar.on_download_start(tracker);
                }
            }
            ReporterCall::InstallOnDownloadProgress {
                download_idx,
                progress,
                total,
            } => {
                let s = self.state();
                let tracker = s.cache_entry_to_prep.get(&download_idx).copied();
                let bar = s.prep_bar.clone();
                drop(s);
                if let (Some(mut bar), Some(tracker)) = (bar, tracker) {
                    bar.on_download_progress(tracker, progress, total);
                }
            }
            ReporterCall::InstallOnDownloadCompleted { download_idx } => {
                let s = self.state();
                let tracker = s.cache_entry_to_prep.get(&download_idx).copied();
                let bar = s.prep_bar.clone();
                drop(s);
                if let (Some(mut bar), Some(tracker)) = (bar, tracker) {
                    bar.on_download_complete(tracker);
                }
            }
            ReporterCall::InstallOnPopulateCacheComplete { cache_entry } => {
                let mut s = self.state();
                let tracker = s.cache_entry_to_prep.remove(&cache_entry);
                let bar = s.prep_bar.clone();
                drop(s);
                if let (Some(mut bar), Some(tracker)) = (bar, tracker) {
                    bar.on_entry_finished(tracker);
                }
            }

            // ── Source builds: drive the same prep bar as cache
            //    prep, via `on_build_*` (matching `SyncReporter`). ─────
            ReporterCall::BackendSourceBuildOnQueued { package, id, .. } => {
                let mut s = self.state();
                let mut bar = self.prep_bar(&mut s);
                let tracker = bar.on_build_queued(&package);
                s.source_build_to_prep.insert(id, tracker);
            }
            ReporterCall::BackendSourceBuildOnStarted { id } => {
                let s = self.state();
                let tracker = s.source_build_to_prep.get(&id).copied();
                let bar = s.prep_bar.clone();
                drop(s);
                if let (Some(mut bar), Some(tracker)) = (bar, tracker) {
                    bar.on_build_start(tracker);
                }
            }
            ReporterCall::BackendSourceBuildOnFinished { id, failed: _ } => {
                let mut s = self.state();
                let tracker = s.source_build_to_prep.remove(&id);
                let bar = s.prep_bar.clone();
                drop(s);
                if let (Some(mut bar), Some(tracker)) = (bar, tracker) {
                    bar.on_build_finished(tracker);
                }
            }

            // ── Git checkouts: one spinner per fetch, prefix
            //    "fetching git dependencies" with a per-checkout
            //    message naming the URL + ref. Mirrors the local
            //    `GitCheckoutProgress` UX. ────────────────────────────
            ReporterCall::GitCheckoutOnQueued {
                url, reference, id, ..
            } => {
                self.state().git_pending.insert(id, (url, reference));
            }
            ReporterCall::GitCheckoutOnStarted { id } => {
                let pending = self.state().git_pending.remove(&id);
                if let Some((url, reference)) = pending {
                    let pb = self.multi.add(ProgressBar::hidden());
                    pb.set_style(GitCheckoutProgress::spinner_style());
                    pb.set_prefix("fetching git dependencies");
                    pb.set_message(format!("checking out {url}@{reference}"));
                    pb.enable_steady_tick(Duration::from_millis(100));
                    self.state().git_bars.insert(id, pb);
                }
            }
            ReporterCall::GitCheckoutOnFinished { id } => {
                if let Some(pb) = self.state().git_bars.remove(&id) {
                    pb.finish_and_clear();
                }
                self.state().git_pending.remove(&id);
            }

            // ── Reporter callbacks the local `TopLevelProgress`
            //    doesn't render today: mirror its no-op behaviour. ────
            ReporterCall::PixiInstallOnQueued { .. }
            | ReporterCall::PixiInstallOnStarted { .. }
            | ReporterCall::PixiInstallOnFinished { .. }
            | ReporterCall::UrlCheckoutOnQueued { .. }
            | ReporterCall::UrlCheckoutOnStarted { .. }
            | ReporterCall::UrlCheckoutOnFinished { .. }
            | ReporterCall::InstantiateBackendOnQueued { .. }
            | ReporterCall::InstantiateBackendOnStarted { .. }
            | ReporterCall::InstantiateBackendOnFinished { .. }
            | ReporterCall::BuildBackendMetadataOnQueued { .. }
            | ReporterCall::BuildBackendMetadataOnStarted { .. }
            | ReporterCall::BuildBackendMetadataOnFinished { .. }
            | ReporterCall::SourceRecordOnQueued { .. }
            | ReporterCall::SourceRecordOnStarted { .. }
            | ReporterCall::SourceRecordOnFinished { .. }
            | ReporterCall::SourceMetadataOnQueued { .. }
            | ReporterCall::SourceMetadataOnStarted { .. }
            | ReporterCall::SourceMetadataOnFinished { .. }
            | ReporterCall::InstallOnTransactionOperationStart { .. }
            | ReporterCall::InstallOnLinkComplete { .. }
            | ReporterCall::InstallOnUnlinkComplete { .. }
            | ReporterCall::InstallOnPostLinkStart { .. }
            | ReporterCall::InstallOnPostLinkComplete { .. }
            | ReporterCall::InstallOnPreUnlinkStart { .. }
            | ReporterCall::InstallOnPreUnlinkComplete { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::io;
    use std::sync::Arc;

    use console::strip_ansi_codes;
    use indicatif::{ProgressDrawTarget, TermLike};
    use pixi_varlink::{PixiSolveEnvWire, ReporterCall, ReporterClient, TransactionOpWire};

    use super::*;

    /// Buffer-backed [`TermLike`] that splits indicatif's draw stream
    /// into distinct **frames** at every `flush()` call (indicatif
    /// flushes exactly once per draw cycle in `DrawState::draw_to_term`).
    /// Tests can then walk the frame sequence — not just the final
    /// rendered text — and catch UX regressions like "bar pops up
    /// only when finished" or "stuck on first state": a missing
    /// transition leaves a hole in the frame list.
    struct Capture {
        state: Arc<Mutex<CaptureState>>,
        width: u16,
    }

    #[derive(Default)]
    struct CaptureState {
        /// Text emitted in the current draw cycle (between flushes).
        current: String,
        /// All completed frames in order. Each entry is the raw
        /// (possibly ANSI-coloured) text indicatif wrote to render
        /// one bar update.
        frames: Vec<String>,
    }

    impl fmt::Debug for Capture {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Capture")
                .field("width", &self.width)
                .finish_non_exhaustive()
        }
    }

    /// Handle returned alongside the renderer; lets tests pull
    /// captured frames as they accumulate.
    #[derive(Clone)]
    struct CaptureHandle(Arc<Mutex<CaptureState>>);

    impl CaptureHandle {
        /// Snapshot of all frames captured so far, ANSI-stripped and
        /// trimmed of the trailing whitespace indicatif uses to
        /// pad each frame to terminal width.
        fn frames(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .frames
                .iter()
                .map(|f| strip_ansi_codes(f).trim_end().to_string())
                .collect()
        }

        /// Drains and prints the captured frames. Call after each
        /// state transition; with `cargo test -- --nocapture` you
        /// see the bar evolve frame-by-frame in the test log, which
        /// is the *only* reliable way to confirm progress bars
        /// actually move on a user's screen.
        fn dump(&self, label: &str) {
            let frames = self.frames();
            eprintln!(
                "── {label} ── ({} frame{})",
                frames.len(),
                if frames.len() == 1 { "" } else { "s" }
            );
            for (i, frame) in frames.iter().enumerate() {
                eprintln!("  [{i:>2}] {frame}");
            }
        }
    }

    impl Capture {
        fn new(width: u16) -> (Self, CaptureHandle) {
            let state = Arc::new(Mutex::new(CaptureState::default()));
            let handle = CaptureHandle(state.clone());
            (Self { state, width }, handle)
        }
    }

    impl TermLike for Capture {
        fn width(&self) -> u16 {
            self.width
        }
        fn height(&self) -> u16 {
            24
        }
        // Cursor moves are part of indicatif's redraw protocol but
        // carry no visible content, so we drop them silently. The
        // frame is reconstructed from `write_line` / `write_str`
        // alone, which is what the user actually sees.
        fn move_cursor_up(&self, _: usize) -> io::Result<()> {
            Ok(())
        }
        fn move_cursor_down(&self, _: usize) -> io::Result<()> {
            Ok(())
        }
        fn move_cursor_right(&self, _: usize) -> io::Result<()> {
            Ok(())
        }
        fn move_cursor_left(&self, _: usize) -> io::Result<()> {
            Ok(())
        }
        fn write_line(&self, s: &str) -> io::Result<()> {
            let mut state = self.state.lock().expect("capture state mutex poisoned");
            state.current.push_str(s);
            state.current.push('\n');
            Ok(())
        }
        fn write_str(&self, s: &str) -> io::Result<()> {
            self.state
                .lock()
                .expect("capture state mutex poisoned")
                .current
                .push_str(s);
            Ok(())
        }
        fn clear_line(&self) -> io::Result<()> {
            // No visible content; the next `write_str` overwrites
            // the line on a real terminal anyway.
            Ok(())
        }
        fn flush(&self) -> io::Result<()> {
            // End-of-draw boundary. Snapshot the current frame.
            // Indicatif sometimes flushes with nothing buffered
            // (e.g. when the bar hasn't moved); skip those.
            let mut state = self.state.lock().expect("capture state mutex poisoned");
            if !state.current.is_empty() {
                let frame = std::mem::take(&mut state.current);
                state.frames.push(frame);
            }
            Ok(())
        }
    }

    /// Build a renderer wired to a forced-draw `Capture`. The
    /// `term_like` draw target has no rate limiter, so every state
    /// change indicatif decides to draw lands as a frame in the
    /// capture — exactly what we need to verify movement.
    fn renderer_with_capture(width: u16) -> (WireReporterClient, CaptureHandle) {
        let (term, handle) = Capture::new(width);
        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::term_like(Box::new(term)));
        (WireReporterClient::new(multi), handle)
    }

    /// Frame guard: assert each subsequent state change adds at
    /// least one new frame. Catches "bar drew once, then never
    /// updated" regressions where the bar appears stuck.
    #[track_caller]
    fn expect_growth(handle: &CaptureHandle, before: usize, label: &str) -> usize {
        let after = handle.frames().len();
        assert!(
            after > before,
            "{label}: expected new frame(s) after the state change but frame count stayed at {before}"
        );
        after
    }

    /// Solve flow end-to-end: pixi-queue → pixi-start → pixi-finish.
    /// The wire format no longer carries parent context, so PixiSolve
    /// drives its own tracker on the bar directly.
    #[test]
    fn solve_bar_emits_frames_through_each_state() {
        let (client, h) = renderer_with_capture(120);

        let mut total = 0;

        client.on_call(ReporterCall::PixiSolveOnQueued {
            env: PixiSolveEnvWire {
                name: "xz".into(),
                platform: "linux-64".into(),
                has_direct_conda_dependency: false,
            },
            id: 1,
        });
        h.dump("after pixi-queued");
        total = expect_growth(&h, total, "pixi-queued");
        let last = h.frames().last().unwrap().clone();
        assert!(
            last.contains("solving") && last.contains("0/1"),
            "pixi-queued frame must show solving 0/1: {last:?}"
        );

        client.on_call(ReporterCall::PixiSolveOnStarted { id: 1 });
        h.dump("after pixi-started");
        total = expect_growth(&h, total, "pixi-started");
        let last = h.frames().last().unwrap().clone();
        assert!(
            last.contains("xz (linux-64)"),
            "started frame missing env label (MainProgressBar shows running items only): {last:?}"
        );
        assert!(
            last.contains("0/1"),
            "started frame must still show 0/1: {last:?}"
        );

        client.on_call(ReporterCall::PixiSolveOnFinished { id: 1 });
        h.dump("after pixi-finished");
        let _ = expect_growth(&h, total, "pixi-finished");
        let last = h.frames().last().unwrap().clone();
        assert!(
            last.contains("1/1"),
            "finished frame must show 1/1: {last:?}"
        );

        // Cross-check movement: 0/1 must appear at least twice (the
        // initial queue + the started-running frame), then 1/1 once
        // (the finish). A "bar shows up only when finished" bug
        // would leave only 1/1 frames.
        let frames = h.frames();
        let zero_count = frames.iter().filter(|f| f.contains("0/1")).count();
        let one_count = frames.iter().filter(|f| f.contains("1/1")).count();
        assert!(
            zero_count >= 2,
            "expected ≥2 frames showing 0/1 (queue + start), saw {zero_count}; frames: {frames:?}"
        );
        assert!(
            one_count >= 1,
            "expected ≥1 frame showing 1/1 (after finish), saw {one_count}; frames: {frames:?}"
        );
    }

    /// Top-level conda solve (no pixi parent context): gets its
    /// own bar entry, advances on its own start/finish.
    #[test]
    fn top_level_conda_solve_drives_bar() {
        let (client, h) = renderer_with_capture(120);

        client.on_call(ReporterCall::CondaSolveOnQueued {
            env: pixi_varlink::CondaSolveEnvWire {
                name: Some("orphan-solve".into()),
            },
            id: 7,
        });
        client.on_call(ReporterCall::CondaSolveOnStarted { id: 7 });
        client.on_call(ReporterCall::CondaSolveOnFinished { id: 7 });
        h.dump("orphan conda solve");

        let frames = h.frames();
        assert!(
            frames.iter().any(|f| f.contains("0/1")),
            "expected 0/1 in some frame; frames: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| f.contains("1/1")),
            "expected 1/1 (after finish) in some frame; frames: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| f.contains("orphan-solve")),
            "expected env label in some frame; frames: {frames:?}"
        );
    }

    /// Three solves queued (each as a pixi-solve with a nested
    /// conda-solve), finishing one at a time. The frame sequence
    /// must include every intermediate counter — 0/3, 1/3, 2/3, 3/3
    /// — proving each step actually drew. We don't assume frame
    /// count equals state-change count (indicatif may emit extras
    /// for hidden→visible transitions) but we do require each
    /// counter value show up at least once and in monotonic order.
    #[test]
    fn solve_bar_walks_through_all_counter_states() {
        let (client, h) = renderer_with_capture(120);

        for (i, name) in ["xz", "zlib", "openssl"].iter().enumerate() {
            let pixi_id: u64 = (i as u64) + 1;
            client.on_call(ReporterCall::PixiSolveOnQueued {
                env: PixiSolveEnvWire {
                    name: (*name).to_string(),
                    platform: "linux-64".into(),
                    has_direct_conda_dependency: false,
                },
                id: pixi_id,
            });
        }
        h.dump("after 3 queued");

        client.on_call(ReporterCall::PixiSolveOnStarted { id: 1 });
        client.on_call(ReporterCall::PixiSolveOnFinished { id: 1 });
        h.dump("after 1st finished");

        client.on_call(ReporterCall::PixiSolveOnFinished { id: 2 });
        h.dump("after 2nd finished");

        client.on_call(ReporterCall::PixiSolveOnFinished { id: 3 });
        h.dump("after 3rd finished");

        let frames = h.frames();
        for expected in ["0/3", "1/3", "2/3", "3/3"] {
            assert!(
                frames.iter().any(|f| f.contains(expected)),
                "no frame ever showed {expected}; this is the 'bar stuck' bug — frames: {frames:?}"
            );
        }

        // First-mention ordering: the counter states must appear in
        // monotonic order across the frame stream. `0/3` must show
        // up before `1/3`, etc. Catches "all four counters appear,
        // but in the wrong order" — which would mean the renderer
        // is mismapping wire ids.
        let first_idx = |needle: &str| frames.iter().position(|f| f.contains(needle));
        let i_0 = first_idx("0/3").expect("0/3");
        let i_1 = first_idx("1/3").expect("1/3");
        let i_2 = first_idx("2/3").expect("2/3");
        let i_3 = first_idx("3/3").expect("3/3");
        assert!(
            i_0 < i_1 && i_1 < i_2 && i_2 < i_3,
            "counter states must appear in 0→1→2→3 order; saw indexes {i_0}, {i_1}, {i_2}, {i_3}; frames: {frames:?}"
        );
    }

    /// Install bar: start a transaction with 3 real ops + 1 `None`,
    /// then complete each op. Same frame-sequence guard as the
    /// solve test plus a `None`-slot test (operation index 2 is a
    /// hole; index 3 is the third real op).
    #[test]
    fn install_bar_walks_through_all_counter_states() {
        let (client, h) = renderer_with_capture(120);

        client.on_call(ReporterCall::InstallOnTransactionStart {
            operations: vec![
                Some(TransactionOpWire {
                    name: "libgomp".into(),
                    size: 100,
                }),
                Some(TransactionOpWire {
                    name: "xz".into(),
                    size: 200,
                }),
                None, // hole: must not consume a counter slot
                Some(TransactionOpWire {
                    name: "openssl".into(),
                    size: 300,
                }),
            ],
        });
        h.dump("after transaction start");
        {
            let frames = h.frames();
            let last = frames.last().unwrap();
            assert!(
                last.contains("installing"),
                "install bar prefix missing: {last:?}"
            );
            assert!(
                last.contains("0/3"),
                "transaction start frame must show 0/3 (hole excluded): {last:?}"
            );
        }

        // Finish op 0 (libgomp).
        client.on_call(ReporterCall::InstallOnLinkStart {
            operation: 0,
            package: "libgomp".into(),
            id: 10,
        });
        client.on_call(ReporterCall::InstallOnTransactionOperationComplete { operation: 0 });
        h.dump("after op 0 complete");

        // Finish op 3 (openssl) — operation index 2 was the hole.
        client.on_call(ReporterCall::InstallOnTransactionOperationComplete { operation: 3 });
        h.dump("after op 3 complete");

        // Finish op 1 (xz).
        client.on_call(ReporterCall::InstallOnTransactionOperationComplete { operation: 1 });
        h.dump("after op 1 complete");

        let frames = h.frames();
        for expected in ["0/3", "1/3", "2/3", "3/3"] {
            assert!(
                frames.iter().any(|f| f.contains(expected)),
                "install bar never reached {expected}; frames: {frames:?}"
            );
        }
        let first_idx = |needle: &str| frames.iter().position(|f| f.contains(needle));
        let i_0 = first_idx("0/3").expect("0/3");
        let i_1 = first_idx("1/3").expect("1/3");
        let i_2 = first_idx("2/3").expect("2/3");
        let i_3 = first_idx("3/3").expect("3/3");
        assert!(
            i_0 < i_1 && i_1 < i_2 && i_2 < i_3,
            "install counter must appear in order; saw {i_0},{i_1},{i_2},{i_3}; frames: {frames:?}"
        );
    }

    /// Variants the renderer deliberately doesn't render (matching
    /// `TopLevelProgress`'s no-op impls — `PixiInstall`,
    /// `InstantiateBackend`, source metadata, etc.) must produce no
    /// frames at all when sent in isolation. This pins that "no-op"
    /// really means no draw, not "draws an empty bar".
    #[test]
    fn unrendered_variants_emit_no_frames() {
        let (client, h) = renderer_with_capture(120);

        client.on_call(ReporterCall::PixiInstallOnQueued {
            env: pixi_varlink::InstallEnvWire {
                name: "ignored-env".into(),
            },
            id: 1,
        });
        client.on_call(ReporterCall::PixiInstallOnStarted { id: 1 });
        client.on_call(ReporterCall::InstantiateBackendOnQueued {
            spec: "ignored-backend-spec".into(),
            id: 1,
        });
        client.on_call(ReporterCall::SourceRecordOnQueued {
            spec: "ignored-source-rec".into(),
            id: 1,
        });
        client.on_call(ReporterCall::GitCheckoutOnQueued {
            url: "https://example.test/repo.git".into(),
            reference: "HEAD".into(),
            id: 1,
        });
        h.dump("no-op variants");

        let frames = h.frames();
        for frame in &frames {
            for needle in ["ignored-env", "ignored-backend-spec", "ignored-source-rec"] {
                assert!(
                    !frame.contains(needle),
                    "no-op variant leaked {needle} into a frame: {frame:?}"
                );
            }
        }
    }

    /// Git checkout flow: `OnQueued` stashes the (url, reference)
    /// pair without drawing; `OnStarted` adds a spinner bar with the
    /// `fetching git dependencies` prefix and a `checking out
    /// <url>@<ref>` message; `OnFinished` clears it. Mirrors what
    /// `GitCheckoutProgress` does in the local install path.
    #[test]
    fn git_checkout_renders_spinner_with_url_and_ref() {
        let (client, h) = renderer_with_capture(120);

        client.on_call(ReporterCall::GitCheckoutOnQueued {
            url: "https://example.test/foo.git".into(),
            reference: "branch:main".into(),
            id: 7,
        });
        // Queue alone draws nothing — bar only appears once started.
        let queued_frames = h.frames();
        assert!(
            !queued_frames.iter().any(|f| f.contains("fetching git")),
            "queue alone shouldn't draw; frames: {queued_frames:?}"
        );

        client.on_call(ReporterCall::GitCheckoutOnStarted { id: 7 });
        h.dump("after git checkout started");
        let frames = h.frames();
        assert!(
            frames
                .iter()
                .any(|f| f.contains("fetching git dependencies")),
            "expected git bar prefix; frames: {frames:?}"
        );
        assert!(
            frames
                .iter()
                .any(|f| f.contains("https://example.test/foo.git") && f.contains("branch:main")),
            "expected url@reference message; frames: {frames:?}"
        );

        client.on_call(ReporterCall::GitCheckoutOnFinished { id: 7 });
        h.dump("after git checkout finished");
        // No assertion on post-finish frames — `finish_and_clear`
        // erases the bar; what matters is the call doesn't panic
        // and removes internal state (an `OnFinished` arm relies on
        // the bar having been removed from `git_bars`).
    }

    /// Out-of-order events (`InstallOnLinkStart` for a `None` slot)
    /// must no-op silently. Frame count after the dance is allowed
    /// to be zero (no work => nothing drawn) but must not panic.
    #[test]
    fn unknown_operation_index_does_not_panic() {
        let (client, h) = renderer_with_capture(120);

        client.on_call(ReporterCall::InstallOnTransactionStart {
            operations: vec![None, None],
        });
        client.on_call(ReporterCall::InstallOnLinkStart {
            operation: 0,
            package: "phantom".into(),
            id: 1,
        });
        client.on_call(ReporterCall::InstallOnTransactionOperationComplete { operation: 0 });
        h.dump("phantom op sequence");
    }

    /// Cache prep bar: `InstallOnPopulateCacheStart` queues an
    /// entry, `Validate*` and `Download*` toggle its inner state,
    /// `PopulateCacheComplete` retires it. The frame stream must
    /// show the package label appearing during work and the bar
    /// growing through `0/1` then completing.
    #[test]
    fn prep_bar_lifecycle_for_one_package() {
        let (client, h) = renderer_with_capture(120);

        // TransactionStart populates op_meta so PopulateCacheStart
        // can look up the size; without it the bar still works but
        // the size is None.
        client.on_call(ReporterCall::InstallOnTransactionStart {
            operations: vec![Some(TransactionOpWire {
                name: "libgomp".into(),
                size: 1234,
            })],
        });

        // Cache entry for op 0. Server-side wire id = 42. The prep
        // bar deliberately doesn't draw on Pending state — only
        // once an entry transitions to Validating / Downloading /
        // Building does indicatif emit a frame. Local UX matches:
        // the bar appears when work begins, not when work queues.
        client.on_call(ReporterCall::InstallOnPopulateCacheStart {
            operation: 0,
            package: "libgomp".into(),
            id: 42,
        });
        client.on_call(ReporterCall::InstallOnValidateStart {
            cache_entry: 42,
            id: 43,
        });
        h.dump("after validate-start");
        let frames = h.frames();
        assert!(
            frames.iter().any(|f| f.contains("preparing packages")),
            "prep bar prefix missing after validate-start; frames: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| f.contains("libgomp")),
            "package label missing from prep bar frames: {frames:?}"
        );
        client.on_call(ReporterCall::InstallOnValidateComplete { validate_idx: 42 });
        client.on_call(ReporterCall::InstallOnDownloadStart {
            cache_entry: 42,
            id: 44,
        });
        client.on_call(ReporterCall::InstallOnDownloadProgress {
            download_idx: 42,
            progress: 600,
            total: Some(1234),
        });
        client.on_call(ReporterCall::InstallOnDownloadCompleted { download_idx: 42 });
        client.on_call(ReporterCall::InstallOnPopulateCacheComplete { cache_entry: 42 });
        h.dump("after full prep");

        let frames = h.frames();
        // Bar advances 0/1 → 1/1 across the lifecycle.
        assert!(
            frames.iter().any(|f| f.contains("0/1")),
            "expected 0/1 frame in prep bar; frames: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| f.contains("1/1")),
            "expected 1/1 frame after PopulateCacheComplete; frames: {frames:?}"
        );
        let i_zero = frames.iter().position(|f| f.contains("0/1")).unwrap();
        let i_one = frames.iter().position(|f| f.contains("1/1")).unwrap();
        assert!(
            i_zero < i_one,
            "0/1 must appear before 1/1; saw {i_zero},{i_one}; frames: {frames:?}"
        );
    }

    /// Source build: `BackendSourceBuildOnQueued` adds a "building
    /// <pkg>" entry on the prep bar, `OnStarted` flips it to
    /// active, `OnFinished` retires it. Same bar as cache-prep.
    #[test]
    fn source_build_drives_prep_bar() {
        let (client, h) = renderer_with_capture(120);

        // Queue is Pending — prep bar doesn't draw yet (matches
        // local UX: build bar appears once work begins).
        client.on_call(ReporterCall::BackendSourceBuildOnQueued {
            package: "my-source-pkg".into(),
            id: 1,
        });
        client.on_call(ReporterCall::BackendSourceBuildOnStarted { id: 1 });
        h.dump("after source build started");

        let frames = h.frames();
        assert!(
            frames.iter().any(|f| f.contains("preparing packages")),
            "source build must use the prep bar; frames: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| f.contains("building my-source-pkg")),
            "expected 'building <pkg>' label; frames: {frames:?}"
        );

        client.on_call(ReporterCall::BackendSourceBuildOnFinished {
            id: 1,
            failed: false,
        });
        h.dump("after source build finished");

        let frames = h.frames();
        assert!(
            frames.iter().any(|f| f.contains("1/1")),
            "expected 1/1 after build finished; frames: {frames:?}"
        );
    }
}
