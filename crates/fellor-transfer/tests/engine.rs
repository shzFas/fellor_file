mod common;

use common::Fake;
use fellor_local::LocalBackend;
use fellor_transfer::*;
use fellor_vfs::*;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::UnboundedReceiver;

fn t(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn cfg(tmp: &Path) -> EngineConfig {
    EngineConfig {
        max_retries: 3,
        retry_backoff: Duration::from_millis(5),
        device_poll: Duration::from_millis(50),
        progress_interval: Duration::from_millis(100),
        temp_dir: tmp.to_path_buf(),
    }
}

struct Env {
    reg: BackendRegistry,
    engine: TransferEngine,
    rx: UnboundedReceiver<TransferEvent>,
    _stage: tempfile::TempDir,
    stage: std::path::PathBuf,
}

fn env() -> Env {
    let reg = BackendRegistry::new();
    let stage = tempfile::tempdir().unwrap();
    let (engine, rx) = TransferEngine::with_config(reg.clone(), cfg(stage.path()));
    Env { reg, engine, rx, stage: stage.path().to_path_buf(), _stage: stage }
}

fn add(reg: &BackendRegistry, f: &Arc<Fake>) {
    reg.insert(f.clone() as BackendRef);
}

fn req(sources: Vec<VPath>, dest: VPath, op: TransferOp, policy: ConflictPolicy) -> TransferRequest {
    TransferRequest { sources, dest_dir: dest, op, policy }
}

async fn wait_done(rx: &mut UnboundedReceiver<TransferEvent>, task: TaskId) -> Vec<TransferEvent> {
    let mut evs = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.expect("timeout waiting for events").expect("channel closed");
        let fin = matches!(&ev, TransferEvent::Finished { task: t, .. } if *t == task);
        evs.push(ev);
        if fin {
            return evs;
        }
    }
}

fn finished(evs: &[TransferEvent]) -> (TaskOutcome, u64) {
    match evs.last().unwrap() {
        TransferEvent::Finished { outcome, failed_files, .. } => (outcome.clone(), *failed_files),
        e => panic!("last event is {e:?}"),
    }
}

fn last_progress(evs: &[TransferEvent]) -> (u64, u64, u64, u64) {
    evs.iter()
        .rev()
        .find_map(|e| match e {
            TransferEvent::Progress { done_files, done_bytes, total_files, total_bytes, .. } => Some((*done_files, *done_bytes, *total_files, *total_bytes)),
            _ => None,
        })
        .unwrap()
}

fn data(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in walk(dir) {
        out.push(e.strip_prefix(dir).unwrap().to_string_lossy().into_owned());
    }
    out.sort();
    out
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        v.push(p.clone());
        if p.is_dir() {
            v.extend(walk(&p));
        }
    }
    v
}

// ---------------------------------------------------------------- local

#[tokio::test]
async fn recursive_copy_local_to_local() {
    let mut e = env();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(a.path().join("Photos/2024/empty")).unwrap();
    std::fs::write(a.path().join("Photos/a.jpg"), data(10_000, 1)).unwrap();
    std::fs::write(a.path().join("Photos/2024/b.mov"), data(20_000, 2)).unwrap();
    std::fs::write(a.path().join("top.txt"), b"hi").unwrap();
    std::fs::File::options().write(true).open(a.path().join("Photos/a.jpg")).unwrap().set_modified(t(1_600_000_000)).unwrap();
    e.reg.insert(LocalBackend::volume("A", a.path().into()));
    e.reg.insert(LocalBackend::volume("B", b.path().into()));

    let id = e.engine.submit(req(
        vec![VPath::from_str_path("vol:A", "/Photos"), VPath::from_str_path("vol:A", "/top.txt")],
        VPath::from_str_path("vol:B", "/in/here"),
        TransferOp::Copy,
        ConflictPolicy::Ask,
    ));
    let evs = wait_done(&mut e.rx, id).await;
    assert!(matches!(evs[0], TransferEvent::Queued { .. }));
    assert!(evs.iter().any(|e| matches!(e, TransferEvent::Planned { total_files: 3, total_bytes: 30_002, .. })));
    assert_eq!(finished(&evs), (TaskOutcome::Done, 0));
    assert_eq!(last_progress(&evs), (3, 30_002, 3, 30_002));
    let dst = b.path().join("in/here");
    assert_eq!(files_in(&dst), vec!["Photos", "Photos/2024", "Photos/2024/b.mov", "Photos/2024/empty", "Photos/a.jpg", "top.txt"]);
    assert_eq!(std::fs::read(dst.join("Photos/2024/b.mov")).unwrap(), data(20_000, 2));
    assert_eq!(std::fs::metadata(dst.join("Photos/a.jpg")).unwrap().modified().unwrap(), t(1_600_000_000));
    // sources untouched
    assert!(a.path().join("Photos/a.jpg").exists());
}

