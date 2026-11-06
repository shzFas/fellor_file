//! Fellor File — two-pane file manager for Mac ↔ iPhone / Android / volumes.
//!
//! `Fellor` — real devices (fellor-devices + fellor-transfer).
//! `Fellor --demo` — in-memory fake devices and a fake transfer engine, for
//! trying the UI without hardware.

mod app;
mod demo;
mod devshot;
mod dialogs;
mod engine;
mod fmt;
mod import;
mod pane;
mod panes;
mod platform;
mod settings;
mod sidebar;
mod style;
mod toolbar;
mod transfers;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let demo = std::env::args().skip(1).any(|a| a == "--demo");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("fellor-rt")
        .build()
        .expect("failed to start tokio runtime");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Fellor File")
            .with_inner_size([1180.0, 740.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    // Demo mode keeps its own persisted settings so it never clobbers the real ones.
    let app_name = if demo { "Fellor File Demo" } else { "Fellor File" };
    eframe::run_native(
        app_name,
        options,
        Box::new(move |cc| Ok(Box::new(app::FellorApp::new(cc, runtime, demo)))),
    )
}
