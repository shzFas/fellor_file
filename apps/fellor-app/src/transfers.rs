//! Transfer queue model (fed by `TransferEvent`s) and the bottom panel.

use crate::app::FellorApp;
use crate::fmt;
use egui::{Layout, RichText};
use fellor_transfer::{TaskId, TaskOutcome, TransferEvent};
use fellor_vfs::{BackendId, VPath};

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Queued,
    Running,
    Paused,
    WaitingForDevice(BackendId),
    Conflict,
    Done,
    Cancelled,
    Failed(String),
}

impl Status {
    pub fn is_finished(&self) -> bool {
        matches!(self, Status::Done | Status::Cancelled | Status::Failed(_))
    }
}

pub struct Task {
    pub id: TaskId,
    pub label: String,
    pub status: Status,
    pub total_files: u64,
    pub total_bytes: u64,
    pub done_files: u64,
    pub done_bytes: u64,
    pub bps: f64,
    pub current: Option<VPath>,
    pub failed: Vec<(VPath, String)>,
    /// Directories to reload when the task finishes.
    pub refresh: Vec<VPath>,
    pub planned: bool,
}

impl Task {
    fn fraction(&self) -> f32 {
        if self.status == Status::Done {
            return 1.0;
        }
        if self.total_bytes > 0 {
            (self.done_bytes as f64 / self.total_bytes as f64) as f32
        } else if self.total_files > 0 {
            self.done_files as f32 / self.total_files as f32
        } else {
            0.0
        }
    }
}

#[derive(Default)]
pub struct Transfers {
    pub tasks: Vec<Task>,
}

impl Transfers {
    pub fn add(&mut self, id: TaskId, label: String, refresh: Vec<VPath>) {
        if let Some(t) = self.get_mut(id) {
            t.refresh = refresh;
            if t.label.is_empty() {
                t.label = label;
            }
            return;
        }
        self.tasks.push(Task {
            id,
            label,
            status: Status::Queued,
            total_files: 0,
            total_bytes: 0,
            done_files: 0,
            done_bytes: 0,
            bps: 0.0,
            current: None,
            failed: Vec::new(),
            refresh,
            planned: false,
        });
    }