#[tokio::test]
async fn move_across_backends_removes_source() {
    let mut e = env();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(a.path().join("d/sub")).unwrap();
    std::fs::write(a.path().join("d/sub/x"), b"xx").unwrap();
    std::fs::write(a.path().join("d/y"), b"y").unwrap();
    e.reg.insert(LocalBackend::volume("A", a.path().into()));
    e.reg.insert(LocalBackend::volume("B", b.path().into()));
    let id = e.engine.submit(req(vec![VPath::from_str_path("vol:A", "/d")], VPath::root("vol:B"), TransferOp::Move, ConflictPolicy::Skip));
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs), (TaskOutcome::Done, 0));
    assert_eq!(std::fs::read(b.path().join("d/sub/x")).unwrap(), b"xx");
    assert!(!a.path().join("d").exists(), "source dir removed");
}

#[tokio::test]
async fn move_within_backend_uses_rename() {
    let mut e = env();
    let f = Fake::new("mtp:one", 1);
    f.put_file("/a/1.jpg", b"one", t(10));
    f.put_file("/a/sub/2.jpg", b"two", t(10));
    f.put_file("/b.txt", b"b", t(10));
    f.put_dir("/dest");
    add(&e.reg, &f);
    let id = e.engine.submit(req(vec![f.vp("/a"), f.vp("/b.txt")], f.vp("/dest"), TransferOp::Move, ConflictPolicy::Ask));
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs), (TaskOutcome::Done, 0));
    assert_eq!(f.renames.load(Ordering::SeqCst), 2);
    assert!(f.open_offsets.lock().unwrap().is_empty(), "no data copied");
    assert_eq!(f.get("/dest/a/sub/2.jpg").unwrap(), b"two");
    assert_eq!(f.get("/dest/b.txt").unwrap(), b"b");
    assert!(!f.exists("/a"));
}

#[tokio::test]
async fn move_within_merges_into_existing_dir() {
    let mut e = env();
    let f = Fake::new("mtp:one", 1);
    f.put_file("/a/1.jpg", b"one", t(10));
    f.put_file("/a/2.jpg", b"new", t(10));
    f.put_file("/dest/a/2.jpg", b"old", t(10));
    add(&e.reg, &f);
    let id = e.engine.submit(req(vec![f.vp("/a")], f.vp("/dest"), TransferOp::Move, ConflictPolicy::Skip));
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs), (TaskOutcome::Done, 0));
    assert_eq!(f.get("/dest/a/1.jpg").unwrap(), b"one");
    assert_eq!(f.get("/dest/a/2.jpg").unwrap(), b"old");
    // skipped file stays at the source, so the source dir stays too
    assert_eq!(f.get("/a/2.jpg").unwrap(), b"new");
    assert!(!f.exists("/a/1.jpg"));
}

// ---------------------------------------------------------------- conflicts

async fn conflict_case(policy: ConflictPolicy, src_mtime: u64, dst_mtime: u64, dst_data: &[u8]) -> (Arc<Fake>, Vec<TransferEvent>) {
    let mut e = env();
    let s = Fake::new("vol:s", 4);
    let d = Fake::new("vol:d", 4);
    s.put_file("/f.jpg", b"source", t(src_mtime));
    d.put_file("/out/f.jpg", dst_data, t(dst_mtime));
    add(&e.reg, &s);
    add(&e.reg, &d);
    let id = e.engine.submit(req(vec![s.vp("/f.jpg")], d.vp("/out"), TransferOp::Copy, policy));
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs).0, TaskOutcome::Done);
    (d, evs)
}

