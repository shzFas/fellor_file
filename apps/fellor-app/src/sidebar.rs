//! Left sidebar: devices with state hints, volumes, Mac favourites.

use crate::app::FellorApp;
use crate::dialogs::Dialog;
use crate::platform;
use egui::{Color32, RichText};
use fellor_vfs::{DeviceInfo, DeviceKind, DeviceState, VPath};

struct Favourite {
    label: &'static str,
    icon: &'static str,
    path: Option<std::path::PathBuf>,
}

fn favourites() -> Vec<Favourite> {
    vec![
        Favourite { label: "Домой", icon: "🏠", path: dirs::home_dir() },
        Favourite { label: "Рабочий стол", icon: "🖥", path: dirs::desktop_dir() },
        Favourite { label: "Документы", icon: "📄", path: dirs::document_dir() },
        Favourite { label: "Загрузки", icon: "⬇", path: dirs::download_dir() },
        Favourite { label: "Изображения", icon: "🖼", path: dirs::picture_dir() },
        Favourite { label: "Фильмы", icon: "🎬", path: dirs::video_dir() },
    ]
}

fn device_icon(k: DeviceKind) -> &'static str {
    match k {
        DeviceKind::IPhone | DeviceKind::IPod | DeviceKind::Android => "📱",
        DeviceKind::IPad => "▭",
        DeviceKind::Camera => "📷",
        DeviceKind::Volume => "💾",
        DeviceKind::Mac => "💻",
    }
}

fn state_color(s: &DeviceState) -> Color32 {
    match s {
        DeviceState::Ready => Color32::from_rgb(52, 199, 89),
        DeviceState::Connecting => Color32::from_rgb(142, 142, 147),
        DeviceState::Error(_) => Color32::from_rgb(255, 69, 58),
        _ => Color32::from_rgb(255, 159, 10),
    }
}

fn state_badge(s: &DeviceState) -> &'static str {
    match s {
        DeviceState::Ready => "подключено",
        DeviceState::NeedsTrust => "нужно доверие",
        DeviceState::Locked => "заблокировано",
        DeviceState::NeedsMtpMode => "нужен режим MTP",
        DeviceState::Busy { .. } => "занято",
        DeviceState::Connecting => "подключение…",
        DeviceState::Error(_) => "ошибка",
    }
}

/// Hint texts from SPEC 5.4.
pub fn state_hint(d: &DeviceInfo) -> Option<String> {
    let ios = matches!(d.kind, DeviceKind::IPhone | DeviceKind::IPad | DeviceKind::IPod);
    let word = match d.kind {
        DeviceKind::IPad => "iPad",
        DeviceKind::IPod => "iPod",
        _ => "iPhone",
    };
    Some(match &d.state {
        DeviceState::Ready => return None,
        DeviceState::NeedsTrust => format!("Нажмите „Доверять“ на {word} и введите код-пароль"),
        DeviceState::Locked if ios => format!("Разблокируйте {word}"),
        DeviceState::Locked => "Разблокируйте экран телефона".into(),
        DeviceState::NeedsMtpMode => "На телефоне: Уведомление „Зарядка через USB“ → „Передача файлов“".into(),
        DeviceState::Busy { by: Some(by) } => format!("Закройте {by}"),
        DeviceState::Busy { by: None } => "Закройте Android File Transfer / OpenMTP".into(),
        DeviceState::Connecting => "Подключение…".into(),
        DeviceState::Error(e) => e.clone(),
    })
}

