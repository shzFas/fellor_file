//! `fellor` — command-line front-end for debugging backends and scripting.
mod doctor;
mod paths;
mod run;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use fellor_transfer::{ConflictPolicy, TaskOutcome, TransferEngine, TransferOp, TransferRequest};
use fellor_vfs::{BackendRef, BackendRegistry, Entry, EntryKind, VPath};
use paths::PathArg;
use run::fmt_bytes;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "fellor", version, about = "Fellor File — файлы iPhone, Android и томов из командной строки")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Policy {
    Ask,
    Skip,
    Overwrite,
    Rename,
    Newer,
    SkipSameSize,
}

impl From<Policy> for ConflictPolicy {
    fn from(p: Policy) -> Self {
        match p {
            Policy::Ask => ConflictPolicy::Ask,
            Policy::Skip => ConflictPolicy::Skip,
            Policy::Overwrite => ConflictPolicy::Overwrite,
            Policy::Rename => ConflictPolicy::Rename,
            Policy::Newer => ConflictPolicy::KeepNewer,
            Policy::SkipSameSize => ConflictPolicy::SkipIfSameSize,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// List devices and their states.
    Devices,
    /// List a directory: `fellor ls ios:/DCIM/100APPLE`, `fellor ls ~/Pictures`.
    Ls {
        path: String,
        /// Show hidden entries.
        #[arg(short = 'a', long)]
        all: bool,
    },
    /// Copy files/folders into a destination directory (created if missing).
    Cp {
        /// Sources followed by the destination directory.
        #[arg(required = true, num_args = 2..)]
        paths: Vec<String>,
        #[arg(long, value_enum, default_value = "ask")]
        on_conflict: Policy,
    },
    /// Move files/folders into a destination directory.
    Mv {
        #[arg(required = true, num_args = 2..)]
        paths: Vec<String>,
        #[arg(long, value_enum, default_value = "ask")]
        on_conflict: Policy,
    },
    /// Remove files (and folders with -r).
    Rm {
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(short = 'r', long)]
        recursive: bool,
    },
    /// Create directories.
    Mkdir {
        #[arg(required = true)]
        paths: Vec<String>,
        /// Create parents as needed; no error if it exists.
        #[arg(short = 'p', long)]
        parents: bool,
    },
    /// Import new photos/videos from a device's DCIM.
    Import {
        /// Backend id or prefix (`ios`, `mtp`, `vol:SD`, `ios:<udid>`) or a folder path containing DCIM.
        #[arg(long)]
        device: String,
        /// Destination root directory.
        #[arg(long)]
        dest: String,
        /// Sub-folder template: {date} {year} {month} {day} {device}.
        #[arg(long, default_value = "{date}")]
        template: String,
        /// Delete imported files from the device afterwards (asks for confirmation).
        #[arg(long)]
        delete_after: bool,
        /// Don't ask for confirmation with --delete-after.
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(long, value_enum, default_value = "skip-same-size")]
        on_conflict: Policy,
        /// Index database (default: ~/Library/Application Support/Fellor/index.db).
        #[arg(long)]
        index: Option<PathBuf>,
        /// Only show what would be imported.
        #[arg(long)]
        dry_run: bool,
    },
    /// Diagnose usbmuxd, devices, MTP-grabbing apps and folder access.
    Doctor,
}

struct Ctx {
    reg: BackendRegistry,
}

impl Ctx {
    async fn start() -> Self {
        let reg = BackendRegistry::new();
        let mut rx = fellor_devices::start(reg.clone());
        // Drain device events so the channel doesn't grow.
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        Ctx { reg }
    }

    /// Wait until every hint resolves (max 5 s); always at least until `local` is there.
    async fn wait_for(&self, hints: &[String]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let ready = self.reg.get("local").is_some() && hints.iter().all(|h| paths::resolve_backend(&self.reg, h).is_some());
            if ready || Instant::now() > deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Give discovery time to settle (devices/doctor).
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }

