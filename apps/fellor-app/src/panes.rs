//! The two file panes: navigation bar, breadcrumbs, sortable table, footer.

use crate::app::{home_vpath, FellorApp};
use crate::dialogs::Dialog;
use crate::fmt;
use crate::pane::SortKey;
use egui::{Align, Layout, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use fellor_transfer::TransferOp;
use fellor_vfs::{Entry, VPath};

/// Things the user did while the pane was drawn; applied afterwards so the
/// drawing code only needs `&Pane`.
enum Act {
    Activate,
    Back,
    Forward,
    Up,
    Refresh,
    Goto(VPath),
    Click { idx: usize, cmd: bool, shift: bool },
    EnsureSelected(usize),
    Open(Entry),
    Sort(SortKey),
    FilterChanged,
    Transfer(TransferOp),
    Rename,
    NewFolder,
    Delete,
    Reveal(VPath),
    Properties(Entry),
    Preview(Entry),
    Hover(Entry),
    ClearSelection,
}

pub fn entry_icon(e: &Entry) -> &'static str {
    if e.is_dir() {
        return "📁";
    }
    match e.mime {
        Some(m) if m.starts_with("image/") => "🖼",
        Some(m) if m.starts_with("video/") => "🎞",
        Some(m) if m.starts_with("audio/") => "🎵",
        Some("application/zip") => "🗜",
        _ => "📄",
    }
}

impl FellorApp {
    pub fn panes_ui(&mut self, ui: &mut egui::Ui) {
        ui.columns(2, |cols| {
            for (i, col) in cols.iter_mut().enumerate() {
                self.pane_ui(col, i);
            }
        });
    }