    pub fn get(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    fn get_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    fn task(&mut self, id: TaskId) -> &mut Task {
        if self.get(id).is_none() {
            self.add(id, String::new(), Vec::new());
        }
        self.get_mut(id).unwrap()
    }

    /// Apply an engine event. Returns directories to refresh.
    pub fn apply(&mut self, ev: &TransferEvent) -> Vec<VPath> {
        match ev {
            TransferEvent::Queued { task, label } => {
                let t = self.task(*task);
                // Keep our own label (e.g. "Импорт: …") if we already set one.
                if t.label.is_empty() {
                    t.label = label.clone();
                }
            }
            TransferEvent::Planned { task, total_files, total_bytes } => {
                let t = self.task(*task);
                t.total_files = *total_files;
                t.total_bytes = *total_bytes;
                t.planned = true;
                if t.status == Status::Queued {
                    t.status = Status::Running;
                }
            }
            TransferEvent::Progress { task, done_files, done_bytes, total_files, total_bytes, bytes_per_sec, current } => {
                let t = self.task(*task);
                t.done_files = *done_files;
                t.done_bytes = *done_bytes;
                t.total_files = *total_files;
                t.total_bytes = *total_bytes;
                t.bps = *bytes_per_sec;
                t.current = current.clone();
                if matches!(t.status, Status::Queued | Status::WaitingForDevice(_) | Status::Conflict) {
                    t.status = Status::Running;
                }
            }
            TransferEvent::Conflict { task, .. } => self.task(*task).status = Status::Conflict,
            TransferEvent::FileFailed { task, path, error } => self.task(*task).failed.push((path.clone(), error.clone())),
            TransferEvent::Paused { task } => self.task(*task).status = Status::Paused,
            TransferEvent::Resumed { task } => self.task(*task).status = Status::Running,
            TransferEvent::WaitingForDevice { task, backend } => {
                self.task(*task).status = Status::WaitingForDevice(backend.clone())
            }
            TransferEvent::Finished { task, outcome, .. } => {
                let t = self.task(*task);
                t.status = match outcome {
                    TaskOutcome::Done => Status::Done,
                    TaskOutcome::Cancelled => Status::Cancelled,
                    TaskOutcome::Failed(e) => Status::Failed(e.clone()),
                };
                t.bps = 0.0;
                t.current = None;
                return t.refresh.clone();
            }
        }
        Vec::new()
    }

    pub fn any_active(&self) -> bool {
        self.tasks.iter().any(|t| !t.status.is_finished())
    }

    pub fn clear_finished(&mut self) {
        self.tasks.retain(|t| !t.status.is_finished());
    }
}

enum TAct {
    Pause(TaskId),
    Resume(TaskId),
    Cancel(TaskId),
    PauseAll,
    ResumeAll,
    CancelAll,
    ClearFinished,
    Toggle,
}

impl FellorApp {
    pub fn transfers_panel(&mut self, ui: &mut egui::Ui) {
        let mut acts = Vec::new();
        let open = self.settings.transfers_open;
        let panel = egui::Panel::bottom("transfers").resizable(open).min_size(32.0);
        let panel = if open { panel.default_size(170.0).max_size(420.0) } else { panel.exact_size(32.0) };
        panel.show(ui, |ui| {
            ui.add_space(4.0);
            self.transfers_header(ui, &mut acts);
            if open {
                ui.separator();
                self.transfers_list(ui, &mut acts);
            }
        });
        for a in acts {
            match a {
                TAct::Pause(t) => self.engine.pause(t),
                TAct::Resume(t) => self.engine.resume(t),
                TAct::Cancel(t) => self.engine.cancel(t),
                TAct::PauseAll | TAct::ResumeAll | TAct::CancelAll => {
                    for t in &self.transfers.tasks {
                        match (&a, &t.status) {
                            (TAct::PauseAll, Status::Running | Status::Queued) => self.engine.pause(t.id),
                            (TAct::ResumeAll, Status::Paused) => self.engine.resume(t.id),
                            (TAct::CancelAll, s) if !s.is_finished() => self.engine.cancel(t.id),
                            _ => {}
                        }
                    }
                }
                TAct::ClearFinished => self.transfers.clear_finished(),
                TAct::Toggle => self.settings.transfers_open = !self.settings.transfers_open,
            }
        }
    }