#[tokio::test]
async fn policy_skip_overwrite_rename() {
    let (d, _) = conflict_case(ConflictPolicy::Skip, 20, 10, b"dest").await;
    assert_eq!(d.get("/out/f.jpg").unwrap(), b"dest");
    let (d, _) = conflict_case(ConflictPolicy::Overwrite, 20, 10, b"dest").await;
    assert_eq!(d.get("/out/f.jpg").unwrap(), b"source");
    assert_eq!(d.mtime("/out/f.jpg").unwrap(), t(20));
    assert!(d.paths().iter().all(|p| !p.contains("fellor-new")), "no temp left: {:?}", d.paths());
    let (d, _) = conflict_case(ConflictPolicy::Rename, 20, 10, b"dest").await;
    assert_eq!(d.get("/out/f.jpg").unwrap(), b"dest");
    assert_eq!(d.get("/out/f (2).jpg").unwrap(), b"source");
}

#[tokio::test]
async fn policy_keep_newer_and_same_size() {
    let (d, _) = conflict_case(ConflictPolicy::KeepNewer, 20, 10, b"dest").await;
    assert_eq!(d.get("/out/f.jpg").unwrap(), b"source");
    let (d, _) = conflict_case(ConflictPolicy::KeepNewer, 10, 20, b"dest").await;
    assert_eq!(d.get("/out/f.jpg").unwrap(), b"dest");
    let (d, _) = conflict_case(ConflictPolicy::SkipIfSameSize, 20, 10, b"SOURCE").await;
    assert_eq!(d.get("/out/f.jpg").unwrap(), b"SOURCE");
    let (d, _) = conflict_case(ConflictPolicy::SkipIfSameSize, 20, 10, b"different-size").await;
    assert_eq!(d.get("/out/f.jpg").unwrap(), b"source");
}

#[tokio::test]
async fn rename_numbering_continues() {
    let mut e = env();
    let s = Fake::new("vol:s", 4);
    let d = Fake::new("vol:d", 4);
    s.put_file("/IMG_0001.HEIC", b"new", t(1));
    d.put_file("/IMG_0001.HEIC", b"a", t(1));
    d.put_file("/IMG_0001 (2).HEIC", b"b", t(1));
    add(&e.reg, &s);
    add(&e.reg, &d);
    let id = e.engine.submit(req(vec![s.vp("/IMG_0001.HEIC")], d.vp("/"), TransferOp::Copy, ConflictPolicy::Rename));
    wait_done(&mut e.rx, id).await;
    assert_eq!(d.get("/IMG_0001 (3).HEIC").unwrap(), b"new");
    // Same for a folder: copying a folder next to itself → "Folder (2)"
    s.put_file("/Folder/x", b"x", t(1));
    let id = e.engine.submit(req(vec![s.vp("/Folder")], s.vp("/"), TransferOp::Copy, ConflictPolicy::Rename));
    wait_done(&mut e.rx, id).await;
    assert_eq!(s.get("/Folder (2)/x").unwrap(), b"x");
}