    fn pane_ui(&mut self, ui: &mut egui::Ui, i: usize) {
        let mut acts = Vec::new();
        let active = self.active == i;
        let stroke = if active {
            egui::Stroke::new(1.5, ui.visuals().selection.stroke.color)
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke
        };
        let frame = egui::Frame::group(ui.style()).stroke(stroke).inner_margin(6.0);
        let resp = frame
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                self.nav_bar(ui, i, &mut acts);
                ui.add_space(2.0);
                let footer_h = 22.0;
                let table_h = (ui.available_height() - footer_h).max(60.0);
                ui.allocate_ui(egui::vec2(ui.available_width(), table_h), |ui| {
                    ui.set_min_height(table_h);
                    self.table(ui, i, &mut acts);
                });
                ui.separator();
                self.footer(ui, i);
            })
            .response;
        if resp.contains_pointer() && ui.input(|inp| inp.pointer.any_pressed()) {
            acts.insert(0, Act::Activate);
        }
        for a in acts {
            self.apply(i, a);
        }
    }

    fn apply(&mut self, i: usize, a: Act) {
        let show_hidden = self.settings.show_hidden;
        match a {
            Act::Activate => {
                if self.active != i {
                    self.active = i;
                    self.preview = None;
                }
            }
            Act::Back => self.go_back(i),
            Act::Forward => self.go_forward(i),
            Act::Up => self.go_up(i),
            Act::Refresh => self.load_pane(i),
            Act::Goto(p) => self.navigate(i, p),
            Act::Click { idx, cmd, shift } => self.panes[i].click(idx, cmd, shift),
            Act::EnsureSelected(idx) => self.panes[i].ensure_selected(idx),
            Act::Open(e) => self.open_entry(i, e),
            Act::Sort(k) => self.panes[i].set_sort(k, show_hidden),
            Act::FilterChanged => self.panes[i].rebuild_view(show_hidden),
            Act::Transfer(op) => {
                self.active = i;
                self.transfer_to_other(op);
            }
            Act::Rename => {
                self.active = i;
                self.ask_rename();
            }
            Act::NewFolder => {
                self.active = i;
                self.ask_new_folder();
            }
            Act::Delete => {
                self.active = i;
                self.ask_delete();
            }
            Act::Reveal(p) => self.reveal_in_finder(&p),
            Act::Properties(e) => self.dialog = Some(Dialog::Properties { entry: e }),
            Act::Preview(e) => self.open_preview(e),
            Act::Hover(e) => {
                self.hover_seen = true;
                if self.hover.as_ref().map(|h| &h.entry.path) != Some(&e.path) {
                    self.hover = Some(crate::app::HoverState { entry: e, since: std::time::Instant::now() });
                }
            }
            Act::ClearSelection => self.panes[i].selected.clear(),
        }
    }

    fn breadcrumbs(&self, path: &VPath) -> Vec<(String, VPath)> {
        let mut crumbs = Vec::new();
        let home = home_vpath();
        let mut start = 0;
        if path.backend == "local" && path.starts_with(&home) && !home.is_root() {
            crumbs.push(("🏠 ~".to_owned(), home.clone()));
            start = home.components.len();
        } else {
            crumbs.push((self.backend_label(&path.backend), VPath::root(path.backend.clone())));
        }
        for n in start..path.components.len() {
            crumbs.push((path.components[n].clone(), VPath::new(path.backend.clone(), path.components[..=n].to_vec())));
        }
        crumbs
    }

    fn nav_bar(&mut self, ui: &mut egui::Ui, i: usize, acts: &mut Vec<Act>) {
        let pane = &self.panes[i];
        let crumbs = self.breadcrumbs(&pane.path);
        let focus_filter = self.focus_filter == Some(i);
        let pane = &mut self.panes[i];
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if ui.add_enabled(pane.can_back(), egui::Button::new("◀")).on_hover_text("Назад (⌘[)").clicked() {
                acts.push(Act::Back);
            }
            if ui.add_enabled(pane.can_forward(), egui::Button::new("▶")).on_hover_text("Вперёд (⌘])").clicked() {
                acts.push(Act::Forward);
            }
            if ui.add_enabled(pane.can_up(), egui::Button::new("⬆")).on_hover_text("Вверх (⌘↑)").clicked() {
                acts.push(Act::Up);
            }
            ui.add_space(6.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("⟳").on_hover_text("Обновить (⌘R)").clicked() {
                    acts.push(Act::Refresh);
                }
                let te = egui::TextEdit::singleline(&mut pane.filter).hint_text("🔍 Фильтр (⌘F)").desired_width(130.0);
                let r = ui.add(te);
                if focus_filter {
                    r.request_focus();
                }
                if r.changed() {
                    acts.push(Act::FilterChanged);
                }
                if r.has_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Escape)) {
                    pane.filter.clear();
                    acts.push(Act::FilterChanged);
                    r.surrender_focus();
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    egui::ScrollArea::horizontal().id_salt(("crumbs", i)).stick_to_right(true).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let last = crumbs.len() - 1;
                            for (n, (label, path)) in crumbs.into_iter().enumerate() {
                                if n > 0 {
                                    ui.label(RichText::new("›").weak());
                                }
                                let text = if n == last { RichText::new(label).strong() } else { RichText::new(label) };
                                if ui.add(egui::Button::new(text).frame(false)).clicked() {
                                    acts.push(Act::Goto(path));
                                }
                            }
                        });
                    });
                });
            });
        });
        if focus_filter {
            self.focus_filter = None;
        }
    }

    fn table(&mut self, ui: &mut egui::Ui, i: usize, acts: &mut Vec<Act>) {
        let pane = &self.panes[i];
        if pane.view.is_empty() {
            ui.centered_and_justified(|ui| {
                if pane.waiting {
                    ui.label(RichText::new("Ожидание устройства…").weak());
                } else if pane.loading {
                    ui.spinner();
                } else if let Some(e) = &pane.error {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.label(RichText::new(format!("⚠ {e}")).color(ui.visuals().error_fg_color));
                        if ui.button("Повторить").clicked() {
                            acts.push(Act::Refresh);
                        }
                    });
                } else if !pane.filter.is_empty() {
                    ui.label(RichText::new("Ничего не найдено").weak());
                } else {
                    let r = ui.label(RichText::new("Папка пуста").weak());
                    r.context_menu(|ui| {
                        if ui.button("Новая папка").clicked() {
                            acts.push(Act::NewFolder);
                        }
                    });
                }
            });
            return;
        }

        let is_local = fellor_vfs_is_host(&pane.path);
        let row_h = 20.0;
        let arrow = |k: SortKey| {
            if pane.sort == k {
                if pane.ascending {
                    " ▲"
                } else {
                    " ▼"
                }
            } else {
                ""
            }
        };
        let mut table = TableBuilder::new(ui)
            .id_salt(("table", i))
            .striped(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().clip(true).at_least(120.0))
            .column(Column::exact(80.0))
            .column(Column::exact(118.0))
            .auto_shrink([false, false]);
        if pane.scroll_to_cursor {
            if let Some(c) = pane.cursor {
                table = table.scroll_to_row(c, None);
            }
        }
        let loading = pane.loading;
        table
            .header(20.0, |mut h| {
                for (k, title) in [(SortKey::Name, "Имя"), (SortKey::Size, "Размер"), (SortKey::Date, "Изменён")] {
                    h.col(|ui| {
                        let text = RichText::new(format!("{title}{}", arrow(k))).strong();
                        if ui.add(egui::Button::new(text).frame(false)).clicked() {
                            acts.push(Act::Sort(k));
                        }
                        if k == SortKey::Name && loading {
                            ui.spinner();
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, pane.view.len(), |mut row| {
                    let idx = row.index();
                    let Some(e) = pane.entry_at(idx) else { return };
                    row.set_selected(pane.is_selected(idx));
                    row.col(|ui| {
                        ui.label(entry_icon(e));
                        let mut t = RichText::new(&e.name);
                        if e.hidden || e.name.starts_with('.') {
                            t = t.weak();
                        }
                        ui.add(egui::Label::new(t).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let s = if e.is_dir() { "—".to_owned() } else { e.size.map(fmt::size).unwrap_or_else(|| "—".into()) };
                            ui.add(egui::Label::new(RichText::new(s).monospace().small()).selectable(false));
                        });
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(fmt::date(e.mtime)).small()).selectable(false));
                    });
                    let r = row.response();
                    if r.hovered() && !e.is_dir() {
                        acts.push(Act::Hover(e.clone()));
                    }
                    if r.double_clicked() {
                        acts.push(Act::Open(e.clone()));
                    } else if r.clicked() {
                        let m = ui_modifiers(&r);
                        acts.push(Act::Click { idx, cmd: m.command, shift: m.shift });
                    } else if r.secondary_clicked() {
                        acts.push(Act::EnsureSelected(idx));
                    }
                    r.context_menu(|ui| {
                        row_menu(ui, e, is_local, acts);
                    });
                });
            });
        self.panes[i].scroll_to_cursor = false;

        // Click on the empty area below the rows clears the selection; right
        // click offers "new folder".
        let rest = ui.available_rect_before_wrap();
        if rest.height() > 4.0 {
            let r = ui.interact(rest, ui.scope_id().with(("empty", i)), Sense::click());
            if r.clicked() {
                acts.push(Act::ClearSelection);
            }
            r.context_menu(|ui| {
                if ui.button("Новая папка").clicked() {
                    acts.push(Act::NewFolder);
                }
                if ui.button("Обновить").clicked() {
                    acts.push(Act::Refresh);
                }
            });
        }
    }

    fn footer(&self, ui: &mut egui::Ui, i: usize) {
        let pane = &self.panes[i];
        let (files, dirs, bytes) = pane.totals();
        ui.horizontal(|ui| {
            let mut s = Vec::new();
            if dirs > 0 {
                s.push(fmt::folders(dirs));
            }
            s.push(fmt::files(files));
            s.push(fmt::size(bytes));
            let sel = pane.selected.len() as u64;
            if sel > 0 {
                s.push(format!("выбрано {sel} ({})", fmt::size(pane.selection_bytes())));
            }
            ui.label(RichText::new(s.join(" · ")).small());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(sp) = pane.space {
                    ui.label(RichText::new(format!("{} свободно из {}", fmt::size(sp.free), fmt::size(sp.total))).small().weak());
                }
            });
        });
    }
}

