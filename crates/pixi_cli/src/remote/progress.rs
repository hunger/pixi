use indicatif::ProgressBar;
use pixi_reporters::main_progress_bar::MainProgressBar;
use pixi_varlink::Progress;

/// Drives progress bars from server-streamed messages, using the same
/// [`MainProgressBar`] that `pixi global install` uses for solving.
pub struct RemoteProgress {
    solve_bar: MainProgressBar<String>,
    install_bar: MainProgressBar<String>,
    solve_id: Option<usize>,
    install_id: Option<usize>,
}

impl RemoteProgress {
    pub fn new() -> Self {
        let multi = pixi_progress::global_multi_progress();
        let anchor = multi.add(ProgressBar::hidden());
        let solve_bar = MainProgressBar::new(
            multi.clone(),
            pixi_progress::ProgressBarPlacement::Before(anchor.clone()),
            "solving".to_owned(),
        );
        let install_bar = MainProgressBar::new(
            multi,
            pixi_progress::ProgressBarPlacement::Before(anchor),
            "installing".to_owned(),
        );
        Self {
            solve_bar,
            install_bar,
            solve_id: None,
            install_id: None,
        }
    }

    pub fn on_message(&mut self, progress: &Progress) {
        let msg = &progress.message;
        // The eprintln flushes stderr which triggers indicatif to redraw.
        eprintln!("[remote progress] {msg}");
        match msg.as_str() {
            "pixi solve: queued" => {
                let id = self.solve_bar.queued("remote".to_owned());
                self.solve_id = Some(id);
            }
            "solving: started" => {
                if let Some(id) = self.solve_id {
                    self.solve_bar.start(id);
                }
            }
            "solving: finished" => {
                if let Some(id) = self.solve_id {
                    self.solve_bar.finish(id);
                }
            }
            "pixi solve: finished" => {
                self.solve_bar.clear();
            }
            "install: queued" => {
                let id = self.install_bar.queued("remote".to_owned());
                self.install_id = Some(id);
            }
            "install: started" => {
                if let Some(id) = self.install_id {
                    self.install_bar.start(id);
                }
            }
            "install: finished" => {
                if let Some(id) = self.install_id {
                    self.install_bar.finish(id);
                }
            }
            _ => {}
        }
    }

    pub fn finish(&mut self) {
        self.solve_bar.clear();
        self.install_bar.clear();
    }
}