#[tokio::test]
async fn ask_with_apply_to_all() {
    let mut e = env();
    let s = Fake::new("vol:s", 4);
    let d = Fake::new("vol:d", 4);
    for i in 0..5 {
        s.put_file(&format!("/f{i}"), b"new", t(1));
        if i < 4 {
            d.put_file(&format!("/f{i}"), b"old", t(1));
        }
    }
    add(&e.reg, &s);
    add(&e.reg, &d);
    let id = e.engine.submit(req(vec![s.vp("/")], d.vp("/"), TransferOp::Copy, ConflictPolicy::Ask));
    // root source → name is "/" → rejected; use the files instead
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs).1, 1);

    let sources: Vec<_> = (0..5).map(|i| s.vp(&format!("/f{i}"))).collect();
    let id = e.engine.submit(req(sources, d.vp("/"), TransferOp::Copy, ConflictPolicy::Ask));
    let mut conflicts = 0;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), e.rx.recv()).await.unwrap().unwrap();
        match ev {
            TransferEvent::Conflict { task, id: cid, src, dst } => {
                conflicts += 1;
                assert_eq!(src.name, dst.name);
                e.engine.resolve_conflict(task, cid, ConflictResolution::Overwrite, true);
            }
            TransferEvent::Finished { task, outcome, failed_files } if task == id => {
                assert_eq!(outcome, TaskOutcome::Done);
                assert_eq!(failed_files, 0);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(conflicts, 1, "apply_to_all remembers the choice");
    for i in 0..5 {
        assert_eq!(d.get(&format!("/f{i}")).unwrap(), b"new");
    }
}

#[tokio::test]
async fn ask_each_time_without_apply_to_all() {
    let mut e = env();
    let s = Fake::new("vol:s", 4);
    let d = Fake::new("vol:d", 4);
    for i in 0..3 {
        s.put_file(&format!("/f{i}"), b"new", t(1));
        d.put_file(&format!("/f{i}"), b"old", t(1));
    }
    add(&e.reg, &s);
    add(&e.reg, &d);
    let id = e.engine.submit(req((0..3).map(|i| s.vp(&format!("/f{i}"))).collect(), d.vp("/"), TransferOp::Copy, ConflictPolicy::Ask));
    let mut conflicts = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), e.rx.recv()).await.unwrap().unwrap() {
            TransferEvent::Conflict { task, id: cid, src, .. } => {
                let r = match src.name.as_str() {
                    "f0" => ConflictResolution::Skip,
                    "f1" => ConflictResolution::Rename,
                    _ => ConflictResolution::Overwrite,
                };
                conflicts.push(src.name.clone());
                e.engine.resolve_conflict(task, cid, r, false);
            }
            TransferEvent::Finished { task, .. } if task == id => break,
            _ => {}
        }
    }
    assert_eq!(conflicts.len(), 3);
    assert_eq!(d.get("/f0").unwrap(), b"old");
    assert_eq!(d.get("/f1").unwrap(), b"old");
    assert_eq!(d.get("/f1 (2)").unwrap(), b"new");
    assert_eq!(d.get("/f2").unwrap(), b"new");
}

// ---------------------------------------------------------------- cancel / pause

#[tokio::test]
async fn cancel_mid_file_leaves_no_partial() {
    let mut e = env();
    let s = Fake::new("mtp:s", 1);
    s.put_file("/big.mp4", &data(100_000, 3), t(1));
    *s.serve_chunk.lock().unwrap() = 1000;
    *s.delay.lock().unwrap() = Duration::from_millis(5);
    add(&e.reg, &s);
    let b = tempfile::tempdir().unwrap();
    e.reg.insert(LocalBackend::volume("B", b.path().into()));
    let id = e.engine.submit(req(vec![s.vp("/big.mp4")], VPath::root("vol:B"), TransferOp::Copy, ConflictPolicy::Skip));
    loop {
        if let TransferEvent::Progress { done_bytes, .. } = e.rx.recv().await.unwrap() {
            if done_bytes > 0 {
                break;
            }
        }
    }
    // the temp file exists while copying
    assert!(files_in(b.path()).iter().any(|n| n.contains("fellor-tmp")));
    e.engine.cancel(id);
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs).0, TaskOutcome::Cancelled);
    assert!(files_in(b.path()).is_empty(), "left: {:?}", files_in(b.path()));
}

#[tokio::test]
async fn pause_and_resume() {
    let mut e = env();
    let s = Fake::new("vol:s", 4);
    let d = Fake::new("vol:d", 4);
    s.put_file("/f", &data(20_000, 1), t(1));
    *s.serve_chunk.lock().unwrap() = 1000;
    *s.delay.lock().unwrap() = Duration::from_millis(5);
    add(&e.reg, &s);
    add(&e.reg, &d);
    let id = e.engine.submit(req(vec![s.vp("/f")], d.vp("/"), TransferOp::Copy, ConflictPolicy::Skip));
    e.engine.pause(id);
    e.engine.pause(id); // idempotent: one Paused event
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(d.get("/f").is_none());
    assert!(s.read_sizes.lock().unwrap().len() <= 1);
    e.engine.resume(id);
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(evs.iter().filter(|e| matches!(e, TransferEvent::Paused { .. })).count(), 1);
    assert_eq!(evs.iter().filter(|e| matches!(e, TransferEvent::Resumed { .. })).count(), 1);
    assert_eq!(d.get("/f").unwrap(), data(20_000, 1));
}