    fn transfers_header(&self, ui: &mut egui::Ui, acts: &mut Vec<TAct>) {
        let tasks = &self.transfers.tasks;
        let running = tasks.iter().filter(|t| matches!(t.status, Status::Running | Status::Conflict)).count();
        let queued = tasks.iter().filter(|t| matches!(t.status, Status::Queued | Status::Paused | Status::WaitingForDevice(_))).count();
        let live: Vec<&Task> = tasks.iter().filter(|t| !t.status.is_finished()).collect();
        let done: u64 = live.iter().map(|t| t.done_bytes).sum();
        let total: u64 = live.iter().map(|t| t.total_bytes).sum();
        let bps: f64 = live.iter().filter(|t| t.status == Status::Running).map(|t| t.bps).sum();
        ui.horizontal(|ui| {
            let arrow = if self.settings.transfers_open { "⏷" } else { "⏵" };
            let title = if tasks.is_empty() {
                "Передачи".to_owned()
            } else {
                format!("Передачи ({running} активных, {queued} в очереди)")
            };
            if ui.add(egui::Button::new(RichText::new(format!("{arrow} {title}")).strong()).frame(false)).clicked() {
                acts.push(TAct::Toggle);
            }
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                let any_live = !live.is_empty();
                if ui.add_enabled(any_live, egui::Button::new("×").small()).on_hover_text("Отменить все").clicked() {
                    acts.push(TAct::CancelAll);
                }
                let any_paused = live.iter().any(|t| t.status == Status::Paused);
                if any_paused {
                    if ui.small_button("▶").on_hover_text("Продолжить все").clicked() {
                        acts.push(TAct::ResumeAll);
                    }
                } else if ui.add_enabled(any_live, egui::Button::new("⏸").small()).on_hover_text("Приостановить все").clicked() {
                    acts.push(TAct::PauseAll);
                }
                if tasks.iter().any(|t| t.status.is_finished())
                    && ui.small_button("Очистить").on_hover_text("Убрать завершённые").clicked()
                {
                    acts.push(TAct::ClearFinished);
                }
                if any_live {
                    let mut s = format!("{} / {}", fmt::size(done), fmt::size(total));
                    if bps > 0.0 {
                        s.push_str(&format!("  {}", fmt::speed(bps)));
                        if let Some(eta) = fmt::eta(total.saturating_sub(done), bps) {
                            s.push_str(&format!("  ~{eta}"));
                        }
                    }
                    ui.label(RichText::new(s).monospace().small());
                }
            });
        });
    }

    fn transfers_list(&self, ui: &mut egui::Ui, acts: &mut Vec<TAct>) {
        if self.transfers.tasks.is_empty() {
            ui.label(RichText::new("Очередь пуста. F5 — копировать, F6 — переместить в другую панель, ⌘I — быстрый импорт.").weak().small());
            return;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for t in self.transfers.tasks.iter().rev() {
                ui.horizontal(|ui| {
                    let label_w = (ui.available_width() * 0.38).max(160.0);
                    ui.allocate_ui(egui::vec2(label_w, 18.0), |ui| {
                        ui.add(egui::Label::new(&t.label).truncate()).on_hover_text(
                            t.current.as_ref().map(|c| format!("{}\nСейчас: {}", t.label, c.name())).unwrap_or_else(|| t.label.clone()),
                        );
                    });
                    let (text, color) = status_text(t);
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        if !t.status.is_finished() {
                            if ui.small_button("×").on_hover_text("Отменить").clicked() {
                                acts.push(TAct::Cancel(t.id));
                            }
                            if t.status == Status::Paused {
                                if ui.small_button("▶").on_hover_text("Продолжить").clicked() {
                                    acts.push(TAct::Resume(t.id));
                                }
                            } else if ui.small_button("⏸").on_hover_text("Приостановить").clicked() {
                                acts.push(TAct::Pause(t.id));
                            }
                        }
                        let mut rt = RichText::new(text).small();
                        if let Some(c) = color {
                            rt = rt.color(c);
                        }
                        ui.add_sized(egui::vec2(170.0, 18.0), egui::Label::new(rt).truncate());
                        let bar = egui::ProgressBar::new(t.fraction())
                            .desired_height(12.0)
                            .text(RichText::new(format!("{:.0}%", t.fraction() * 100.0)).small());
                        ui.add(bar);
                    });
                });
                if !t.failed.is_empty() {
                    ui.indent(("failed", t.id), |ui| {
                        egui::CollapsingHeader::new(
                            RichText::new(format!("Ошибки: {}", fmt::files(t.failed.len() as u64))).small().color(ui.visuals().error_fg_color),
                        )
                        .id_salt(("failed-h", t.id))
                        .show(ui, |ui| {
                            for (p, e) in t.failed.iter().take(200) {
                                ui.label(RichText::new(format!("{} — {e}", p.name())).small()).on_hover_text(p.to_string());
                            }
                            if t.failed.len() > 200 {
                                ui.label(RichText::new(format!("…и ещё {}", t.failed.len() - 200)).small().weak());
                            }
                        });
                    });
                }
            }
        });
    }
}

fn status_text(t: &Task) -> (String, Option<egui::Color32>) {
    let warn = egui::Color32::from_rgb(255, 159, 10);
    match &t.status {
        Status::Queued => ("В очереди".into(), None),
        Status::Running if !t.planned => ("Подсчёт…".into(), None),
        Status::Running => {
            let mut s = format!("{}/{}  {}", t.done_files, t.total_files, fmt::speed(t.bps));
            if let Some(eta) = fmt::eta(t.total_bytes.saturating_sub(t.done_bytes), t.bps) {
                s.push_str(&format!("  ~{eta}"));
            }
            (s, None)
        }
        Status::Paused => ("Пауза".into(), Some(warn)),
        Status::Conflict => ("Ждёт решения по конфликту".into(), Some(warn)),
        Status::WaitingForDevice(_) => ("Ожидание устройства".into(), Some(warn)),
        Status::Done if t.failed.is_empty() => (format!("Готово · {}", fmt::size(t.total_bytes)), Some(egui::Color32::from_rgb(52, 199, 89))),
        Status::Done => ("Готово с ошибками".into(), Some(warn)),
        Status::Cancelled => ("Отменено".into(), None),
        Status::Failed(e) => (format!("Ошибка: {e}"), Some(egui::Color32::from_rgb(255, 69, 58))),
    }
}
