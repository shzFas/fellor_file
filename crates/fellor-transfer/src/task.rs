//! One transfer task: plan → mkdirs → parallel file jobs → cleanup.
use crate::names::{chunk_size_for, is_device_backend, numbered_name};
use crate::progress::Progress;
use crate::stage::{StageGuard, TempReader, TempWriter};
use crate::*;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::AtomicU64;
use tokio::sync::OwnedSemaphorePermit;

pub(crate) fn label(req: &TransferRequest) -> String {
    let verb = match req.op {
        TransferOp::Copy => "Копирование",
        TransferOp::Move => "Перемещение",
    };
    let what = match req.sources.as_slice() {
        [one] if !one.is_root() => format!("«{}»", one.name()),
        [one] => one.to_string(),
        many => format!("{} объектов", many.len()),
    };
    let dest = if req.dest_dir.is_root() { req.dest_dir.to_string() } else { req.dest_dir.name().to_owned() };
    format!("{verb} {what} → {dest}")
}

struct FileJob {
    src: Entry,
    dst: VPath,
    /// Same-backend move via `Backend::rename`.
    rename_only: bool,
}

#[derive(Default)]
struct Plan {
    /// Destination dirs to create, parents first.
    dirs: Vec<VPath>,
    files: Vec<FileJob>,
    /// Source dirs to remove after a move, parents first (removed in reverse).
    src_dirs: Vec<VPath>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Read,
    Write,
}

enum Src {
    Backend(VPath),
    Temp(PathBuf),
}

enum Dst {
    Backend(VPath),
    Temp(PathBuf),
}

enum Done {
    Copied,
    Skipped,
}

/// Bytes credited to the task progress for the file currently in flight.
#[derive(Default)]
struct FileCredit {
    credited: u64,
}

#[derive(Default)]
struct PumpState {
    sink: Option<Box<dyn WriteSink>>,
    sink_owner: Option<BackendRef>,
    written: u64,
}

struct Run {
    id: TaskId,
    inner: Arc<Inner>,
    ctl: Arc<TaskCtl>,
    op: TransferOp,
    policy: ConflictPolicy,
    progress: Mutex<Progress>,
    failed: AtomicU64,
    remembered: Mutex<Option<ConflictResolution>>,
    ask_lock: tokio::sync::Mutex<()>,
    waiting: Mutex<HashSet<BackendId>>,
}

pub(crate) async fn run(inner: Arc<Inner>, id: TaskId, ctl: Arc<TaskCtl>, req: TransferRequest) {
    let r = Arc::new(Run {
        id,
        progress: Mutex::new(Progress::new(id, inner.cfg.progress_interval)),
        inner: inner.clone(),
        ctl: ctl.clone(),
        op: req.op,
        policy: req.policy,
        failed: AtomicU64::new(0),
        remembered: Mutex::new(None),
        ask_lock: tokio::sync::Mutex::new(()),
        waiting: Mutex::new(HashSet::new()),
    });
    let res = r.clone().execute(req).await;
    let outcome = if ctl.cancel.is_cancelled() {
        TaskOutcome::Cancelled
    } else {
        match res {
            Ok(()) => TaskOutcome::Done,
            Err(VfsError::Cancelled) => TaskOutcome::Cancelled,
            Err(e) => TaskOutcome::Failed(e.to_string()),
        }
    };
    if let Some(ev) = r.progress.lock().unwrap().tick(true) {
        inner.emit(ev);
    }
    inner.tasks.lock().unwrap().remove(&id);
    let failed_files = r.failed.load(Ordering::Relaxed);
    tracing::info!(task = id, ?outcome, failed_files, "transfer finished");
    inner.emit(TransferEvent::Finished { task: id, outcome, failed_files });
}

fn fmt_bytes(b: u64) -> String {
    const U: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1000.0 && i + 1 < U.len() {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} {}", U[0])
    } else {
        format!("{v:.1} {}", U[i])
    }
}

impl Run {
    fn cancelled(&self) -> bool {
        self.ctl.cancel.is_cancelled()
    }

