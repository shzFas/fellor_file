//! Visual tweaks: a slightly roomier, macOS-like look in both themes.

pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(7.0, 3.0);
        style.spacing.interact_size.y = 22.0;
        let accent = egui::Color32::from_rgb(10, 132, 255);
        style.visuals.selection.bg_fill = accent.gamma_multiply(if style.visuals.dark_mode { 0.55 } else { 0.35 });
        style.visuals.selection.stroke = egui::Stroke::new(1.0, accent);
        style.visuals.hyperlink_color = accent;
    });
}
