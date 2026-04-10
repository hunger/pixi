use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;

use pixi_command_dispatcher::reporter::{
    CondaSolveId, CondaSolveReporter, PixiInstallId, PixiInstallReporter, PixiSolveId,
    PixiSolveReporter, Reporter, ReporterContext,
};
use pixi_command_dispatcher::{
    InstallPixiEnvironmentSpec, PixiEnvironmentSpec, SolveCondaEnvironmentSpec,
};

use crate::dev_prefix_pixi;

/// Shared counter for progress IDs, allowing callers outside the reporter
/// to allocate IDs from the same sequence.
pub type ProgressIdCounter = Arc<AtomicUsize>;

/// A reporter that sends progress messages through a channel
/// for real-time streaming to a varlink client.
pub struct VarlinkReporter {
    tx: mpsc::UnboundedSender<dev_prefix_pixi::Progress>,
    id_counter: ProgressIdCounter,
}

impl VarlinkReporter {
    pub fn new(
        tx: mpsc::UnboundedSender<dev_prefix_pixi::Progress>,
        id_counter: ProgressIdCounter,
    ) -> Self {
        Self { tx, id_counter }
    }

    fn send(&self, msg: dev_prefix_pixi::Progress) {
        let _ = self.tx.send(msg);
    }

    fn next_id(&self) -> usize {
        self.id_counter.fetch_add(1, Ordering::Relaxed)
    }
}

impl Reporter for VarlinkReporter {
    fn on_start(&mut self) {
        self.send(dev_prefix_pixi::Progress {
            progress_bar: dev_prefix_pixi::ProgressBar::Global,
            progress_state: dev_prefix_pixi::ProgressState::Started,
            id: i64::MIN,
        });
    }

    fn on_finished(&mut self) {
        self.send(dev_prefix_pixi::Progress {
            progress_bar: dev_prefix_pixi::ProgressBar::Global,
            progress_state: dev_prefix_pixi::ProgressState::Finished,
            id: i64::MIN,
        });
    }

    fn as_conda_solve_reporter(&mut self) -> Option<&mut dyn CondaSolveReporter> {
        Some(self)
    }

    fn as_pixi_solve_reporter(&mut self) -> Option<&mut dyn PixiSolveReporter> {
        Some(self)
    }

    fn as_pixi_install_reporter(&mut self) -> Option<&mut dyn PixiInstallReporter> {
        Some(self)
    }
}

impl CondaSolveReporter for VarlinkReporter {
    fn on_queued(
        &mut self,
        _: Option<ReporterContext>,
        _: &SolveCondaEnvironmentSpec,
    ) -> CondaSolveId {
        let id = self.next_id();
        self.send(dev_prefix_pixi::Progress {
            progress_bar: dev_prefix_pixi::ProgressBar::CondaSolve,
            progress_state: dev_prefix_pixi::ProgressState::Queued,
            id: id as i64,
        });
        CondaSolveId(id)
    }
    fn on_start(&mut self, id: CondaSolveId) {
        self.send(dev_prefix_pixi::Progress {
            progress_bar: dev_prefix_pixi::ProgressBar::CondaSolve,
            progress_state: dev_prefix_pixi::ProgressState::Started,
            id: id.0 as i64,
        });
    }
    fn on_finished(&mut self, id: CondaSolveId) {
        self.send(dev_prefix_pixi::Progress {
            progress_bar: dev_prefix_pixi::ProgressBar::CondaSolve,
            progress_state: dev_prefix_pixi::ProgressState::Finished,
            id: id.0 as i64,
        });
    }
}

impl PixiSolveReporter for VarlinkReporter {
    fn on_queued(&mut self, _: Option<ReporterContext>, _: &PixiEnvironmentSpec) -> PixiSolveId {
        let id = self.next_id();
        self.send(dev_prefix_pixi::Progress {
            progress_bar: dev_prefix_pixi::ProgressBar::PixiSolve,
            progress_state: dev_prefix_pixi::ProgressState::Queued,
            id: id as i64,
        });
        PixiSolveId(id)
    }
    fn on_start(&mut self, id: PixiSolveId) {
        self.send(dev_prefix_pixi::Progress {
            progress_bar: dev_prefix_pixi::ProgressBar::PixiSolve,
            progress_state: dev_prefix_pixi::ProgressState::Started,
            id: id.0 as i64,
        });
    }
    fn on_finished(&mut self, id: PixiSolveId) {
        self.send(dev_prefix_pixi::Progress {
            progress_bar: dev_prefix_pixi::ProgressBar::PixiSolve,
            progress_state: dev_prefix_pixi::ProgressState::Started,
            id: id.0 as i64,
        });
    }
}

impl PixiInstallReporter for VarlinkReporter {
    fn on_queued(
        &mut self,
        _: Option<ReporterContext>,
        _: &InstallPixiEnvironmentSpec,
    ) -> PixiInstallId {
        let id = self.next_id();
        self.send(dev_prefix_pixi::Progress {
            progress_bar: dev_prefix_pixi::ProgressBar::PixiInstall,
            progress_state: dev_prefix_pixi::ProgressState::Queued,
            id: id as i64,
        });
        PixiInstallId(id)
    }
    fn on_start(&mut self, id: PixiInstallId) {
        self.send(dev_prefix_pixi::Progress {
            progress_bar: dev_prefix_pixi::ProgressBar::PixiInstall,
            progress_state: dev_prefix_pixi::ProgressState::Started,
            id: id.0 as i64,
        });
    }
    fn on_finished(&mut self, id: PixiInstallId) {
        self.send(dev_prefix_pixi::Progress {
            progress_bar: dev_prefix_pixi::ProgressBar::PixiInstall,
            progress_state: dev_prefix_pixi::ProgressState::Started,
            id: id.0 as i64,
        });
    }
}