    async fn resolve_all(&self, args: &[String]) -> Result<Vec<VPath>> {
        let parsed: Vec<PathArg> = args.iter().map(|a| paths::parse(a)).collect();
        let hints: Vec<String> = parsed.iter().map(paths::hint).collect();
        self.wait_for(&hints).await;
        parsed
            .iter()
            .zip(args)
            .map(|(p, a)| paths::resolve(&self.reg, p).ok_or_else(|| anyhow!("устройство для «{a}» не найдено (см. `fellor devices`)")))
            .collect()
    }

    fn backend(&self, p: &VPath) -> Result<BackendRef> {
        self.reg.get(&p.backend).ok_or_else(|| anyhow!("устройство {} отключено", p.backend))
    }
}

fn fmt_time(t: Option<std::time::SystemTime>) -> String {
    t.map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| "                ".into())
}

fn print_entries(entries: &[Entry], all: bool) {
    for e in entries.iter().filter(|e| all || !e.hidden) {
        let (k, size) = match e.kind {
            EntryKind::Dir => ('d', "-".to_string()),
            EntryKind::File => ('-', e.size.map(fmt_bytes).unwrap_or_default()),
            EntryKind::Symlink => ('l', e.size.map(fmt_bytes).unwrap_or_default()),
            EntryKind::Special => ('s', String::new()),
        };
        let suffix = if e.is_dir() { "/" } else { "" };
        println!("{k} {size:>10}  {}  {}{suffix}", fmt_time(e.mtime), e.name);
    }
}

async fn cmd_devices(ctx: &Ctx) -> Result<()> {
    ctx.settle().await;
    let mut all = ctx.reg.all();
    all.sort_by_key(|b| b.id());
    for b in all {
        let i = b.info();
        let (state, hint) = doctor::state_hint(&i.state);
        let model = i.model.map(|m| format!(" ({m})")).unwrap_or_default();
        let os = i.os_version.map(|v| format!(" iOS/Android {v}")).unwrap_or_default();
        let hint = if hint.is_empty() { String::new() } else { format!(" — {hint}") };
        let space = match b.free_space(&VPath::root(i.id.clone())).await {
            Ok(Some(s)) => format!("  свободно {} из {}", fmt_bytes(s.free), fmt_bytes(s.total)),
            _ => String::new(),
        };
        println!("{:<28} {:<10?} {}{model}{os}  [{state}]{hint}{space}", i.id, i.kind, i.name);
    }
    Ok(())
}

async fn cmd_ls(ctx: &Ctx, path: &str, all: bool) -> Result<()> {
    let p = ctx.resolve_all(&[path.to_owned()]).await?.remove(0);
    let b = ctx.backend(&p)?;
    if p.is_root() {
        let roots = b.roots().await?;
        if !(roots.len() == 1 && roots[0].path.is_root()) {
            print_entries(&roots, all);
            return Ok(());
        }
    }
    let st = b.stat(&p).await?;
    if st.is_dir() {
        print_entries(&b.list(&p).await?, all);
    } else {
        print_entries(&[st], true);
    }
    Ok(())
}

async fn cmd_transfer(ctx: &Ctx, args: &[String], op: TransferOp, policy: Policy) -> Result<bool> {
    let mut all = ctx.resolve_all(args).await?;
    let dest = all.pop().unwrap();
    let (engine, mut rx) = TransferEngine::new(ctx.reg.clone());
    let id = engine.submit(TransferRequest { sources: all, dest_dir: dest.clone(), op, policy: policy.into() });
    let r = run::run_task(&engine, &mut rx, id).await;
    let secs = r.elapsed.as_secs_f64().max(0.001);
    match &r.outcome {
        TaskOutcome::Done => eprintln!(
            "готово: {} файлов, {} за {:.2} с ({}/s){}",
            r.done_files,
            fmt_bytes(r.done_bytes),
            secs,
            fmt_bytes((r.done_bytes as f64 / secs) as u64),
            if r.failed.is_empty() { String::new() } else { format!(", ошибок: {}", r.failed.len()) }
        ),
        TaskOutcome::Cancelled => eprintln!("отменено"),
        TaskOutcome::Failed(e) => eprintln!("не удалось: {e}"),
    }
    Ok(r.outcome == TaskOutcome::Done && r.failed.is_empty())
}