impl FellorApp {
    pub fn sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(220.0)
            .size_range(170.0..=340.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_space(6.0);
                    self.devices_section(ui);
                    ui.add_space(10.0);
                    self.volumes_section(ui);
                    ui.add_space(10.0);
                    self.favourites_section(ui);
                });
            });
    }

    fn section_title(ui: &mut egui::Ui, title: &str) {
        ui.label(RichText::new(title).small().strong().color(ui.visuals().weak_text_color()));
        ui.add_space(2.0);
    }

    fn devices_section(&mut self, ui: &mut egui::Ui) {
        Self::section_title(ui, "УСТРОЙСТВА");
        let devices: Vec<DeviceInfo> = self.devices.values().filter(|d| d.kind != DeviceKind::Volume).cloned().collect();
        if devices.is_empty() {
            ui.label(RichText::new("Подключите iPhone или Android-телефон кабелем USB").small().weak());
            return;
        }
        let active_backend = self.panes[self.active].path.backend.clone();
        for d in devices {
            let ready = d.state == DeviceState::Ready;
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 18.0), egui::Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, state_color(&d.state));
                let text = format!("{} {}", device_icon(d.kind), d.name);
                let resp = ui.selectable_label(active_backend == d.id, text);
                let resp = resp.on_hover_text(device_tooltip(&d));
                if resp.clicked() {
                    if ready {
                        self.open_device(&d.id);
                    } else {
                        self.recheck(d.id.clone());
                    }
                }
            });
            if !ready {
                ui.indent(("hint", &d.id), |ui| {
                    ui.label(RichText::new(state_badge(&d.state)).small().color(state_color(&d.state)));
                    if let Some(h) = state_hint(&d) {
                        ui.label(RichText::new(h).small());
                    }
                    ui.horizontal_wrapped(|ui| {
                        if self.rechecking.contains(&d.id) || d.state == DeviceState::Connecting {
                            ui.spinner();
                        } else if ui.small_button("Проверить снова").clicked() {
                            self.recheck(d.id.clone());
                        }
                        if let DeviceState::Busy { by } = &d.state {
                            if ui.small_button("Закрыть за меня").clicked() {
                                let names = match by {
                                    Some(b) => vec![b.clone()],
                                    None => platform::MTP_HOGS.iter().map(|s| s.to_string()).collect(),
                                };
                                self.dialog = Some(Dialog::KillBusy { device: d.id.clone(), names });
                            }
                        }
                    });
                });
                ui.add_space(4.0);
            }
        }
    }

    fn volumes_section(&mut self, ui: &mut egui::Ui) {
        let vols: Vec<DeviceInfo> = self.devices.values().filter(|d| d.kind == DeviceKind::Volume).cloned().collect();
        if vols.is_empty() {
            return;
        }
        Self::section_title(ui, "ТОМА");
        let active_backend = self.panes[self.active].path.backend.clone();
        for d in vols {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 18.0), egui::Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, state_color(&d.state));
                if ui.selectable_label(active_backend == d.id, format!("💾 {}", d.name)).clicked() {
                    self.open_device(&d.id);
                }
                if d.ejectable {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("⏏").on_hover_text("Извлечь (⌘E)").clicked() {
                            self.eject(d.id.clone());
                        }
                    });
                }
            });
        }
    }

    fn favourites_section(&mut self, ui: &mut egui::Ui) {
        Self::section_title(ui, "МАК");
        let current = self.panes[self.active].path.clone();
        for f in favourites() {
            let Some(p) = f.path else { continue };
            let vp: VPath = fellor_local::vpath_from_host(&p);
            if ui.selectable_label(current == vp, format!("{} {}", f.icon, f.label)).clicked() {
                let i = self.active;
                self.navigate(i, vp);
            }
        }
        let root = VPath::root("local");
        if ui.selectable_label(current == root, "💻 Macintosh HD").clicked() {
            let i = self.active;
            self.navigate(i, root);
        }
    }
}

fn device_tooltip(d: &DeviceInfo) -> String {
    let mut s = d.name.clone();
    if let Some(m) = &d.model {
        s.push_str(&format!("\nМодель: {m}"));
    }
    if let Some(v) = &d.os_version {
        s.push_str(&format!("\nВерсия ОС: {v}"));
    }
    s.push_str(&format!("\nСостояние: {}", state_badge(&d.state)));
    s
}
