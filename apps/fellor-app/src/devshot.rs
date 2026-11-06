//! Developer hook for headless UI checks (works without Screen Recording
//! permission): `FELLOR_SCREENSHOT=out.png Fellor --demo` saves a capture of
//! the window after a few seconds and exits. `FELLOR_SCENE` picks a state to
//! show first: `copy`, `import`, `preview`, `delete`, `settings`, `conflict`,
//! `device` (opens the demo iPhone in the left pane).

use crate::app::FellorApp;
use fellor_transfer::TransferOp;
use fellor_vfs::VPath;
use std::time::{Duration, Instant};

pub struct DevShot {
    out: String,
    scene: String,
    start: Instant,
    step: u8,
    extra: bool,
}

impl DevShot {
    pub fn from_env() -> Option<Self> {
        let out = std::env::var("FELLOR_SCREENSHOT").ok()?;
        Some(Self { out, scene: std::env::var("FELLOR_SCENE").unwrap_or_default(), start: Instant::now(), step: 0, extra: false })
    }
}

impl FellorApp {
    pub fn devshot_tick(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.devshot.take() else { return };
        let t = d.start.elapsed();
        ctx.request_repaint_after(Duration::from_millis(100));
        // Keep painting even if the window is occluded by other windows.
        ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
        if d.step == 0 && t > Duration::from_millis(1200) {
            d.step = 1;
            let iphone = VPath::from_str_path("ios:DEMO00008110", "/DCIM/100APPLE");
            match d.scene.as_str() {
                "device" | "copy" | "preview" | "delete" | "conflict" => {
                    self.active = 0;
                    self.navigate(0, iphone);
                }
                "import" | "importrun" => self.start_import(),
                "settings" => self.settings_open = true,
                _ => {}
            }
        }
        if d.step == 1 && t > Duration::from_millis(2200) {
            d.step = 2;
            let p = &mut self.panes[0];
            match d.scene.as_str() {
                "copy" | "delete" | "conflict" => {
                    p.click(2, false, false);
                    p.click(9, false, true);
                    match d.scene.as_str() {
                        "copy" => self.transfer_to_other(TransferOp::Copy),
                        "delete" => self.ask_delete(),
                        _ => {
                            // Copy the same files twice to provoke a conflict.
                            self.settings.conflict_policy = crate::settings::PolicyChoice::Ask;
                            self.transfer_to_other(TransferOp::Copy);
                        }
                    }
                }
                "importrun" => self.submit_import(),
                "preview" => {
                    p.click(3, false, false);
                    if let Some(e) = p.cursor_entry().cloned() {
                        self.open_preview(e);
                    }
                }
                _ => {}
            }
        }
        if d.step == 2 && d.scene == "conflict" && !d.extra && t > Duration::from_millis(3600) {
            d.extra = true;
            self.transfer_to_other(TransferOp::Copy);
        }
        let shot_at = if d.scene == "conflict" { 5000 } else { 4200 };
        if d.step == 2 && t > Duration::from_millis(shot_at) {
            d.step = 3;
            let out = d.out.clone();
            let ctx2 = ctx.clone();
            ctx.request_screenshot(move |img| {
                let [w, h] = img.size;
                let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                match image::RgbaImage::from_raw(w as u32, h as u32, rgba) {
                    Some(buf) => match buf.save(&out) {
                        Ok(()) => eprintln!("screenshot saved to {out}"),
                        Err(e) => eprintln!("screenshot save failed: {e}"),
                    },
                    None => eprintln!("screenshot: bad buffer"),
                }
                ctx2.send_viewport_cmd(egui::ViewportCommand::Close);
            });
        }
        self.devshot = Some(d);
    }
}
