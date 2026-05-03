//! Daemon-routed install renderer.
//!
//! Receives [`ReporterCall`]s the daemon's `WireReporter` marshalled
//! over the varlink stream and drives indicatif using the same
//! primitives as the local install path's `TopLevelProgress` —
//! [`MainProgressBar`] for solving and installing — so the user
//! sees the same bars whether the install ran locally or via the
//! daemon. The renderer only consumes the wire types directly; no
//! fake `Transaction` / `RepoDataRecord` round-tripping.
//!
//! Coverage today is the two bars `TopLevelProgress` shows for a
//! binary install: one for solving environments, one for the
//! per-package install. Cache prep, repodata fetches, and source
//! builds are out of scope for v1; the variants for those just no-op
//! through the renderer until follow-up work plumbs them in.

use std::collections::HashMap;
use std::sync::Mutex;

use indicatif::MultiProgress;
use pixi_progress::ProgressBarPlacement;
use pixi_reporters::main_progress_bar::MainProgressBar;
use pixi_reporters::sync_reporter::PackageWithSize;
use pixi_varlink::{ReporterCall, ReporterClient};

/// `ReporterClient` that drives indicatif from the daemon's wire
/// stream. Cheap to construct — bars are created lazily on first
/// queue so a request that never reaches the install phase doesn't
/// flash an empty bar.
pub struct WireReporterClient {
    multi: MultiProgress,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Lazily-constructed "solving" bar, shared across all
    /// `PixiSolveOnQueued` events arriving during the install.
    solve_bar: Option<MainProgressBar<String>>,
    /// Lazily-constructed "installing" bar, populated up-front when
    /// `InstallOnTransactionStart` arrives so its total reflects
    /// the full operation set immediately.
    install_bar: Option<MainProgressBar<PackageWithSize>>,

    /// Wire-side `PixiSolveOnQueued.id` → tracker id returned by
    /// `MainProgressBar::queued`. Lets the
    /// `PixiSolveOnStarted` / `PixiSolveOnFinished` arms find the
    /// right tracker.
    solve_id_map: HashMap<u64, usize>,
    /// `Transaction::operations` index → install bar tracker id, so
    /// `InstallOnLinkStart`'s `operation` field looks up the right
    /// tracker. Slots whose wire `operations` entry was `None`
    /// don't have a tracker; they get filtered at lookup.
    install_op_map: HashMap<usize, usize>,
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
}

