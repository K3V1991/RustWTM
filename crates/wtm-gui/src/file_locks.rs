//! File-centric diagnostics; results are a snapshot, not an assertion a file is free.
use eframe::egui::{self, Ui};
use wtm_core::control::ProcessRef;
#[derive(Default)]
pub struct View {
    path: String,
    task: crate::background::Task<wtm_sys::file_locks::Report>,
    fetching: String,
    /// The latest explicitly requested path should be scanned as soon as the worker is free.
    queued: bool,
    result: Option<Result<wtm_sys::file_locks::Report, String>>,
}
impl View {
    pub fn dropped(&mut self, path: &std::path::Path, ctx: &egui::Context) {
        self.path = path.to_string_lossy().into_owned();
        self.start(ctx);
    }
    fn start(&mut self, ctx: &egui::Context) {
        self.result = None;
        self.queued = true;
        self.start_queued(ctx);
    }
    fn start_queued(&mut self, ctx: &egui::Context) {
        if self.queued && !self.task.busy() {
            self.queued = false;
            self.fetching.clone_from(&self.path);
            let path = std::path::PathBuf::from(&self.path);
            self.task
                .start(ctx, move || wtm_sys::file_locks::scan(&path));
        }
    }
    pub fn show(&mut self, ui: &mut Ui) -> Option<ProcessRef> {
        if let Some(result) = self.task.poll()
            && !self.queued
            && self.fetching == self.path
        {
            self.result = Some(result);
        }
        ui.weak(crate::i18n::tr(
            "Drop a local file or folder, or enter its full path. This query never closes handles.",
        ));
        let mut start = false;
        ui.horizontal_wrapped(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.path)
                    .desired_width(ui.available_width().min(520.0))
                    .char_limit(2048)
                    .hint_text(crate::i18n::tr("Full file or folder path")),
            );
            if response.changed() {
                self.result = None;
                self.queued = false;
            }
            start = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            start |= ui
                .add_enabled(
                    !self.task.busy() && !self.path.trim().is_empty(),
                    egui::Button::new(crate::i18n::tr("Find file owners")),
                )
                .clicked();
        });
        if start {
            self.start(ui.ctx());
        }
        self.start_queued(ui.ctx());
        if self.task.busy() {
            ui.spinner();
        }
        let mut selected = None;
        if let Some(result) = &self.result {
            match result {
                Err(e) => {
                    crate::theme::colored(ui, crate::theme::WARN, e);
                }
                Ok(report) => {
                    crate::theme::card(ui, crate::i18n::tr("File owners"), |ui| {
                        let path = ui.label(&report.path);
                        crate::context_actions::copy_text(
                            &path,
                            ("file-owner-path", &report.path),
                            crate::i18n::tr("Path"),
                            &report.path,
                        );
                        ui.weak(crate::i18n::format(
                            "{0} files checked",
                            &[report.files_checked.to_string()],
                        ));
                        for warning in &report.warnings {
                            crate::theme::colored(ui, crate::theme::WARN, warning);
                        }
                        if report.lockers.is_empty() {
                            ui.label(crate::i18n::tr("No owners were reported. Protected processes, directories and mappings may not be reported by Windows."));
                        }
                        for locker in &report.lockers {
                            ui.horizontal_wrapped(|ui| {
                                let name = ui.strong(&locker.name).interact(egui::Sense::click());
                                crate::context_actions::menu(
                                    &name,
                                    ("file-owner", locker.pid, locker.create_time),
                                    |ui| {
                                        crate::context_actions::copy(ui, "PID", || {
                                            locker.pid.to_string()
                                        });
                                        if crate::context_actions::action(
                                            ui,
                                            crate::i18n::tr("Inspect process"),
                                        ) {
                                            selected = Some(ProcessRef {
                                                pid: locker.pid,
                                                create_time: locker.create_time,
                                                name: locker.name.clone(),
                                            });
                                        }
                                    },
                                );
                                ui.weak(format!("PID {}", locker.pid));
                                if !locker.service.is_empty() {
                                    ui.label(&locker.service);
                                }
                                if locker.reason == wtm_sys::file_locks::Hold::WorkingDirectory {
                                    ui.weak(crate::i18n::tr("Working directory")).on_hover_text(
                                        crate::i18n::tr(
                                            "It works in this folder or one inside it, so the folder cannot be removed or renamed until it moves elsewhere or ends.",
                                        ),
                                    );
                                }
                                if ui.button(crate::i18n::tr("Inspect process")).clicked() {
                                    selected = Some(ProcessRef {
                                        pid: locker.pid,
                                        create_time: locker.create_time,
                                        name: locker.name.clone(),
                                    });
                                }
                            });
                        }
                    });
                }
            }
        }
        selected
    }
}

#[cfg(test)]
impl View {
    pub fn fixture() -> Self {
        Self {
            path: r"C:\Example\report.xlsx".into(),
            result: Some(Ok(wtm_sys::file_locks::Report {
                path: r"C:\Example\report.xlsx".into(),
                files_checked: 1,
                partial: false,
                warnings: vec![],
                lockers: vec![wtm_sys::file_locks::Locker {
                    pid: 100,
                    create_time: 1000,
                    name: "Example spreadsheet".into(),
                    service: String::new(),
                    reason: wtm_sys::file_locks::Hold::OpenFile,
                }],
            })),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(view: &mut View, ctx: &egui::Context) {
        ctx.run_pass(egui::RawInput::default(), |ctx| {
            ctx.root_ui(|ui| {
                view.show(ui);
            });
        })
        .drop_without_applying_deltas();
    }

    fn hold_initial_scan(
        view: &mut View,
        ctx: &egui::Context,
    ) -> std::sync::mpsc::SyncSender<Result<wtm_sys::file_locks::Report, String>> {
        view.dropped(std::path::Path::new(r"C:\Example\first.txt"), ctx);
        assert!(
            view.task.poll().is_some(),
            "replace the synthetic worker result"
        );
        view.task.hold_reply()
    }

    #[test]
    fn a_late_scan_cannot_replace_the_latest_dropped_path() {
        let ctx = egui::Context::default();
        let mut view = View::default();
        let gate = hold_initial_scan(&mut view, &ctx);
        view.dropped(std::path::Path::new(r"C:\Example\second.txt"), &ctx);
        gate.send(Err("first scan error".into())).unwrap();

        frame(&mut view, &ctx);
        assert!(
            view.result.is_none(),
            "the old error belongs to the first path"
        );
        assert!(view.task.busy(), "start the requested replacement");
        frame(&mut view, &ctx);
        assert!(matches!(&view.result, Some(Err(e)) if e == "Synthetic view: use a fixture"));
    }
}