fn ui_modifiers(r: &egui::Response) -> egui::Modifiers {
    r.ctx.input(|i| i.modifiers)
}

fn fellor_vfs_is_host(p: &VPath) -> bool {
    crate::platform::host_path(p).is_some()
}

fn row_menu(ui: &mut egui::Ui, e: &Entry, is_local: bool, acts: &mut Vec<Act>) {
    let open_label = if e.is_dir() {
        "Открыть"
    } else if crate::media::is_video(e) {
        "▶ Воспроизвести"
    } else {
        "Открыть в программе"
    };
    if ui.add(egui::Button::new(open_label).shortcut_text("Enter")).clicked() {
        acts.push(Act::Open(e.clone()));
    }
    if !e.is_dir() && ui.add(egui::Button::new("Быстрый просмотр").shortcut_text("Пробел")).clicked() {
        acts.push(Act::Preview(e.clone()));
    }
    ui.separator();
    if ui.add(egui::Button::new("Копировать в другую панель").shortcut_text("F5")).clicked() {
        acts.push(Act::Transfer(TransferOp::Copy));
    }
    if ui.add(egui::Button::new("Переместить в другую панель").shortcut_text("F6")).clicked() {
        acts.push(Act::Transfer(TransferOp::Move));
    }
    ui.separator();
    if ui.add(egui::Button::new("Переименовать").shortcut_text("F2")).clicked() {
        acts.push(Act::Rename);
    }
    if ui.add(egui::Button::new("Новая папка").shortcut_text("F7")).clicked() {
        acts.push(Act::NewFolder);
    }
    if ui.add(egui::Button::new("Удалить").shortcut_text("F8")).clicked() {
        acts.push(Act::Delete);
    }
    ui.separator();
    if is_local && ui.button("Показать в Finder").clicked() {
        acts.push(Act::Reveal(e.path.clone()));
    }
    if ui.button("Свойства").clicked() {
        acts.push(Act::Properties(e.clone()));
    }
}
