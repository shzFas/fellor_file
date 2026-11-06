//! Quick Import (⌘I): plan → confirm dialog → mkdir dest folders → one
//! `TransferRequest` per destination subfolder → mark the index on success.

use crate::app::{FellorApp, ToastKind, UiMsg};
use crate::fmt;
use egui::{Id, Modal, RichText};
use fellor_index::{ImportIndex, ImportPlan};
use fellor_transfer::{ConflictPolicy, TaskId, TransferOp, TransferRequest};
use fellor_vfs::*;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

pub enum Stage {
    Planning,
    Ready(ImportPlan),
    /// Creating destination folders.
    Preparing,
    Error(String),
}

pub struct ImportDialog {
    pub device: BackendId,
    pub device_name: String,
    pub stage: Stage,
    pub dest: String,
    pub template: String,
    pub delete_after: bool,
    /// Second confirmation for "delete from device".
    pub confirm_delete: bool,
}

/// One submitted task of an import, so we can mark the index when it ends.
pub struct ImportBatch {
    pub device: BackendId,
    pub entries: Vec<Entry>,
    pub dest_dir: VPath,
    pub failed: HashSet<VPath>,
}

impl FellorApp {
    /// Pick the device to import from: the active pane's device, otherwise
    /// the first ready phone/camera, otherwise the first ready volume.
    fn import_source(&self) -> Option<DeviceInfo> {
        let active = &self.panes[self.active].path.backend;
        if let Some(d) = self.devices.get(active) {
            return Some(d.clone());
        }
        let ready = self.devices.values().filter(|d| d.state == DeviceState::Ready);
        ready
            .clone()
            .find(|d| d.kind != DeviceKind::Volume)
            .or_else(|| ready.clone().next())
            .or_else(|| self.devices.values().next())
            .cloned()
    }

    pub fn start_import(&mut self) {
        let Some(dev) = self.import_source() else {
            self.toast(ToastKind::Info, "Нет подключённых устройств для импорта");
            return;
        };
        let mut dialog = ImportDialog {
            device: dev.id.clone(),
            device_name: dev.name.clone(),
            stage: Stage::Planning,
            dest: self.settings.import_dest.clone(),
            template: self.settings.import_template.clone(),
            delete_after: false,
            confirm_delete: false,
        };
        if dev.state != DeviceState::Ready {
            let hint = crate::sidebar::state_hint(&dev).unwrap_or_else(|| "устройство не готово".into());
            dialog.stage = Stage::Error(hint);
            self.import = Some(dialog);
            return;
        }
        self.import = Some(dialog);
        self.plan_import(dev.id);
    }