// ---------------------------------------------------------------- retries

#[tokio::test]
async fn retry_resumes_with_offset() {
    let mut e = env();
    let s = Fake::new("vol:s", 4);
    let d = Fake::new("vol:d", 4);
    s.put_file("/f", &data(10_000, 7), t(1));
    *s.serve_chunk.lock().unwrap() = 1000;
    s.fail_reads.lock().unwrap().extend([3000, 7000]);
    add(&e.reg, &s);
    add(&e.reg, &d);
    let id = e.engine.submit(req(vec![s.vp("/f")], d.vp("/"), TransferOp::Copy, ConflictPolicy::Skip));
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs), (TaskOutcome::Done, 0));
    assert_eq!(d.get("/f").unwrap(), data(10_000, 7));
    assert_eq!(*s.open_offsets.lock().unwrap(), vec![0, 3000, 7000]);
    assert_eq!(last_progress(&evs).1, 10_000);
}

#[tokio::test]
async fn retry_restarts_without_partial_read() {
    let mut e = env();
    let s = Fake::new("vol:s", 4);
    s.caps.lock().unwrap().partial_read = false;
    let d = Fake::new("vol:d", 4);
    s.put_file("/f", &data(5000, 7), t(1));
    *s.serve_chunk.lock().unwrap() = 1000;
    s.fail_reads.lock().unwrap().push_back(3000);
    *d.fail_writes.lock().unwrap() = 1;
    add(&e.reg, &s);
    add(&e.reg, &d);
    let id = e.engine.submit(req(vec![s.vp("/f")], d.vp("/"), TransferOp::Copy, ConflictPolicy::Skip));
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs), (TaskOutcome::Done, 0));
    assert_eq!(d.get("/f").unwrap(), data(5000, 7));
    assert!(s.open_offsets.lock().unwrap().iter().all(|&o| o == 0));
}

#[tokio::test]
async fn exhausted_retries_and_logical_errors_fail_file_but_task_continues() {
    let mut e = env();
    let s = Fake::new("vol:s", 4);
    let d = Fake::new("vol:d", 4);
    s.put_file("/bad", &data(3000, 1), t(1));
    s.put_file("/denied", b"x", t(1));
    s.put_file("/good", b"ok", t(1));
    *s.serve_chunk.lock().unwrap() = 1000;
    s.fail_reads.lock().unwrap().extend([1000; 10]);
    *s.deny_name.lock().unwrap() = Some("denied".into());
    *s.caps.lock().unwrap() = common::caps(1); // sequential so "bad" consumes the injected errors
    add(&e.reg, &s);
    add(&e.reg, &d);
    let id = e.engine.submit(req(vec![s.vp("/bad"), s.vp("/denied"), s.vp("/good")], d.vp("/"), TransferOp::Copy, ConflictPolicy::Skip));
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs), (TaskOutcome::Done, 2));
    let failed: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            TransferEvent::FileFailed { path, .. } => Some(path.name().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(failed, vec!["bad", "denied"]);
    assert!(d.get("/bad").is_none());
    assert_eq!(d.get("/good").unwrap(), b"ok");
    // 1 initial attempt + 3 retries for "bad"
    assert_eq!(s.open_offsets.lock().unwrap().iter().filter(|&&o| o == 1000).count(), 3);
}