async fn cmd_rm(ctx: &Ctx, args: &[String], recursive: bool) -> Result<bool> {
    let mut ok = true;
    for (p, a) in ctx.resolve_all(args).await?.into_iter().zip(args) {
        if p.is_root() {
            eprintln!("rm: {a}: нельзя удалить корень");
            ok = false;
            continue;
        }
        if let Err(e) = ctx.backend(&p)?.remove(&p, recursive).await {
            eprintln!("rm: {a}: {e}");
            ok = false;
        }
    }
    Ok(ok)
}

async fn cmd_mkdir(ctx: &Ctx, args: &[String], parents: bool) -> Result<bool> {
    let mut ok = true;
    for (p, a) in ctx.resolve_all(args).await?.into_iter().zip(args) {
        let b = ctx.backend(&p)?;
        let r = if parents {
            let mut res = Ok(());
            for i in 1..=p.components.len() {
                let sub = VPath::new(p.backend.clone(), p.components[..i].to_vec());
                match b.stat(&sub).await {
                    Ok(e) if e.is_dir() => continue,
                    Ok(_) => {
                        res = Err(fellor_vfs::VfsError::NotDir(sub.to_string()));
                        break;
                    }
                    Err(_) => match b.mkdir(&sub).await {
                        Ok(_) | Err(fellor_vfs::VfsError::AlreadyExists(_)) => {}
                        Err(e) => {
                            res = Err(e);
                            break;
                        }
                    },
                }
            }
            res
        } else {
            b.mkdir(&p).await.map(|_| ())
        };
        if let Err(e) = r {
            eprintln!("mkdir: {a}: {e}");
            ok = false;
        }
    }
    Ok(ok)
}

fn confirm(question: &str) -> bool {
    use std::io::Write;
    eprint!("{question} [y/N]: ");
    let _ = std::io::stderr().flush();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).is_ok() && matches!(s.trim().to_lowercase().as_str(), "y" | "yes" | "д" | "да")
}