    fn plan_import(&mut self, device: BackendId) {
        let Some(b) = self.backend(&device) else {
            if let Some(d) = &mut self.import {
                d.stage = Stage::Error("Устройство отключено".into());
            }
            return;
        };
        if self.demo {
            let set = self.demo_imported.clone();
            self.spawn(async move {
                let result = crate::demo::plan_import(b, &set).await.map_err(|e| e.to_string());
                UiMsg::ImportPlanned { device, result }
            });
            return;
        }
        // The index (SQLite) need not be `Send`/`Sync`: open it on a helper
        // thread and drive `plan_import` there with the app runtime's handle.
        let (tx, ctx, rt) = (self.tx.clone(), self.ctx.clone(), self.rt.clone());
        std::thread::Builder::new()
            .name("fellor-import-plan".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let idx = ImportIndex::open(&ImportIndex::default_path()).map_err(|e| format!("индекс импорта: {e}"))?;
                    rt.block_on(fellor_index::plan_import(b, &idx)).map_err(|e| e.to_string())
                }))
                .unwrap_or_else(|_| Err("внутренняя ошибка планировщика импорта".into()));
                let _ = tx.send(UiMsg::ImportPlanned { device, result });
                ctx.request_repaint();
            })
            .ok();
    }

    pub fn on_import_planned(&mut self, device: BackendId, result: Result<ImportPlan, String>) {
        let Some(d) = &mut self.import else { return };
        if d.device != device {
            return;
        }
        d.stage = match result {
            Ok(p) => Stage::Ready(p),
            Err(e) => Stage::Error(e),
        };
    }

    fn subdir_for(&self, template: &str, e: &Entry) -> String {
        if self.demo {
            crate::demo::dest_subdir(template, e)
        } else {
            fellor_index::dest_subdir(template, e)
        }
    }

    /// Group by destination dir, create the dirs, then hand over to the UI
    /// thread which submits the transfers.
    pub(crate) fn submit_import(&mut self) {
        let Some(d) = &mut self.import else { return };
        let Stage::Ready(plan) = std::mem::replace(&mut d.stage, Stage::Preparing) else { return };
        let device = d.device.clone();
        let base = fellor_local::vpath_from_host(&PathBuf::from(d.dest.trim()));
        let template = d.template.trim().to_owned();
        let op = if d.delete_after { TransferOp::Move } else { TransferOp::Copy };
        self.settings.import_dest = d.dest.trim().to_owned();
        self.settings.import_template = template.clone();

        let mut groups: BTreeMap<VPath, Vec<Entry>> = BTreeMap::new();
        for e in plan.new_files {
            let mut dir = base.clone();
            for c in self.subdir_for(&template, &e).split('/').filter(|c| !c.is_empty() && *c != "." && *c != "..") {
                dir = dir.join(c);
            }
            groups.entry(dir).or_default().push(e);
        }
        let Some(local) = self.backend("local") else {
            if let Some(d) = &mut self.import {
                d.stage = Stage::Error("Локальный диск ещё не готов".into());
            }
            return;
        };
        self.spawn(async move {
            let mut errors = Vec::new();
            let mut ok_groups = Vec::new();
            for (dir, entries) in groups {
                match mkdir_p(&local, &dir).await {
                    Ok(()) => ok_groups.push((dir, entries)),
                    Err(e) => errors.push(format!("{}: {e}", dir.as_unix())),
                }
            }
            UiMsg::ImportReady { device, groups: ok_groups, op, errors }
        });
    }

    pub fn on_import_ready(&mut self, device: BackendId, groups: Vec<(VPath, Vec<Entry>)>, op: TransferOp, errors: Vec<String>) {
        for e in &errors {
            self.toast(ToastKind::Error, format!("Не удалось создать папку {e}"));
        }
        let mut files = 0;
        let mut first_dir = None;
        for (dir, entries) in groups {
            files += entries.len();
            let req = TransferRequest {
                sources: entries.iter().map(|e| e.path.clone()).collect(),
                dest_dir: dir.clone(),
                op,
                policy: ConflictPolicy::SkipIfSameSize,
            };
            let id: TaskId = self.engine.submit(req);
            let label = format!("Импорт: {} → {}", fmt::files(entries.len() as u64), dir.name());
            let mut refresh = vec![dir.clone()];
            refresh.extend(dir.parent());
            self.transfers.add(id, label, refresh);
            first_dir.get_or_insert(dir.clone());
            self.import_tasks.insert(id, ImportBatch { device: device.clone(), entries, dest_dir: dir, failed: HashSet::new() });
        }
        self.import = None;
        self.settings.transfers_open = true;
        if files > 0 {
            self.toast(ToastKind::Info, format!("Импорт: {} поставлено в очередь", fmt::files(files as u64)));
        }
    }

    /// Record successfully imported files in the index.
    pub fn mark_imported(&mut self, batch: ImportBatch) {
        let ImportBatch { device, entries, dest_dir, failed } = batch;
        let done: Vec<Entry> = entries.into_iter().filter(|e| !failed.contains(&e.path)).collect();
        if done.is_empty() {
            return;
        }
        if self.demo {
            let mut set = self.demo_imported.lock().unwrap();
            for e in done {
                set.insert((device.clone(), e.path.as_unix()));
            }
            return;
        }
        let dest_host = crate::platform::host_path(&dest_dir).unwrap_or_default();
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn_blocking(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
                let idx = ImportIndex::open(&ImportIndex::default_path()).map_err(|e| e.to_string())?;
                for e in &done {
                    let dest = dest_host.join(&e.name).to_string_lossy().into_owned();
                    idx.mark_imported(&device, &e.path.as_unix(), e.size.unwrap_or(0), e.mtime, &dest)
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            }))
            .unwrap_or_else(|_| Err("внутренняя ошибка".into()));
            if let Err(e) = r {
                let _ = tx.send(UiMsg::Toast(ToastKind::Error, format!("Не удалось обновить индекс импорта: {e}")));
                ctx.request_repaint();
            }
        });
    }

    pub fn import_dialog(&mut self, ctx: &egui::Context) {
        if self.import.is_none() {
            return;
        }
        let mut close = false;
        let mut submit = false;
        let mut retry = false;
        let resp = Modal::new(Id::unique("import")).show(ctx, |ui| {
            let d = self.import.as_mut().unwrap();
            ui.set_width(480.0);
            ui.heading(format!("Быстрый импорт — {}", d.device_name));
            ui.add_space(8.0);
            match &d.stage {
                Stage::Planning => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Ищем новые фото и видео…");
                    });
                }
                Stage::Preparing => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Создаём папки…");
                    });
                }
                Stage::Error(e) => {
                    ui.label(RichText::new(format!("⚠ {e}")).color(ui.visuals().error_fg_color));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Повторить").clicked() {
                            retry = true;
                        }
                        if ui.button("Закрыть").clicked() {
                            close = true;
                        }
                    });
                    return;
                }
                Stage::Ready(plan) => {
                    let n = plan.new_files.len() as u64;
                    if n == 0 {
                        ui.label(RichText::new("Новых файлов нет — всё уже импортировано.").strong());
                    } else {
                        ui.label(RichText::new(format!("Новых: {} · {}", fmt::files(n), fmt::size(plan.total_bytes))).strong());
                    }
                    if plan.already_imported > 0 {
                        ui.label(RichText::new(format!("Уже импортировано ранее: {}", plan.already_imported)).small().weak());
                    }
                    if let (Some(a), Some(b)) = (plan.new_files.first(), plan.new_files.last()) {
                        ui.label(RichText::new(format!("С {} по {}", fmt::date(a.mtime), fmt::date(b.mtime))).small().weak());
                    }
                }
            }
            if !matches!(d.stage, Stage::Ready(_)) {
                ui.add_space(8.0);
                if ui.button("Отмена").clicked() {
                    close = true;
                }
                return;
            }
            ui.add_space(10.0);
            egui::Grid::new("import-grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                ui.label("Куда");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut d.dest).desired_width(270.0));
                    if ui.button("Выбрать…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().set_directory(&d.dest).pick_folder() {
                            d.dest = p.to_string_lossy().into_owned();
                        }
                    }
                });
                ui.end_row();
                ui.label("Подпапка");
                ui.vertical(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut d.template).desired_width(270.0));
                    ui.label(RichText::new("{date} → ГГГГ-ММ-ДД по дате съёмки").small().weak());
                });
                ui.end_row();
            });
            ui.add_space(6.0);
            if ui.checkbox(&mut d.delete_after, "Удалить с устройства после импорта").changed() {
                d.confirm_delete = false;
            }
            if d.delete_after {
                ui.label(
                    RichText::new("⚠ Оригиналы будут удалены с устройства сразу после успешного копирования. Корзины на устройстве нет.")
                        .small()
                        .color(ui.visuals().warn_fg_color),
                );
            }
            ui.add_space(10.0);
            let n = matches!(&d.stage, Stage::Ready(p) if !p.new_files.is_empty());
            let dest_ok = !d.dest.trim().is_empty() && d.dest.trim().starts_with('/');
            ui.horizontal(|ui| {
                if d.delete_after && d.confirm_delete {
                    let b = egui::Button::new(RichText::new("Да, импортировать и удалить").color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(200, 50, 45));
                    if ui.add(b).clicked() {
                        submit = true;
                    }
                } else if ui.add_enabled(n && dest_ok, egui::Button::new("Импортировать")).clicked() {
                    if d.delete_after {
                        d.confirm_delete = true;
                    } else {
                        submit = true;
                    }
                }
                if ui.button("Отмена").clicked() {
                    close = true;
                }
            });
            if !dest_ok {
                ui.label(RichText::new("Укажите полный путь папки назначения").small().color(ui.visuals().warn_fg_color));
            }
        });
        if retry {
            if let Some(d) = &mut self.import {
                d.stage = Stage::Planning;
                let id = d.device.clone();
                self.plan_import(id);
            }
        }
        if submit {
            self.submit_import();
        } else if close || (resp.should_close() && !matches!(self.import.as_ref().map(|d| &d.stage), Some(Stage::Preparing))) {
            self.import = None;
        }
    }
}

/// `mkdir -p` over the `Backend` API.
async fn mkdir_p(b: &BackendRef, dir: &VPath) -> Result<()> {
    for n in 1..=dir.components.len() {
        let p = VPath::new(dir.backend.clone(), dir.components[..n].to_vec());
        match b.stat(&p).await {
            Ok(e) if e.is_dir() => continue,
            Ok(_) => return Err(VfsError::NotDir(p.as_unix())),
            Err(_) => match b.mkdir(&p).await {
                Ok(_) | Err(VfsError::AlreadyExists(_)) => {}
                Err(e) => return Err(e),
            },
        }
    }
    Ok(())
}