#[tokio::test]
async fn waiting_for_device_then_resume() {
    let mut e = env();
    let s = Fake::new("ios:phone", 1);
    s.put_file("/DCIM/IMG.HEIC", &data(10_000, 9), t(1));
    *s.serve_chunk.lock().unwrap() = 1000;
    let reg = e.reg.clone();
    // Unplug the phone after 4000 bytes.
    *s.on_read.lock().unwrap() = Some(Box::new(move |f, pos| {
        if pos == 4000 {
            f.disconnected.store(true, Ordering::SeqCst);
            reg.remove(&f.id);
        }
    }));
    add(&e.reg, &s);
    let b = tempfile::tempdir().unwrap();
    e.reg.insert(LocalBackend::volume("B", b.path().into()));
    let id = e.engine.submit(req(vec![s.vp("/DCIM/IMG.HEIC")], VPath::root("vol:B"), TransferOp::Copy, ConflictPolicy::Skip));
    loop {
        if let TransferEvent::WaitingForDevice { backend, .. } = e.rx.recv().await.unwrap() {
            assert_eq!(backend, "ios:phone");
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(b.path().read_dir().unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with("IMG")));
    // Plug back in.
    s.disconnected.store(false, Ordering::SeqCst);
    add(&e.reg, &s);
    e.engine.device_attached(&"ios:phone".to_string());
    let evs = wait_done(&mut e.rx, id).await;
    assert!(evs.iter().any(|e| matches!(e, TransferEvent::Resumed { .. })));
    assert_eq!(finished(&evs), (TaskOutcome::Done, 0));
    assert_eq!(std::fs::read(b.path().join("IMG.HEIC")).unwrap(), data(10_000, 9));
    assert_eq!(*s.open_offsets.lock().unwrap(), vec![0, 4000], "resumed at the offset");
    // iOS chunking: 1 MiB reads
    assert!(s.read_sizes.lock().unwrap().iter().all(|&n| n == 1 << 20));
}

#[tokio::test]
async fn waiting_for_device_detected_by_periodic_check() {
    let mut e = env();
    let s = Fake::new("mtp:x", 1);
    s.put_file("/f", b"abc", t(1));
    let d = Fake::new("vol:d", 1);
    add(&e.reg, &d);
    // source not attached at all when submitting
    let id = e.engine.submit(req(vec![s.vp("/f")], d.vp("/"), TransferOp::Copy, ConflictPolicy::Skip));
    loop {
        if let TransferEvent::WaitingForDevice { .. } = e.rx.recv().await.unwrap() {
            break;
        }
    }
    add(&e.reg, &s); // no device_attached call: poll must notice
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs), (TaskOutcome::Done, 0));
    assert_eq!(d.get("/f").unwrap(), b"abc");
}

// ---------------------------------------------------------------- progress / scheduling

#[tokio::test]
async fn progress_events_are_throttled() {
    let mut e = env();
    let s = Fake::new("vol:s", 4);
    let d = Fake::new("vol:d", 4);
    s.put_file("/f", &data(60_000, 1), t(1));
    *s.serve_chunk.lock().unwrap() = 1000;
    *s.delay.lock().unwrap() = Duration::from_millis(10);
    add(&e.reg, &s);
    add(&e.reg, &d);
    let id = e.engine.submit(req(vec![s.vp("/f")], d.vp("/"), TransferOp::Copy, ConflictPolicy::Skip));
    let mut stamps = Vec::new();
    let mut speeds = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), e.rx.recv()).await.unwrap().unwrap() {
            TransferEvent::Progress { bytes_per_sec, current, .. } => {
                stamps.push(Instant::now());
                speeds.push(bytes_per_sec);
                assert_eq!(current.unwrap().name(), "f");
            }
            TransferEvent::Finished { task, .. } if task == id => break,
            _ => {}
        }
    }
    assert!(stamps.len() >= 3, "got {} progress events", stamps.len());
    assert!(stamps.len() < 60, "not throttled: {}", stamps.len());
    for w in stamps.windows(2).take(stamps.len().saturating_sub(2)) {
        assert!(w[1] - w[0] >= Duration::from_millis(90), "{:?}", w[1] - w[0]);
    }
    assert!(speeds.iter().any(|&v| v > 1000.0), "{speeds:?}");
}

