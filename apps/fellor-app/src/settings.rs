//! User settings, persisted through eframe storage.

use fellor_transfer::ConflictPolicy;
use serde::{Deserialize, Serialize};

pub const STORAGE_KEY: &str = "fellor-settings";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyChoice {
    Ask,
    Skip,
    Overwrite,
    Rename,
    KeepNewer,
    SkipIfSameSize,
}

impl PolicyChoice {
    pub const ALL: [PolicyChoice; 6] = [
        PolicyChoice::Ask,
        PolicyChoice::Skip,
        PolicyChoice::Overwrite,
        PolicyChoice::Rename,
        PolicyChoice::KeepNewer,
        PolicyChoice::SkipIfSameSize,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PolicyChoice::Ask => "Спрашивать",
            PolicyChoice::Skip => "Пропускать",
            PolicyChoice::Overwrite => "Заменять",
            PolicyChoice::Rename => "Переименовывать",
            PolicyChoice::KeepNewer => "Оставлять более новый",
            PolicyChoice::SkipIfSameSize => "Пропускать при одинаковом размере",
        }
    }

    pub fn policy(self) -> ConflictPolicy {
        match self {
            PolicyChoice::Ask => ConflictPolicy::Ask,
            PolicyChoice::Skip => ConflictPolicy::Skip,
            PolicyChoice::Overwrite => ConflictPolicy::Overwrite,
            PolicyChoice::Rename => ConflictPolicy::Rename,
            PolicyChoice::KeepNewer => ConflictPolicy::KeepNewer,
            PolicyChoice::SkipIfSameSize => ConflictPolicy::SkipIfSameSize,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub conflict_policy: PolicyChoice,
    pub show_hidden: bool,
    /// Host folder for Quick Import.
    pub import_dest: String,
    /// Subfolder template, e.g. "{date}".
    pub import_template: String,
    pub transfers_open: bool,
    /// Last local directories of the left/right pane (host paths).
    pub last_paths: [Option<String>; 2],
    /// Show a floating preview when the mouse rests on a file.
    pub hover_preview: bool,
    /// Hover preview edge length in points.
    pub hover_size: f32,
    /// Delay before the hover preview appears.
    pub hover_delay_ms: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            conflict_policy: PolicyChoice::Ask,
            show_hidden: false,
            import_dest: default_import_dest(),
            import_template: "{date}".into(),
            transfers_open: true,
            last_paths: [None, None],
            hover_preview: true,
            hover_size: 380.0,
            hover_delay_ms: 300,
        }
    }
}

pub fn default_import_dest() -> String {
    dirs::picture_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Pictures")))
        .map(|p| p.join("Fellor Import").to_string_lossy().into_owned())
        .unwrap_or_else(|| "/tmp/Fellor Import".into())
}