    fn check_cancel(&self) -> Result<()> {
        if self.cancelled() {
            Err(VfsError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn emit(&self, ev: TransferEvent) {
        self.inner.emit(ev);
    }

    fn file_failed(&self, path: &VPath, e: &VfsError) {
        tracing::warn!(task = self.id, %path, error = %e, "file failed");
        self.failed.fetch_add(1, Ordering::Relaxed);
        self.emit(TransferEvent::FileFailed { task: self.id, path: path.clone(), error: e.to_string() });
    }

    async fn sleep(&self, d: Duration) -> Result<()> {
        tokio::select! {
            _ = self.ctl.cancel.cancelled() => Err(VfsError::Cancelled),
            _ = tokio::time::sleep(d) => Ok(()),
        }
    }

    async fn wait_unpaused(&self) -> Result<()> {
        self.check_cancel()?;
        if !*self.ctl.pause.borrow() {
            return Ok(());
        }
        let mut rx = self.ctl.pause.subscribe();
        loop {
            if !*rx.borrow_and_update() {
                return self.check_cancel();
            }
            tokio::select! {
                _ = self.ctl.cancel.cancelled() => return Err(VfsError::Cancelled),
                r = rx.changed() => if r.is_err() { return Ok(()) },
            }
        }
    }

    /// Backend by id; if it is gone, emit `WaitingForDevice` and wait until it
    /// reappears in the registry (`device_attached` or periodic check).
    async fn backend(&self, id: &str) -> Result<BackendRef> {
        if let Some(b) = self.inner.registry.get(id) {
            return Ok(b);
        }
        if self.waiting.lock().unwrap().insert(id.to_owned()) {
            tracing::info!(task = self.id, backend = id, "waiting for device");
            self.emit(TransferEvent::WaitingForDevice { task: self.id, backend: id.to_owned() });
        }
        loop {
            let notified = self.inner.device_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(b) = self.inner.registry.get(id) {
                if self.waiting.lock().unwrap().remove(id) {
                    self.emit(TransferEvent::Resumed { task: self.id });
                }
                return Ok(b);
            }
            tokio::select! {
                _ = self.ctl.cancel.cancelled() => return Err(VfsError::Cancelled),
                _ = &mut notified => {}
                _ = tokio::time::sleep(self.inner.cfg.device_poll) => {}
            }
        }
    }

    /// Decide what to do after a failed attempt. `Ok(())` = try again.
    async fn on_error(&self, e: VfsError, attempts: &mut u32, ids: &[&str]) -> Result<()> {
        self.check_cancel()?;
        if !e.is_retryable() {
            return Err(e);
        }
        let mut waited = false;
        for id in ids {
            if self.inner.registry.get(id).is_none() {
                self.backend(id).await?;
                waited = true;
            }
        }
        if waited {
            *attempts = 0;
            return Ok(());
        }
        *attempts += 1;
        if *attempts > self.inner.cfg.max_retries {
            return Err(e);
        }
        tracing::debug!(task = self.id, attempt = *attempts, error = %e, "retrying");
        self.sleep(self.inner.cfg.retry_backoff * (1 << (*attempts - 1).min(6))).await?;
        // The device may have dropped off while we slept.
        for id in ids {
            if self.inner.registry.get(id).is_none() {
                self.backend(id).await?;
                *attempts = 0;
            }
        }
        Ok(())
    }

    /// Run a single backend operation with retries / device waits.
    async fn retrying<T, F, Fut>(&self, id: &str, mut f: F) -> Result<T>
    where
        F: FnMut(BackendRef) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut attempts = 0;
        loop {
            self.check_cancel()?;
            let be = self.backend(id).await?;
            match f(be).await {
                Ok(v) => return Ok(v),
                Err(e) => self.on_error(e, &mut attempts, &[id]).await?,
            }
        }
    }

    async fn stat(&self, p: &VPath) -> Result<Entry> {
        self.retrying(&p.backend, |b| {
            let p = p.clone();
            async move { b.stat(&p).await }
        })
        .await
    }

    async fn stat_opt(&self, p: &VPath) -> Result<Option<Entry>> {
        match self.stat(p).await {
            Ok(e) => Ok(Some(e)),
            Err(VfsError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn remove(&self, p: &VPath, recursive: bool) -> Result<()> {
        self.retrying(&p.backend, |b| {
            let p = p.clone();
            async move { b.remove(&p, recursive).await }
        })
        .await
    }

    async fn rename(&self, from: &VPath, to: &VPath) -> Result<Entry> {
        self.retrying(&from.backend, |b| {
            let (from, to) = (from.clone(), to.clone());
            async move { b.rename(&from, &to).await }
        })
        .await
    }

    async fn mkdir_tolerant(&self, p: &VPath) -> Result<()> {
        let r = self
            .retrying(&p.backend, |b| {
                let p = p.clone();
                async move { b.mkdir(&p).await }
            })
            .await;
        match r {
            Ok(_) | Err(VfsError::AlreadyExists(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// mkdir -p for the destination directory.
    async fn ensure_dir(&self, dir: &VPath) -> Result<()> {
        match self.stat_opt(dir).await? {
            Some(e) if e.is_dir() => return Ok(()),
            Some(_) => return Err(VfsError::NotDir(dir.to_string())),
            None => {}
        }
        for i in 1..=dir.components.len() {
            let p = VPath::new(dir.backend.clone(), dir.components[..i].to_vec());
            match self.stat_opt(&p).await? {
                Some(e) if e.is_dir() => {}
                Some(_) => return Err(VfsError::NotDir(p.to_string())),
                None => self.mkdir_tolerant(&p).await?,
            }
        }
        Ok(())
    }

    fn credit(&self, fc: &mut FileCredit, value: u64) {
        let mut p = self.progress.lock().unwrap();
        p.add_bytes(value as i64 - fc.credited as i64);
        fc.credited = value;
        if let Some(ev) = p.tick(false) {
            drop(p);
            self.emit(ev);
        }
    }

    fn complete_file(&self, fc: &mut FileCredit, size: u64) {
        let mut p = self.progress.lock().unwrap();
        p.add_bytes(size as i64 - fc.credited as i64);
        fc.credited = size;
        p.done_files += 1;
        if let Some(ev) = p.tick(false) {
            drop(p);
            self.emit(ev);
        }
    }

    async fn execute(self: Arc<Self>, req: TransferRequest) -> Result<()> {
        let dst_be = self.backend(&req.dest_dir.backend).await?;
        self.ensure_dir(&req.dest_dir).await?;
        let plan = self.plan(&req).await?;
        let total_bytes: u64 = plan.files.iter().map(|j| j.src.size.unwrap_or(0)).sum();
        let total_files = plan.files.len() as u64;
        {
            let mut p = self.progress.lock().unwrap();
            p.total_files = total_files;
            p.total_bytes = total_bytes;
        }
        self.emit(TransferEvent::Planned { task: self.id, total_files, total_bytes });

        // Free-space precheck.
        let copy_bytes: u64 = plan.files.iter().filter(|j| !j.rename_only).map(|j| j.src.size.unwrap_or(0)).sum();
        if copy_bytes > 0 && dst_be.capabilities().free_space {
            if let Ok(Some(space)) = dst_be.free_space(&req.dest_dir).await {
                if space.free < copy_bytes {
                    return Err(VfsError::Other(format!(
                        "{}: нужно {}, свободно {}",
                        VfsError::NoSpace,
                        fmt_bytes(copy_bytes),
                        fmt_bytes(space.free)
                    )));
                }
            }
        }

        for d in &plan.dirs {
            self.check_cancel()?;
            if let Err(e) = self.mkdir_tolerant(d).await {
                if matches!(e, VfsError::Cancelled) {
                    return Err(e);
                }
                self.file_failed(d, &e);
            }
        }

        // Scheduler: per-task parallelism = min(max_parallel) of all involved backends.
        let mut ids: HashSet<&str> = req.sources.iter().map(|s| s.backend.as_str()).collect();
        ids.insert(&req.dest_dir.backend);
        let workers = ids
            .iter()
            .filter_map(|id| self.inner.registry.get(id))
            .map(|b| b.capabilities().max_parallel.max(1))
            .min()
            .unwrap_or(1)
            .min(plan.files.len().max(1));
        let queue = Arc::new(Mutex::new(plan.files.into_iter().collect::<VecDeque<_>>()));
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..workers {
            let me = self.clone();
            let q = queue.clone();
            set.spawn(async move {
                loop {
                    if me.cancelled() {
                        break;
                    }
                    let Some(job) = q.lock().unwrap().pop_front() else { break };
                    me.run_file(job).await;
                }
            });
        }
        while let Some(r) = set.join_next().await {
            if let Err(e) = r {
                tracing::error!(task = self.id, error = %e, "worker panicked");
            }
        }
        self.check_cancel()?;

        if self.op == TransferOp::Move {
            for d in plan.src_dirs.iter().rev() {
                if let Some(b) = self.inner.registry.get(&d.backend) {
                    let _ = b.remove(d, false).await; // NotEmpty when files were skipped/failed — fine.
                }
            }
        }
        Ok(())
    }

    async fn plan(&self, req: &TransferRequest) -> Result<Plan> {
        let mut plan = Plan::default();
        for src in &req.sources {
            self.check_cancel()?;
            let entry = match self.stat(src).await {
                Ok(e) => e,
                Err(VfsError::Cancelled) => return Err(VfsError::Cancelled),
                Err(e) => {
                    self.file_failed(src, &e);
                    continue;
                }
            };
            let name = if src.is_root() { entry.name.clone() } else { src.name().to_owned() };
            if name.is_empty() || name == "/" {
                self.file_failed(src, &VfsError::Unsupported("копирование корня устройства"));
                continue;
            }
            let entry = Entry { path: src.clone(), ..entry };
            let mut dst = req.dest_dir.join(&name);
            let same_be = src.backend == req.dest_dir.backend;
            let move_within = req.op == TransferOp::Move
                && same_be
                && self.inner.registry.get(&src.backend).map(|b| b.capabilities().move_within).unwrap_or(false);
            match entry.kind {
                EntryKind::Dir => {
                    if same_be && req.dest_dir.starts_with(src) {
                        self.file_failed(src, &VfsError::Other("нельзя скопировать папку внутрь самой себя".into()));
                        continue;
                    }
                    let mut exists = self.stat_opt(&dst).await?.is_some();
                    if exists && self.policy == ConflictPolicy::Rename {
                        dst = self.free_name(&dst).await?;
                        exists = false;
                    }
                    if move_within && !exists {
                        plan.files.push(FileJob { src: Entry { size: Some(0), ..entry }, dst, rename_only: true });
                    } else {
                        self.walk(entry, dst, move_within, &mut plan).await?;
                    }
                }
                EntryKind::File | EntryKind::Symlink => plan.files.push(FileJob { src: entry, dst, rename_only: move_within }),
                EntryKind::Special => self.file_failed(src, &VfsError::Unsupported("специальный файл")),
            }
        }
        Ok(plan)
    }

    async fn walk(&self, root: Entry, root_dst: VPath, rename_only: bool, plan: &mut Plan) -> Result<()> {
        plan.dirs.push(root_dst.clone());
        plan.src_dirs.push(root.path.clone());
        let mut stack = vec![(root.path, root_dst)];
        while let Some((s, d)) = stack.pop() {
            self.check_cancel()?;
            let entries = self
                .retrying(&s.backend, |b| {
                    let s = s.clone();
                    async move { b.list(&s).await }
                })
                .await;
            let entries = match entries {
                Ok(v) => v,
                Err(VfsError::Cancelled) => return Err(VfsError::Cancelled),
                Err(e) => {
                    self.file_failed(&s, &e);
                    continue;
                }
            };
            for e in entries {
                let dd = d.join(e.name.clone());
                match e.kind {
                    EntryKind::Dir => {
                        plan.dirs.push(dd.clone());
                        plan.src_dirs.push(e.path.clone());
                        stack.push((e.path.clone(), dd));
                    }
                    EntryKind::File => plan.files.push(FileJob { src: e, dst: dd, rename_only }),
                    EntryKind::Symlink => match self.stat(&e.path).await {
                        // Follow links to files; links to dirs are skipped (loops).
                        Ok(t) if t.kind == EntryKind::File => {
                            plan.files.push(FileJob { src: Entry { path: e.path, name: e.name, ..t }, dst: dd, rename_only })
                        }
                        _ => tracing::debug!(path = %e.path, "skipping symlink"),
                    },
                    EntryKind::Special => tracing::debug!(path = %e.path, "skipping special file"),
                }
            }
        }
        Ok(())
    }

    async fn free_name(&self, dst: &VPath) -> Result<VPath> {
        let parent = dst.parent().unwrap_or_else(|| VPath::root(dst.backend.clone()));
        for n in 2..10_000 {
            let cand = parent.join(numbered_name(dst.name(), n));
            if self.stat_opt(&cand).await?.is_none() {
                return Ok(cand);
            }
        }
        Err(VfsError::AlreadyExists(dst.to_string()))
    }

    async fn ask(&self, src: &Entry, dst: &Entry) -> Result<ConflictResolution> {
        let _g = self.ask_lock.lock().await;
        if let Some(r) = *self.remembered.lock().unwrap() {
            return Ok(r);
        }
        let id = self.inner.next_conflict.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.ctl.conflicts.lock().unwrap().insert(id, tx);
        self.emit(TransferEvent::Conflict { task: self.id, id, src: src.clone(), dst: dst.clone() });
        let r = tokio::select! {
            _ = self.ctl.cancel.cancelled() => Err(VfsError::Cancelled),
            r = rx => r.map_err(|_| VfsError::Cancelled),
        };
        self.ctl.conflicts.lock().unwrap().remove(&id);
        let (res, all) = r?;
        if all {
            *self.remembered.lock().unwrap() = Some(res);
        }
        Ok(res)
    }

    async fn decide(&self, src: &Entry, existing: &Entry) -> Result<ConflictResolution> {
        let r = match self.policy {
            ConflictPolicy::Ask => self.ask(src, existing).await?,
            ConflictPolicy::Skip => ConflictResolution::Skip,
            ConflictPolicy::Overwrite => ConflictResolution::Overwrite,
            ConflictPolicy::Rename => ConflictResolution::Rename,
            ConflictPolicy::KeepNewer => ConflictResolution::KeepNewer,
            ConflictPolicy::SkipIfSameSize => {
                if src.size == existing.size {
                    ConflictResolution::Skip
                } else {
                    ConflictResolution::Overwrite
                }
            }
        };
        Ok(match r {
            ConflictResolution::KeepNewer => {
                let secs = |t: std::time::SystemTime| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                let newer = match (src.mtime, existing.mtime) {
                    (Some(a), Some(b)) => secs(a) > secs(b),
                    (Some(_), None) => true,
                    _ => false,
                };
                if newer {
                    ConflictResolution::Overwrite
                } else {
                    ConflictResolution::Skip
                }
            }
            r => r,
        })
    }

    async fn run_file(&self, job: FileJob) {
        // A pending Ask conflict pauses the task: don't start new files meanwhile.
        drop(self.ask_lock.lock().await);
        if self.wait_unpaused().await.is_err() {
            return;
        }
        self.progress.lock().unwrap().current = Some(job.src.path.clone());
        let size = job.src.size.unwrap_or(0);
        let mut fc = FileCredit::default();
        match self.process(&job, &mut fc).await {
            Ok(_) => self.complete_file(&mut fc, size),
            Err(VfsError::Cancelled) if self.cancelled() => {}
            Err(e) => {
                self.file_failed(&job.src.path, &e);
                self.complete_file(&mut fc, size);
            }
        }
    }

    async fn process(&self, job: &FileJob, fc: &mut FileCredit) -> Result<Done> {
        let dst_be = self.backend(&job.dst.backend).await?;
        let mut target = job.dst.clone();
        let mut replace = false;
        if let Some(existing) = self.stat_opt(&job.dst).await? {
            let same = job.src.path == job.dst;
            match self.decide(&job.src, &existing).await? {
                ConflictResolution::Skip | ConflictResolution::KeepNewer => return Ok(Done::Skipped),
                ConflictResolution::Rename => target = self.free_name(&job.dst).await?,
                ConflictResolution::Overwrite => {
                    if same {
                        return Ok(Done::Skipped);
                    }
                    if existing.is_dir() {
                        return Err(VfsError::IsDir(job.dst.to_string()));
                    }
                    replace = true;
                }
            }
        }

        if job.rename_only {
            if replace {
                self.remove(&target, false).await?;
            }
            match self.rename(&job.src.path, &target).await {
                Ok(_) => return Ok(Done::Copied),
                // e.g. cross-device rename inside `local` → fall back to copy + delete.
                Err(VfsError::Io(_) | VfsError::Unsupported(_) | VfsError::Other(_)) if job.src.kind != EntryKind::Dir => {}
                Err(e) => return Err(e),
            }
        }

        let write_path = if replace && dst_be.capabilities().rename {
            let parent = target.parent().unwrap_or_else(|| VPath::root(target.backend.clone()));
            let tag = uuid::Uuid::new_v4().simple().to_string();
            parent.join(format!(".{}.fellor-new-{}", target.name(), &tag[..12]))
        } else {
            if replace {
                self.remove(&target, false).await?;
            }
            target.clone()
        };

        self.copy(&job.src, &write_path, fc).await?;

        if write_path != target {
            self.remove(&target, false).await?;
            if let Err(e) = self.rename(&write_path, &target).await {
                let _ = self.remove(&write_path, false).await;
                return Err(e);
            }
        }
        if let Some(mt) = job.src.mtime {
            if let Some(be) = self.inner.registry.get(&target.backend) {
                if be.capabilities().set_mtime {
                    if let Err(e) = be.set_mtime(&target, mt).await {
                        tracing::debug!(path = %target, error = %e, "set_mtime failed");
                    }
                }
            }
        }
        if self.op == TransferOp::Move {
            if let Err(e) = self.remove(&job.src.path, false).await {
                return Err(VfsError::Other(format!("скопировано, но не удалось удалить исходный файл: {e}")));
            }
        }
        Ok(Done::Copied)
    }

    async fn acquire(&self, ids: &[&str]) -> Result<Vec<OwnedSemaphorePermit>> {
        let mut ids: Vec<&str> = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let mut permits = Vec::new();
        for id in ids {
            let be = self.backend(id).await?;
            let sem = self.inner.slot(&be);
            let p = tokio::select! {
                _ = self.ctl.cancel.cancelled() => return Err(VfsError::Cancelled),
                p = sem.acquire_owned() => p.map_err(|_| VfsError::Cancelled)?,
            };
            permits.push(p);
        }
        Ok(permits)
    }

    async fn copy(&self, src: &Entry, write_path: &VPath, fc: &mut FileCredit) -> Result<Entry> {
        let size = src.size.unwrap_or(0);
        let (sb, db) = (src.path.backend.as_str(), write_path.backend.as_str());
        if is_device_backend(sb) && is_device_backend(db) {
            let tmp = self.inner.cfg.temp_dir.join(format!("fellor-stage-{}", uuid::Uuid::new_v4().simple()));
            let _guard = StageGuard(tmp.clone());
            {
                let _p = self.acquire(&[sb]).await?;
                self.pump(Src::Backend(src.path.clone()), Dst::Temp(tmp.clone()), size, None, fc, &|w| w / 2).await?;
            }
            let _p = self.acquire(&[db]).await?;
            self.pump(Src::Temp(tmp.clone()), Dst::Backend(write_path.clone()), size, src.mtime, fc, &|w| (size + w) / 2).await
        } else {
            let _p = self.acquire(&[sb, db]).await?;
            self.pump(Src::Backend(src.path.clone()), Dst::Backend(write_path.clone()), size, src.mtime, fc, &|w| w).await
        }
    }

    async fn pump(&self, src: Src, dst: Dst, size: u64, mtime: Option<std::time::SystemTime>, fc: &mut FileCredit, map: &(dyn Fn(u64) -> u64 + Sync)) -> Result<Entry> {
        let mut st = PumpState::default();
        let r = self.pump_inner(&src, &dst, size, mtime, fc, map, &mut st).await;
        if r.is_err() {
            if let Some(s) = st.sink.take() {
                abort_sink(s).await;
            }
        }
        r
    }

    #[allow(clippy::too_many_arguments)]
    async fn pump_inner(
        &self,
        src: &Src,
        dst: &Dst,
        size: u64,
        mtime: Option<std::time::SystemTime>,
        fc: &mut FileCredit,
        map: &(dyn Fn(u64) -> u64 + Sync),
        st: &mut PumpState,
    ) -> Result<Entry> {
        let src_id = match src {
            Src::Backend(p) => Some(p.backend.as_str()),
            Src::Temp(_) => None,
        };
        let dst_id = match dst {
            Dst::Backend(p) => Some(p.backend.as_str()),
            Dst::Temp(_) => None,
        };
        let ids: Vec<&str> = src_id.into_iter().chain(dst_id).collect();
        let chunk = chunk_size_for(src_id.unwrap_or("local")).min(chunk_size_for(dst_id.unwrap_or("local")));
        let mut attempts = 0u32;
        loop {
            self.check_cancel()?;
            let src_be = match src_id {
                Some(id) => Some(self.backend(id).await?),
                None => None,
            };
            let dst_be = match dst_id {
                Some(id) => Some(self.backend(id).await?),
                None => None,
            };
            // Device came back as a new object: the old sink is dead.
            if let (Some(owner), Some(d)) = (&st.sink_owner, &dst_be) {
                if !Arc::ptr_eq(owner, d) {
                    if let Some(s) = st.sink.take() {
                        abort_sink(s).await;
                    }
                    st.sink_owner = None;
                }
            }
            let partial = src_be.as_ref().map(|b| b.capabilities().partial_read).unwrap_or(true);
            if st.written > 0 && !partial {
                if let Some(s) = st.sink.take() {
                    abort_sink(s).await;
                }
            }
            if st.sink.is_none() {
                st.written = 0;
                self.credit(fc, map(0));
                let s = match (dst, &dst_be) {
                    (Dst::Backend(p), Some(b)) => b.create_write(p, size, mtime).await,
                    (Dst::Temp(t), _) => TempWriter::create(t).await.map(|w| Box::new(w) as Box<dyn WriteSink>),
                    _ => unreachable!(),
                };
                match s {
                    Ok(s) => {
                        st.sink = Some(s);
                        st.sink_owner = dst_be.clone();
                    }
                    Err(e) => {
                        self.on_error(e, &mut attempts, &ids).await?;
                        continue;
                    }
                }
            }
            let reader = match (src, &src_be) {
                (Src::Backend(p), Some(b)) => b.open_read(p, st.written).await,
                (Src::Temp(t), _) => TempReader::open(t, st.written).await.map(|r| Box::new(r) as Box<dyn ReadSource>),
                _ => unreachable!(),
            };
            let mut reader = match reader {
                Ok(r) => r,
                Err(e) => {
                    self.on_error(e, &mut attempts, &ids).await?;
                    continue;
                }
            };
            let (side, err) = loop {
                self.wait_unpaused().await?;
                let r = tokio::select! {
                    biased;
                    _ = self.ctl.cancel.cancelled() => return Err(VfsError::Cancelled),
                    r = reader.read_chunk(chunk) => r,
                };
                match r {
                    Err(e) => break (Side::Read, e),
                    Ok(None) => {
                        let sink = st.sink.take().expect("sink present");
                        st.sink_owner = None;
                        match sink.finish().await {
                            Ok(entry) => {
                                if let Some(s) = entry.size {
                                    if s != st.written {
                                        if let (Dst::Backend(p), Some(b)) = (dst, &dst_be) {
                                            let _ = b.remove(p, false).await;
                                        }
                                        return Err(VfsError::Protocol(format!("размер не совпал: записано {}, на приёмнике {s}", st.written)));
                                    }
                                }
                                if size > 0 && st.written != size {
                                    tracing::warn!(expected = size, got = st.written, "source size changed during copy");
                                }
                                return Ok(entry);
                            }
                            Err(e) => break (Side::Write, e),
                        }
                    }
                    Ok(Some(data)) => {
                        let n = data.len() as u64;
                        let sink = st.sink.as_mut().expect("sink present");
                        let w = tokio::select! {
                            biased;
                            _ = self.ctl.cancel.cancelled() => return Err(VfsError::Cancelled),
                            w = sink.write_chunk(data) => w,
                        };
                        if let Err(e) = w {
                            break (Side::Write, e);
                        }
                        st.written += n;
                        self.credit(fc, map(st.written));
                    }
                }
            };
            drop(reader);
            if side == Side::Write {
                if let Some(s) = st.sink.take() {
                    abort_sink(s).await;
                }
                st.sink_owner = None;
            }
            self.on_error(err, &mut attempts, &ids).await?;
        }
    }
}

async fn abort_sink(s: Box<dyn WriteSink>) {
    let _ = tokio::time::timeout(Duration::from_secs(10), s.abort()).await;
}
