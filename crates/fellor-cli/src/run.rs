//! Running a transfer task with a plain-text progress line on stderr and
//! interactive conflict prompts.
use fellor_transfer::*;
use fellor_vfs::VPath;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

pub fn fmt_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1000.0 && i + 1 < U.len() {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

pub struct TaskResult {
    pub outcome: TaskOutcome,
    pub failed: Vec<(VPath, String)>,
    pub done_files: u64,
    pub done_bytes: u64,
    pub elapsed: Duration,
}

struct Line {
    tty: bool,
    width: usize,
}

impl Line {
    fn draw(&mut self, s: &str) {
        if !self.tty {
            return;
        }
        let s: String = s.chars().take(120).collect();
        let pad = self.width.saturating_sub(s.chars().count());
        eprint!("\r{s}{}", " ".repeat(pad));
        self.width = s.chars().count();
        let _ = std::io::stderr().flush();
    }
    fn clear(&mut self) {
        if self.tty && self.width > 0 {
            eprint!("\r{}\r", " ".repeat(self.width));
            self.width = 0;
        }
    }
}

fn bar(done: u64, total: u64) -> String {
    let w = 24;
    let filled = if total == 0 { w } else { ((done as f64 / total as f64) * w as f64).round() as usize }.min(w);
    format!("[{}{}]", "#".repeat(filled), "-".repeat(w - filled))
}

async fn prompt_conflict(src: &fellor_vfs::Entry, dst: &fellor_vfs::Entry) -> (ConflictResolution, bool) {
    let fmt_t = |t: Option<std::time::SystemTime>| {
        t.map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| "?".into())
    };
    eprintln!("Конфликт: {} уже существует", dst.path);
    eprintln!("  источник: {:>10}  {}", src.size.map(fmt_bytes).unwrap_or_default(), fmt_t(src.mtime));
    eprintln!("  приёмник: {:>10}  {}", dst.size.map(fmt_bytes).unwrap_or_default(), fmt_t(dst.mtime));
    loop {
        eprint!("  [s]kip / [o]verwrite / [r]ename / [n]ewer (заглавная буква — для всех): ");
        let _ = std::io::stderr().flush();
        let line = tokio::task::spawn_blocking(|| {
            let mut s = String::new();
            std::io::stdin().read_line(&mut s).map(|n| (n, s))
        })
        .await;
        let s = match line {
            Ok(Ok((0, _))) | Ok(Err(_)) | Err(_) => return (ConflictResolution::Skip, true), // EOF: skip all
            Ok(Ok((_, s))) => s.trim().to_owned(),
        };
        let all = s.chars().next().is_some_and(|c| c.is_uppercase());
        let r = match s.to_lowercase().as_str() {
            "s" | "skip" => ConflictResolution::Skip,
            "o" | "overwrite" => ConflictResolution::Overwrite,
            "r" | "rename" => ConflictResolution::Rename,
            "n" | "newer" => ConflictResolution::KeepNewer,
            _ => continue,
        };
        return (r, all);
    }
}

/// Drive one task until `Finished`. Ctrl-C cancels it.
pub async fn run_task(engine: &TransferEngine, rx: &mut UnboundedReceiver<TransferEvent>, task: TaskId) -> TaskResult {
    let mut line = Line { tty: std::io::stderr().is_terminal(), width: 0 };
    let start = Instant::now();
    let mut failed = Vec::new();
    let (mut done_files, mut done_bytes) = (0, 0);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut cancelled_by_user = false;
    loop {
        let ev = tokio::select! {
            ev = rx.recv() => match ev { Some(ev) => ev, None => break },
            _ = &mut ctrl_c, if !cancelled_by_user => {
                cancelled_by_user = true;
                line.clear();
                eprintln!("отмена…");
                engine.cancel(task);
                continue;
            }
        };
        match ev {
            TransferEvent::Queued { task: t, label } if t == task => tracing::info!(%label, "queued"),
            TransferEvent::Planned { task: t, total_files, total_bytes } if t == task => {
                line.draw(&format!("{total_files} файлов, {}", fmt_bytes(total_bytes)));
            }
            TransferEvent::Progress { task: t, done_files: df, done_bytes: db, total_files, total_bytes, bytes_per_sec, current } if t == task => {
                done_files = df;
                done_bytes = db;
                let pct = if total_bytes == 0 { 100.0 } else { db as f64 * 100.0 / total_bytes as f64 };
                let name = current.as_ref().map(|c| c.name().to_owned()).unwrap_or_default();
                line.draw(&format!(
                    "{} {pct:5.1}%  {df}/{total_files}  {}/{}  {}/s  {name}",
                    bar(db, total_bytes),
                    fmt_bytes(db),
                    fmt_bytes(total_bytes),
                    fmt_bytes(bytes_per_sec as u64)
                ));
            }
            TransferEvent::Conflict { task: t, id, src, dst } if t == task => {
                line.clear();
                let (r, all) = prompt_conflict(&src, &dst).await;
                engine.resolve_conflict(task, id, r, all);
            }
            TransferEvent::FileFailed { task: t, path, error } if t == task => {
                line.clear();
                eprintln!("ошибка: {path}: {error}");
                failed.push((path, error));
            }
            TransferEvent::WaitingForDevice { task: t, backend } if t == task => {
                line.clear();
                eprintln!("ожидание устройства {backend} — подключите его снова…");
            }
            TransferEvent::Finished { task: t, outcome, .. } if t == task => {
                line.clear();
                return TaskResult { outcome, failed, done_files, done_bytes, elapsed: start.elapsed() };
            }
            _ => {}
        }
    }
    line.clear();
    TaskResult { outcome: TaskOutcome::Failed("engine stopped".into()), failed, done_files, done_bytes, elapsed: start.elapsed() }
}