async fn max_concurrency(par: usize) -> usize {
    let mut e = env();
    let a = tempfile::tempdir().unwrap();
    for i in 0..8 {
        std::fs::write(a.path().join(format!("f{i}")), data(4000, i as u8)).unwrap();
    }
    e.reg.insert(LocalBackend::volume("A", a.path().into()));
    let d = Fake::new("vol:d", par);
    *d.delay.lock().unwrap() = Duration::from_millis(20);
    add(&e.reg, &d);
    let id = e.engine.submit(req((0..8).map(|i| VPath::from_str_path("vol:A", &format!("/f{i}"))).collect(), d.vp("/"), TransferOp::Copy, ConflictPolicy::Skip));
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs), (TaskOutcome::Done, 0));
    for i in 0..8 {
        assert_eq!(d.get(&format!("/f{i}")).unwrap(), data(4000, i as u8));
    }
    d.max_writers.load(Ordering::SeqCst)
}

#[tokio::test]
async fn respects_max_parallel() {
    assert_eq!(max_concurrency(1).await, 1);
    let m = max_concurrency(4).await;
    assert!(m > 1 && m <= 4, "{m}");
}

#[tokio::test]
async fn device_to_device_goes_through_temp() {
    let mut e = env();
    let s = Fake::new("mtp:android", 1);
    let d = Fake::new("ios:iphone", 1);
    s.put_file("/DCIM/a.jpg", &data(50_000, 5), t(1234));
    s.put_file("/DCIM/b.jpg", &data(10, 6), t(1234));
    add(&e.reg, &s);
    add(&e.reg, &d);
    let stage = e.stage.clone();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    *d.on_create.lock().unwrap() = Some(Box::new(move |_, _| {
        let staged: Vec<u64> = std::fs::read_dir(&stage).unwrap().flatten().map(|e| e.metadata().unwrap().len()).collect();
        seen2.lock().unwrap().push(staged);
    }));
    let id = e.engine.submit(req(vec![s.vp("/DCIM")], d.vp("/Downloads"), TransferOp::Copy, ConflictPolicy::Skip));
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs), (TaskOutcome::Done, 0));
    assert_eq!(d.get("/Downloads/DCIM/a.jpg").unwrap(), data(50_000, 5));
    assert_eq!(d.mtime("/Downloads/DCIM/a.jpg").unwrap(), t(1234));
    // when the destination write started, the full file was already staged locally
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let mut sizes: Vec<u64> = seen.iter().flatten().copied().collect();
    sizes.sort();
    assert_eq!(sizes, vec![10, 50_000]);
    assert_eq!(std::fs::read_dir(&e.stage).unwrap().count(), 0, "staging cleaned up");
    // MTP side read with 4 MiB chunks (min of mtp/local)
    assert!(s.read_sizes.lock().unwrap().iter().all(|&n| n == 4 << 20));
    assert_eq!(last_progress(&evs).1, 50_010);
}

#[tokio::test]
async fn free_space_precheck_fails_early() {
    let mut e = env();
    let s = Fake::new("vol:s", 4);
    let d = Fake::new("vol:d", 4);
    d.caps.lock().unwrap().free_space = true;
    *d.free.lock().unwrap() = Some(100);
    s.put_file("/f", &data(1000, 1), t(1));
    add(&e.reg, &s);
    add(&e.reg, &d);
    let id = e.engine.submit(req(vec![s.vp("/f")], d.vp("/"), TransferOp::Copy, ConflictPolicy::Skip));
    let evs = wait_done(&mut e.rx, id).await;
    match finished(&evs).0 {
        TaskOutcome::Failed(m) => assert!(m.contains("недостаточно места"), "{m}"),
        o => panic!("{o:?}"),
    }
    assert!(s.open_offsets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn copy_onto_itself_is_skipped_unless_rename() {
    let mut e = env();
    let s = Fake::new("vol:s", 4);
    s.put_file("/a/f", b"x", t(1));
    add(&e.reg, &s);
    let id = e.engine.submit(req(vec![s.vp("/a/f")], s.vp("/a"), TransferOp::Copy, ConflictPolicy::Overwrite));
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs), (TaskOutcome::Done, 0));
    assert_eq!(s.get("/a/f").unwrap(), b"x");
    let id = e.engine.submit(req(vec![s.vp("/a")], s.vp("/a/inner"), TransferOp::Copy, ConflictPolicy::Overwrite));
    let evs = wait_done(&mut e.rx, id).await;
    assert_eq!(finished(&evs).1, 1, "folder into itself rejected");
}
