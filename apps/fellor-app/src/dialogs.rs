//! Modal dialogs: new folder, rename, delete confirmation, conflicts,
//! quick-look preview, properties, settings, help.

use crate::app::{FellorApp, ToastKind};
use crate::fmt;
use crate::platform;
use crate::settings::PolicyChoice;
use egui::{Id, Modal, RichText};
use fellor_transfer::{ConflictId, ConflictResolution, TaskId};
use fellor_vfs::{BackendId, Entry};

pub enum Dialog {
    NewFolder { pane: usize, name: String, focus: bool },
    Rename { pane: usize, entry: Entry, name: String, focus: bool },
    Delete { pane: usize, items: Vec<Entry> },
    KillBusy { device: BackendId, names: Vec<String> },
    Properties { entry: Entry },
}

pub struct ConflictPrompt {
    pub task: TaskId,
    pub id: ConflictId,
    pub src: Entry,
    pub dst: Entry,
    pub apply_all: bool,
}

pub struct PreviewState {
    pub entry: Entry,
    pub loading: bool,
    pub texture: Option<egui::TextureHandle>,
    pub error: Option<String>,
}

impl PreviewState {
    pub fn new(entry: Entry) -> Self {
        Self { entry, loading: true, texture: None, error: None }
    }

    pub fn set_result(&mut self, ctx: &egui::Context, r: Result<Option<egui::ColorImage>, String>) {
        self.loading = false;
        match r {
            Ok(Some(img)) => {
                self.texture = Some(ctx.load_texture(format!("preview:{}", self.entry.path), img, egui::TextureOptions::LINEAR))
            }
            Ok(None) => {}
            Err(e) => self.error = Some(e),
        }
    }
}

enum Outcome {
    None,
    Close,
    Mkdir(usize, String),
    Rename(Entry, String),
    Delete(Vec<Entry>),
    Kill(BackendId, Vec<String>),
}

fn name_error(name: &str, existing: &[Entry], keep: Option<&str>) -> Option<&'static str> {
    let n = name.trim();
    if n.is_empty() {
        return Some("Введите имя");
    }
    if n.contains('/') || n.contains(':') {
        return Some("Имя не может содержать «/» или «:»");
    }
    if n == "." || n == ".." {
        return Some("Недопустимое имя");
    }
    if keep != Some(n) && existing.iter().any(|e| e.name == n) {
        return Some("Объект с таким именем уже есть");
    }
    None
}

fn entry_card(ui: &mut egui::Ui, title: &str, e: &Entry) {
    ui.vertical(|ui| {
        ui.label(RichText::new(title).small().weak());
        ui.label(RichText::new(&e.name).strong());
        ui.label(format!("Размер: {}", e.size.map(fmt::size).unwrap_or_else(|| "—".into())));
        ui.label(format!("Изменён: {}", fmt::date(e.mtime)));
    });
}

impl FellorApp {
    pub fn dialogs(&mut self, ctx: &egui::Context) {
        self.main_dialog(ctx);
        self.conflict_dialog(ctx);
        self.preview_window(ctx);
        self.import_dialog(ctx);
        self.settings_dialog(ctx);
        self.help_dialog(ctx);
    }