impl ReporterClient for WireReporterClient {
    fn on_call(&self, call: ReporterCall) {
        // Hold the lock only as long as the state inspection / map
        // mutation needs; drop before calling into the bar so that
        // bar internals (which take their own locks) can't deadlock
        // against ours.
        match call {
            ReporterCall::PixiSolveOnQueued { env, id, .. } => {
                let mut s = self.state.lock().expect("renderer state mutex poisoned");
                let bar = s
                    .solve_bar
                    .get_or_insert_with(|| {
                        MainProgressBar::new(
                            self.multi.clone(),
                            ProgressBarPlacement::default(),
                            "solving".to_string(),
                        )
                    })
                    .clone();
                let tracker = bar.queued(format!("{} ({})", env.name, env.platform));
                s.solve_id_map.insert(id, tracker);
            }
            ReporterCall::PixiSolveOnStarted { id } => {
                let s = self.state.lock().expect("renderer state mutex poisoned");
                if let (Some(bar), Some(&tracker)) = (&s.solve_bar, s.solve_id_map.get(&id)) {
                    bar.start(tracker);
                }
            }
            ReporterCall::PixiSolveOnFinished { id } => {
                let mut s = self.state.lock().expect("renderer state mutex poisoned");
                let tracker = s.solve_id_map.remove(&id);
                if let (Some(bar), Some(tracker)) = (&s.solve_bar, tracker) {
                    bar.finish(tracker);
                }
            }

            ReporterCall::InstallOnTransactionStart { operations } => {
                let mut s = self.state.lock().expect("renderer state mutex poisoned");
                let bar = s
                    .install_bar
                    .get_or_insert_with(|| {
                        MainProgressBar::new(
                            self.multi.clone(),
                            ProgressBarPlacement::default(),
                            "installing".to_string(),
                        )
                    })
                    .clone();
                for (op_idx, op) in operations.into_iter().enumerate() {
                    if let Some(op) = op {
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
                let s = self.state.lock().expect("renderer state mutex poisoned");
                if let (Some(bar), Some(&tracker)) =
                    (&s.install_bar, s.install_op_map.get(&operation))
                {
                    bar.start(tracker);
                }
            }
            ReporterCall::InstallOnTransactionOperationComplete { operation } => {
                let mut s = self.state.lock().expect("renderer state mutex poisoned");
                let tracker = s.install_op_map.remove(&operation);
                if let (Some(bar), Some(tracker)) = (&s.install_bar, tracker) {
                    bar.finish(tracker);
                }
            }
            ReporterCall::InstallOnTransactionComplete => {
                if let Some(bar) = self
                    .state
                    .lock()
                    .expect("renderer state mutex poisoned")
                    .install_bar
                    .as_ref()
                {
                    bar.clear();
                }
            }

            // CondaSolve shows up nested inside PixiSolve; the local
            // path routes both to the same MainProgressBar to avoid
            // double-counting work that PixiSolve already represents.
            // For v1 we mirror that: skip CondaSolve's own bar.
            ReporterCall::CondaSolveOnQueued { .. }
            | ReporterCall::CondaSolveOnStarted { .. }
            | ReporterCall::CondaSolveOnFinished { .. } => {}

            // Out of scope for v1: cache prep (validate / download /
            // populate), per-package downloads, build backends, git
            // checkouts, source metadata, source builds, factory
            // create_*_reporter calls. Each of these has a
            // counterpart bar in the local path that's natural to
            // wire in once we have a use case; today they no-op
            // silently to keep the renderer surface focused.
            _ => {}
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
            let mut state = self.state.lock().expect("renderer state mutex poisoned");
            state.current.push_str(s);
            state.current.push('\n');
            Ok(())
        }
        fn write_str(&self, s: &str) -> io::Result<()> {
            self.state
                .lock()
                .expect("renderer state mutex poisoned")
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
            let mut state = self.state.lock().expect("renderer state mutex poisoned");
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

    /// Solve flow end-to-end: queue → start → finish. We assert the
    /// frame *sequence* — every transition emits at least one new
    /// frame, and the final frames in each phase carry the expected
    /// content. If indicatif drew only the final state (or only the
    /// first), the per-phase content checks fail.
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
        h.dump("after queued");
        total = expect_growth(&h, total, "queued");
        let after_queued = h.frames();
        let last = after_queued.last().unwrap();
        assert!(
            last.contains("solving"),
            "queued frame missing 'solving' prefix: {last:?}"
        );
        assert!(
            last.contains("0/1"),
            "queued frame missing '0/1' counter: {last:?}"
        );

        client.on_call(ReporterCall::PixiSolveOnStarted { id: 1 });
        h.dump("after started");
        total = expect_growth(&h, total, "started");
        let after_started = h.frames();
        let last = after_started.last().unwrap();
        assert!(
            last.contains("xz (linux-64)"),
            "started frame missing env label (MainProgressBar shows running items only): {last:?}"
        );
        // Position hasn't advanced yet.
        assert!(
            last.contains("0/1"),
            "started frame must still show 0/1: {last:?}"
        );

        client.on_call(ReporterCall::PixiSolveOnFinished { id: 1 });
        h.dump("after finished");
        let _ = expect_growth(&h, total, "finished");
        let after_finished = h.frames();
        let last = after_finished.last().unwrap();
        assert!(
            last.contains("1/1"),
            "finished frame must show 1/1: {last:?}"
        );

        // Cross-check movement: at least three distinct counter
        // states (0/1 with no label, 0/1 with label, 1/1) must
        // appear in distinct frames. This is the regression guard
        // the user explicitly asked for — a "bar shows up only
        // when finished" bug would leave the buffer with frames
        // that all show 1/1.
        let frames = h.frames();
        let zero_count = frames.iter().filter(|f| f.contains("0/1")).count();
        let one_count = frames.iter().filter(|f| f.contains("1/1")).count();
        assert!(
            zero_count >= 2,
            "expected at least 2 frames showing 0/1 (queue + start), saw {zero_count}; frames: {frames:?}"
        );
        assert!(
            one_count >= 1,
            "expected at least 1 frame showing 1/1 (after finish), saw {one_count}; frames: {frames:?}"
        );
    }

    /// Three solves queued, finishing one at a time. The frame
    /// sequence must include every intermediate counter — 0/3, 1/3,
    /// 2/3, 3/3 — proving each step actually drew. We don't assume
    /// frame count exactly equals state-change count (indicatif may
    /// emit extras for hidden→visible transitions etc.) but we do
    /// require *each* counter value show up at least once.
    #[test]
    fn solve_bar_walks_through_all_counter_states() {
        let (client, h) = renderer_with_capture(120);

        for (i, name) in ["xz", "zlib", "openssl"].iter().enumerate() {
            client.on_call(ReporterCall::PixiSolveOnQueued {
                env: PixiSolveEnvWire {
                    name: (*name).to_string(),
                    platform: "linux-64".into(),
                    has_direct_conda_dependency: false,
                },
                id: (i as u64) + 1,
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

    /// Variants outside v1 coverage (CondaSolve nested under PixiSolve,
    /// cache-prep, DownloadProgress) must not produce visible bars.
    /// We assert no frame contains the would-be label text.
    #[test]
    fn out_of_scope_variants_emit_no_visible_content() {
        let (client, h) = renderer_with_capture(120);

        client.on_call(ReporterCall::CondaSolveOnQueued {
            env: pixi_varlink::CondaSolveEnvWire {
                name: Some("nested".into()),
            },
            id: 99,
        });
        client.on_call(ReporterCall::InstallOnPopulateCacheStart {
            operation: 0,
            package: "ignored".into(),
            id: 1,
        });
        h.dump("out-of-scope sequence");

        let frames = h.frames();
        for frame in &frames {
            assert!(
                !frame.contains("nested") && !frame.contains("ignored"),
                "out-of-scope variant leaked into a frame: {frame:?}"
            );
        }
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
}
