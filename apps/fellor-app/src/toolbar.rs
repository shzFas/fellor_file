//! Top toolbar: title, refresh, quick import, file actions, settings, help.

use crate::app::FellorApp;
use egui::{Layout, RichText};
use fellor_transfer::TransferOp;

impl FellorApp {
    pub fn toolbar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("▣ Fellor File").strong().size(15.0));
                if self.demo {
                    ui.label(RichText::new("демо").small().color(ui.visuals().warn_fg_color))
                        .on_hover_text("Запущено с --demo: устройства и файлы ненастоящие");
                }
                ui.separator();
                if ui.button("⟲").on_hover_text("Обновить обе панели (⌘R — активную)").clicked() {
                    self.load_pane(0);
                    self.load_pane(1);
                }
                let import = egui::Button::new(RichText::new("📥 Быстрый импорт").strong());
                if ui.add(import).on_hover_text("Импортировать новые фото и видео с устройства (⌘I)").clicked() {
                    self.start_import();
                }
                ui.separator();
                let has_sel = !self.panes[self.active].selected.is_empty();
                if ui.add_enabled(has_sel, egui::Button::new("Копировать →")).on_hover_text("В другую панель (F5)").clicked() {
                    self.transfer_to_other(TransferOp::Copy);
                }
                if ui.add_enabled(has_sel, egui::Button::new("Переместить →")).on_hover_text("В другую панель (F6)").clicked() {
                    self.transfer_to_other(TransferOp::Move);
                }
                if ui.button("Новая папка").on_hover_text("F7 / ⌘⇧N").clicked() {
                    self.ask_new_folder();
                }
                if ui.add_enabled(has_sel, egui::Button::new("Удалить")).on_hover_text("F8 / ⌘⌫").clicked() {
                    self.ask_delete();
                }
                ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("?").on_hover_text("Горячие клавиши").clicked() {
                        self.help_open = true;
                    }
                    if ui.button("⚙").on_hover_text("Настройки (⌘,)").clicked() {
                        self.settings_open = true;
                    }
                    let mut hidden = self.settings.show_hidden;
                    if ui.toggle_value(&mut hidden, "👁").on_hover_text("Показывать скрытые файлы (⌘⇧.)").changed() {
                        self.settings.show_hidden = hidden;
                        self.panes.iter_mut().for_each(|p| p.rebuild_view(hidden));
                    }
                });
            });
            ui.add_space(2.0);
        });
    }
}