    fn main_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.dialog else { return };
        let mut out = Outcome::None;
        let panes = &self.panes;
        let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));
        let resp = Modal::new(Id::unique("dialog")).show(ctx, |ui| {
            ui.set_width(380.0);
            match dialog {
                Dialog::NewFolder { pane, name, focus } => {
                    ui.heading("Новая папка");
                    ui.label(RichText::new(format!("в {}", panes[*pane].path.as_unix())).small().weak());
                    ui.add_space(6.0);
                    let r = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                    if std::mem::take(focus) {
                        r.request_focus();
                    }
                    let err = name_error(name, &panes[*pane].entries, None);
                    if let Some(e) = err {
                        ui.label(RichText::new(e).small().color(ui.visuals().warn_fg_color));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(err.is_none(), egui::Button::new("Создать")).clicked() || (enter && err.is_none()) {
                            out = Outcome::Mkdir(*pane, name.trim().to_owned());
                        }
                        if ui.button("Отмена").clicked() {
                            out = Outcome::Close;
                        }
                    });
                }
                Dialog::Rename { pane, entry, name, focus } => {
                    ui.heading("Переименовать");
                    ui.add_space(6.0);
                    let r = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                    if std::mem::take(focus) {
                        r.request_focus();
                        // Select the stem, like Finder.
                        let stem = entry.name.rsplit_once('.').filter(|(s, _)| !s.is_empty()).map(|(s, _)| s).unwrap_or(&entry.name);
                        let chars = stem.chars().count();
                        if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), r.id) {
                            st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(chars),
                            )));
                            st.store(ui.ctx(), r.id);
                        }
                    }
                    let err = name_error(name, &panes[*pane].entries, Some(&entry.name));
                    if let Some(e) = err {
                        ui.label(RichText::new(e).small().color(ui.visuals().warn_fg_color));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let ok = err.is_none() && name.trim() != entry.name;
                        if ui.add_enabled(ok, egui::Button::new("Переименовать")).clicked() || (enter && ok) {
                            out = Outcome::Rename(entry.clone(), name.trim().to_owned());
                        }
                        if ui.button("Отмена").clicked() || (enter && name.trim() == entry.name) {
                            out = Outcome::Close;
                        }
                    });
                }
                Dialog::Delete { pane, items } => {
                    let on_device = crate::platform::host_path(&panes[*pane].path).is_none();
                    ui.heading("Удалить?");
                    ui.add_space(4.0);
                    let names: Vec<&str> = items.iter().take(5).map(|e| e.name.as_str()).collect();
                    let mut s = names.join(", ");
                    if items.len() > 5 {
                        s.push_str(&format!(" и ещё {}", items.len() - 5));
                    }
                    ui.label(format!("{} {}: {s}", items.len(), fmt::plural(items.len() as u64, "объект", "объекта", "объектов")));
                    ui.add_space(4.0);
                    let warn = if on_device {
                        "⚠ На устройстве нет корзины — файлы будут удалены безвозвратно."
                    } else {
                        "⚠ Файлы будут удалены безвозвратно, минуя Корзину."
                    };
                    ui.label(RichText::new(warn).color(ui.visuals().warn_fg_color));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let del = egui::Button::new(RichText::new("Удалить").color(egui::Color32::WHITE)).fill(egui::Color32::from_rgb(200, 50, 45));
                        if ui.add(del).clicked() {
                            out = Outcome::Delete(items.clone());
                        }
                        if ui.button("Отмена").clicked() {
                            out = Outcome::Close;
                        }
                    });
                }
                Dialog::KillBusy { device, names } => {
                    ui.heading("Закрыть программы?");
                    ui.label(format!("Будут принудительно завершены: {}.", names.join(", ")));
                    ui.label(RichText::new("Они держат USB-интерфейс телефона, и Fellor не может к нему подключиться.").small().weak());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Закрыть").clicked() {
                            out = Outcome::Kill(device.clone(), names.clone());
                        }
                        if ui.button("Отмена").clicked() {
                            out = Outcome::Close;
                        }
                    });
                }
                Dialog::Properties { entry } => {
                    ui.heading(&entry.name);
                    ui.add_space(4.0);
                    egui::Grid::new("props").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                        let kind = if entry.is_dir() { "Папка" } else { "Файл" };
                        for (k, v) in [
                            ("Тип", entry.mime.map(|m| format!("{kind} ({m})")).unwrap_or_else(|| kind.to_owned())),
                            ("Путь", entry.path.to_string()),
                            ("Размер", entry.size.map(|s| format!("{} ({s} байт)", fmt::size(s))).unwrap_or_else(|| "—".into())),
                            ("Изменён", fmt::date(entry.mtime)),
                            ("Создан", fmt::date(entry.ctime)),
                        ] {
                            ui.label(RichText::new(k).weak());
                            ui.add(egui::Label::new(v).wrap());
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                    if ui.button("Закрыть").clicked() || enter {
                        out = Outcome::Close;
                    }
                }
            }
        });
        if resp.should_close() && matches!(out, Outcome::None) {
            out = Outcome::Close;
        }
        match out {
            Outcome::None => {}
            Outcome::Close => self.dialog = None,
            Outcome::Mkdir(p, n) => {
                self.dialog = None;
                self.do_mkdir(p, n);
            }
            Outcome::Rename(e, n) => {
                self.dialog = None;
                self.do_rename(e, n);
            }
            Outcome::Delete(items) => {
                self.dialog = None;
                self.do_delete(items);
            }
            Outcome::Kill(device, names) => {
                self.dialog = None;
                platform::kill_by_name(&names);
                self.toast(ToastKind::Info, "Программы закрыты, проверяем устройство…");
                self.recheck(device);
            }
        }
    }

    fn conflict_dialog(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() {
            return;
        }
        let Some(c) = self.conflicts.front_mut() else { return };
        let mut choice = None;
        let task_label = self.transfers.get(c.task).map(|t| t.label.clone()).unwrap_or_default();
        Modal::new(Id::unique("conflict")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("Файл уже существует");
            ui.label(RichText::new(task_label).small().weak());
            ui.add_space(8.0);
            ui.columns(2, |cols| {
                entry_card(&mut cols[0], "Копируемый", &c.src);
                entry_card(&mut cols[1], "Уже есть в папке назначения", &c.dst);
            });
            if let (Some(a), Some(b)) = (c.src.mtime, c.dst.mtime) {
                let newer = if a > b { "Копируемый файл новее." } else if a < b { "Файл в папке назначения новее." } else { "Даты совпадают." };
                ui.label(RichText::new(newer).small());
            }
            ui.add_space(8.0);
            ui.checkbox(&mut c.apply_all, "Применить ко всем");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Пропустить").clicked() {
                    choice = Some(ConflictResolution::Skip);
                }
                if ui.button("Заменить").clicked() {
                    choice = Some(ConflictResolution::Overwrite);
                }
                if ui.button("Переименовать").on_hover_text("Сохранить как «имя (2).ext»").clicked() {
                    choice = Some(ConflictResolution::Rename);
                }
                if ui.button("Оставить более новый").clicked() {
                    choice = Some(ConflictResolution::KeepNewer);
                }
            });
        });
        if let Some(res) = choice {
            let c = self.conflicts.pop_front().unwrap();
            self.engine.resolve_conflict(c.task, c.id, res, c.apply_all);
            if c.apply_all {
                // The engine applies it to the rest of this task; drop queued
                // prompts of the same task after answering them the same way.
                let (same, rest): (Vec<_>, Vec<_>) = self.conflicts.drain(..).partition(|o| o.task == c.task);
                for o in same {
                    self.engine.resolve_conflict(o.task, o.id, res, true);
                }
                self.conflicts.extend(rest);
            }
        }
    }

    fn preview_window(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.preview else { return };
        let mut open = true;
        let title = p.entry.name.clone();
        egui::Window::new(title)
            .id(Id::unique("preview"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .min_width(360.0)
            .show(ctx, |ui| {
                if let Some(tex) = &p.texture {
                    // Fit into 75% of the window, never upscale past 1:1.
                    let screen = ctx.content_rect().size() * 0.75;
                    let size = tex.size_vec2();
                    let k = (screen.x / size.x).min(screen.y / size.y).min(1.0);
                    ui.vertical_centered(|ui| {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(size * k));
                    });
                } else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        if p.loading {
                            ui.spinner();
                            ui.label("Загрузка превью…");
                        } else {
                            ui.label(RichText::new(crate::panes::entry_icon(&p.entry)).size(64.0));
                            match &p.error {
                                Some(e) => ui.label(RichText::new(format!("Превью недоступно: {e}")).weak()),
                                None => ui.label(RichText::new("Превью недоступно").weak()),
                            };
                        }
                        ui.add_space(40.0);
                    });
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!(
                        "{} · {}{}",
                        p.entry.size.map(fmt::size).unwrap_or_else(|| "—".into()),
                        fmt::date(p.entry.mtime),
                        p.entry.mime.map(|m| format!(" · {m}")).unwrap_or_default()
                    ))
                    .small()
                    .weak());
                });
            });
        if !open {
            self.preview = None;
        }
    }

    fn settings_dialog(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut close = false;
        let mut hidden_changed = false;
        let resp = Modal::new(Id::unique("settings")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading("Настройки");
            ui.add_space(8.0);
            egui::Grid::new("settings-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("При конфликте имён");
                egui::ComboBox::from_id_salt("policy")
                    .selected_text(self.settings.conflict_policy.label())
                    .width(240.0)
                    .show_ui(ui, |ui| {
                        for p in PolicyChoice::ALL {
                            ui.selectable_value(&mut self.settings.conflict_policy, p, p.label());
                        }
                    });
                ui.end_row();

                ui.label("Скрытые файлы");
                hidden_changed = ui.checkbox(&mut self.settings.show_hidden, "показывать (⌘⇧.)").changed();
                ui.end_row();

                ui.label("Папка импорта");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.settings.import_dest).desired_width(200.0));
                    if ui.button("Выбрать…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().set_directory(&self.settings.import_dest).pick_folder() {
                            self.settings.import_dest = p.to_string_lossy().into_owned();
                        }
                    }
                });
                ui.end_row();

                ui.label("Шаблон подпапки");
                ui.vertical(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.settings.import_template).desired_width(200.0));
                    ui.label(RichText::new("{date} → 2026-11-07; также {year}, {month}, {day}").small().weak());
                });
                ui.end_row();
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Готово").clicked() {
                    close = true;
                }
                if ui.button("Сбросить").clicked() {
                    let keep = (self.settings.last_paths.clone(), self.settings.transfers_open);
                    self.settings = Default::default();
                    (self.settings.last_paths, self.settings.transfers_open) = keep;
                    hidden_changed = true;
                }
            });
        });
        if hidden_changed {
            let sh = self.settings.show_hidden;
            self.panes.iter_mut().for_each(|p| p.rebuild_view(sh));
        }
        if close || resp.should_close() {
            self.settings_open = false;
        }
    }

    fn help_dialog(&mut self, ctx: &egui::Context) {
        if !self.help_open {
            return;
        }
        let resp = Modal::new(Id::unique("help")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading("Горячие клавиши");
            ui.add_space(6.0);
            egui::Grid::new("keys").num_columns(2).spacing([16.0, 3.0]).striped(true).show(ui, |ui| {
                for (k, v) in [
                    ("Tab", "Переключить панель"),
                    ("F5 / ⌘C → ⌘V", "Копировать в другую панель"),
                    ("F6", "Переместить"),
                    ("F7 / ⌘⇧N", "Новая папка"),
                    ("F8 / ⌘⌫", "Удалить"),
                    ("F2 / ⇧Enter", "Переименовать"),
                    ("Enter / ⌘↓", "Открыть"),
                    ("⌘↑ / ⌫", "Вверх"),
                    ("⌘[ / ⌘]", "Назад / вперёд"),
                    ("Пробел", "Быстрый просмотр"),
                    ("⌘F", "Фильтр"),
                    ("⌘R", "Обновить"),
                    ("⌘A", "Выбрать всё"),
                    ("⌘I", "Быстрый импорт"),
                    ("⌘E", "Извлечь том"),
                    ("⌘⇧.", "Скрытые файлы"),
                    ("⌘,", "Настройки"),
                ] {
                    ui.label(RichText::new(k).monospace());
                    ui.label(v);
                    ui.end_row();
                }
            });
            ui.add_space(8.0);
            if ui.button("Закрыть").clicked() {
                self.help_open = false;
            }
        });
        if resp.should_close() {
            self.help_open = false;
        }
    }
}
