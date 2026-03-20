use tokio::sync::mpsc;

use pixi_command_dispatcher::reporter::{
    CondaSolveId, CondaSolveReporter, PixiInstallId, PixiInstallReporter, PixiSolveId,
    PixiSolveReporter, Reporter, ReporterContext,
};
use pixi_command_dispatcher::{
    InstallPixiEnvironmentSpec, PixiEnvironmentSpec, SolveCondaEnvironmentSpec,
};

/// A reporter that sends progress messages through a channel
/// for real-time streaming to a varlink client.
pub struct VarlinkReporter {
    tx: mpsc::UnboundedSender<String>,
    next_id: usize,
}

impl VarlinkReporter {
    pub fn new(tx: mpsc::UnboundedSender<String>) -> Self {
        Self { tx, next_id: 0 }
    }

    fn send(&self, msg: &str) {
        let _ = self.tx.send(msg.to_string());
    }

    fn next_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

impl Reporter for VarlinkReporter {
    fn on_start(&mut self) {
        self.send("started");
    }

    fn on_finished(&mut self) {
        self.send("finished");
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
    fn on_queued(&mut self, _: Option<ReporterContext>, _: &SolveCondaEnvironmentSpec) -> CondaSolveId {
        self.send("solving: queued");
        CondaSolveId(self.next_id())
    }
    fn on_start(&mut self, _: CondaSolveId) {
        self.send("solving: started");
    }
    fn on_finished(&mut self, _: CondaSolveId) {
        self.send("solving: finished");
    }
}

impl PixiSolveReporter for VarlinkReporter {
    fn on_queued(&mut self, _: Option<ReporterContext>, _: &PixiEnvironmentSpec) -> PixiSolveId {
        self.send("pixi solve: queued");
        PixiSolveId(self.next_id())
    }
    fn on_start(&mut self, _: PixiSolveId) {
        self.send("pixi solve: started");
    }
    fn on_finished(&mut self, _: PixiSolveId) {
        self.send("pixi solve: finished");
    }
}

impl PixiInstallReporter for VarlinkReporter {
    fn on_queued(&mut self, _: Option<ReporterContext>, _: &InstallPixiEnvironmentSpec) -> PixiInstallId {
        self.send("install: queued");
        PixiInstallId(self.next_id())
    }
    fn on_start(&mut self, _: PixiInstallId) {
        self.send("install: started");
    }
    fn on_finished(&mut self, _: PixiInstallId) {
        self.send("install: finished");
    }
}
