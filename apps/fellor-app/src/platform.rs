//! Thin wrappers over macOS command-line tools (`open`, `osascript`, `pkill`).
//! Every call spawns a child process and never waits on the UI thread.

use fellor_vfs::VPath;
use std::path::PathBuf;
use std::process::Command;

/// Host filesystem path for `local` and `vol:<name>` VPaths.
pub fn host_path(p: &VPath) -> Option<PathBuf> {
    if p.backend == "local" {
        return Some(PathBuf::from(p.as_unix()));
    }
    if let Some(name) = p.backend.strip_prefix("vol:") {
        let mut path = PathBuf::from("/Volumes").join(name);
        for c in &p.components {
            path.push(c);
        }
        return Some(path);
    }
    None
}

fn spawn_detached(cmd: &mut Command) -> Result<(), String> {
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    // Reap in the background so we leave no zombies.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// `open <path>` — open with the default application.
pub fn open(path: &std::path::Path) -> Result<(), String> {
    spawn_detached(Command::new("/usr/bin/open").arg(path))
}

/// `open -R <path>` — reveal in Finder.
pub fn reveal(path: &std::path::Path) -> Result<(), String> {
    spawn_detached(Command::new("/usr/bin/open").arg("-R").arg(path))
}

fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// macOS notification via `osascript` (no extra dependencies).
pub fn notify(title: &str, body: &str) {
    let script = format!(
        "display notification {} with title {}",
        applescript_quote(body),
        applescript_quote(title)
    );
    if let Err(e) = spawn_detached(Command::new("/usr/bin/osascript").arg("-e").arg(script)) {
        tracing::warn!("notification failed: {e}");
    }
}

/// Programs known to grab the MTP interface.
pub const MTP_HOGS: [&str; 3] = ["Android File Transfer", "Android File Transfer Agent", "OpenMTP"];

/// `pkill -x <name>` for each name.
pub fn kill_by_name(names: &[String]) {
    for n in names {
        let _ = spawn_detached(Command::new("/usr/bin/pkill").arg("-x").arg(n));
    }
}