#[allow(clippy::too_many_arguments)]
async fn cmd_import(
    ctx: &Ctx,
    device: &str,
    dest: &str,
    template: &str,
    delete_after: bool,
    yes: bool,
    policy: Policy,
    index: Option<PathBuf>,
    dry_run: bool,
) -> Result<bool> {
    let hint = match device {
        "ios" | "iphone" | "ipad" => "ios".to_owned(),
        "mtp" | "android" => "mtp".to_owned(),
        // A folder (card dump, mounted camera dir…) is treated as a volume with DCIM inside.
        d if d.starts_with('/') || d.starts_with('~') || d.starts_with('.') => {
            let dir = paths::host_abs(d);
            if !dir.is_dir() {
                bail!("{} — не папка", dir.display());
            }
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "folder".into());
            let b = fellor_local::LocalBackend::volume(&name, dir);
            let id = b.id();
            ctx.reg.insert(b);
            id
        }
        d => d.to_owned(),
    };
    ctx.wait_for(std::slice::from_ref(&hint)).await;
    let id = paths::resolve_backend(&ctx.reg, &hint).ok_or_else(|| anyhow!("устройство «{device}» не найдено (см. `fellor devices`)"))?;
    let backend = ctx.reg.get(&id).context("устройство отключилось")?;
    let info = backend.info();
    if info.state != fellor_vfs::DeviceState::Ready {
        let (s, h) = doctor::state_hint(&info.state);
        bail!("{} ({}): {s} — {h}", info.name, info.id);
    }
    let index_path = index.unwrap_or_else(fellor_index::ImportIndex::default_path);
    let ix = fellor_index::ImportIndex::open(&index_path)?;
    eprintln!("поиск новых фото и видео на {} …", info.name);
    let plan = fellor_index::plan_import(backend.clone(), &ix).await?;
    eprintln!("новых: {} ({}), уже импортировано: {}", plan.new_files.len(), fmt_bytes(plan.total_bytes), plan.already_imported);
    if plan.new_files.is_empty() {
        return Ok(true);
    }
    let dest_root = ctx.resolve_all(&[dest.to_owned()]).await?.remove(0);
    let groups = fellor_index::group_by_subdir_with_device(&plan, template, &info.name);
    if dry_run {
        for (sub, files) in &groups {
            let dir = sub.split('/').fold(dest_root.clone(), |p, c| p.join(c));
            println!("{dir}  ({} файлов)", files.len());
            for f in files {
                println!("    {}", f.as_unix());
            }
        }
        return Ok(true);
    }
    let by_path: HashMap<&VPath, &Entry> = plan.new_files.iter().map(|e| (&e.path, e)).collect();
    let (engine, mut rx) = TransferEngine::new(ctx.reg.clone());
    let mut imported: Vec<&Entry> = Vec::new();
    let mut all_ok = true;
    for (sub, files) in &groups {
        let dir = sub.split('/').filter(|c| !c.is_empty()).fold(dest_root.clone(), |p, c| p.join(c));
        eprintln!("→ {dir} ({} файлов)", files.len());
        let task = engine.submit(TransferRequest { sources: files.clone(), dest_dir: dir.clone(), op: TransferOp::Copy, policy: policy.into() });
        let r = run::run_task(&engine, &mut rx, task).await;
        let failed: HashSet<&VPath> = r.failed.iter().map(|(p, _)| p).collect();
        match r.outcome {
            TaskOutcome::Done => {}
            TaskOutcome::Cancelled => {
                eprintln!("импорт отменён");
                return Ok(false);
            }
            TaskOutcome::Failed(e) => {
                eprintln!("ошибка: {e}");
                all_ok = false;
                continue;
            }
        }
        all_ok &= failed.is_empty();
        for f in files.iter().filter(|f| !failed.contains(f)) {
            let e = by_path[f];
            ix.mark_entry_imported(e, &dir.join(e.name.clone()).to_string())?;
            imported.push(e);
        }
    }
    eprintln!("импортировано: {} из {}", imported.len(), plan.new_files.len());
    if delete_after && !imported.is_empty() {
        if !yes && !confirm(&format!("Удалить {} импортированных файлов с {}?", imported.len(), info.name)) {
            eprintln!("файлы на устройстве оставлены");
            return Ok(all_ok);
        }
        let mut removed = 0;
        for e in &imported {
            match backend.remove(&e.path, false).await {
                Ok(()) => removed += 1,
                Err(err) => {
                    all_ok = false;
                    eprintln!("не удалось удалить {}: {err}", e.path);
                }
            }
        }
        eprintln!("удалено с устройства: {removed}");
    }
    Ok(all_ok)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let ctx = Ctx::start().await;
    let res: Result<bool> = match &cli.cmd {
        Cmd::Devices => cmd_devices(&ctx).await.map(|_| true),
        Cmd::Ls { path, all } => cmd_ls(&ctx, path, *all).await.map(|_| true),
        Cmd::Cp { paths, on_conflict } => cmd_transfer(&ctx, paths, TransferOp::Copy, *on_conflict).await,
        Cmd::Mv { paths, on_conflict } => cmd_transfer(&ctx, paths, TransferOp::Move, *on_conflict).await,
        Cmd::Rm { paths, recursive } => cmd_rm(&ctx, paths, *recursive).await,
        Cmd::Mkdir { paths, parents } => cmd_mkdir(&ctx, paths, *parents).await,
        Cmd::Import { device, dest, template, delete_after, yes, on_conflict, index, dry_run } => {
            cmd_import(&ctx, device, dest, template, *delete_after, *yes, *on_conflict, index.clone(), *dry_run).await
        }
        Cmd::Doctor => {
            ctx.settle().await;
            Ok(doctor::run(&ctx.reg).await)
        }
    };
    match res {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("fellor: {e:#}");
            std::process::exit(1);
        }
    }
}
