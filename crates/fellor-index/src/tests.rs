use super::*;
use async_trait::async_trait;
use bytes::Bytes;
use std::time::Duration;

/// Local dir pretending to be a device whose roots are its top-level folders
/// (like iOS "Media"/app dirs or MTP storages).
struct Dev {
    inner: BackendRef,
}

#[async_trait]
impl Backend for Dev {
    fn id(&self) -> BackendId {
        self.inner.id()
    }
    fn info(&self) -> DeviceInfo {
        self.inner.info()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn roots(&self) -> Result<Vec<Entry>> {
        self.inner.list(&VPath::root(self.id())).await
    }
    async fn list(&self, p: &VPath) -> Result<Vec<Entry>> {
        self.inner.list(p).await
    }
    async fn stat(&self, p: &VPath) -> Result<Entry> {
        self.inner.stat(p).await
    }
    async fn free_space(&self, p: &VPath) -> Result<Option<SpaceInfo>> {
        self.inner.free_space(p).await
    }
    async fn open_read(&self, p: &VPath, o: u64) -> Result<Box<dyn ReadSource>> {
        self.inner.open_read(p, o).await
    }
    async fn create_write(&self, p: &VPath, s: u64, m: Option<SystemTime>) -> Result<Box<dyn WriteSink>> {
        self.inner.create_write(p, s, m).await
    }
    async fn mkdir(&self, p: &VPath) -> Result<Entry> {
        self.inner.mkdir(p).await
    }
    async fn remove(&self, p: &VPath, r: bool) -> Result<()> {
        self.inner.remove(p, r).await
    }
    async fn rename(&self, a: &VPath, b: &VPath) -> Result<Entry> {
        self.inner.rename(a, b).await
    }
    async fn set_mtime(&self, p: &VPath, m: SystemTime) -> Result<()> {
        self.inner.set_mtime(p, m).await
    }
    async fn thumbnail(&self, _p: &VPath) -> Result<Option<Bytes>> {
        Ok(None)
    }
}

fn touch(root: &Path, rel: &str, secs: u64) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, rel.as_bytes()).unwrap();
    std::fs::File::options().write(true).open(&p).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

fn names(plan: &ImportPlan) -> Vec<String> {
    plan.new_files.iter().map(|e| e.path.as_unix()).collect()
}

#[test]
fn index_roundtrip_on_disk() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("nested/dir/index.db");
    let t = Some(UNIX_EPOCH + Duration::from_secs(1000));
    {
        let ix = ImportIndex::open(&path).unwrap();
        assert!(!ix.is_imported("ios:a", "/DCIM/x.jpg", 10, t).unwrap());
        ix.mark_imported("ios:a", "/DCIM/x.jpg", 10, t, "/Users/me/Pictures/x.jpg").unwrap();
        ix.mark_imported("ios:a", "/DCIM/x.jpg", 10, t, "/again").unwrap(); // idempotent
        assert_eq!(ix.count("ios:a").unwrap(), 1);
    }
    let ix = ImportIndex::open(&path).unwrap();
    assert!(ix.is_imported("ios:a", "/DCIM/x.jpg", 10, t).unwrap());
    // sub-second differences are ignored
    assert!(ix.is_imported("ios:a", "/DCIM/x.jpg", 10, t.map(|t| t + Duration::from_millis(300))).unwrap());
    assert!(!ix.is_imported("ios:a", "/DCIM/x.jpg", 11, t).unwrap());
    assert!(!ix.is_imported("ios:b", "/DCIM/x.jpg", 10, t).unwrap());
    assert!(!ix.is_imported("ios:a", "/DCIM/x.jpg", 10, None).unwrap());
    assert_eq!(ix.forget_device("ios:a").unwrap(), 1);
    assert_eq!(ix.count("ios:a").unwrap(), 0);
}

#[test]
fn default_path_is_app_support() {
    let p = ImportIndex::default_path();
    assert!(p.ends_with("Fellor/index.db"), "{p:?}");
    assert!(p.parent().unwrap().exists());
}

#[tokio::test]
async fn plan_volume_dcim() {
    let d = tempfile::tempdir().unwrap();
    touch(d.path(), "DCIM/100CANON/IMG_2.JPG", 2000);
    touch(d.path(), "DCIM/100CANON/IMG_1.CR2.MOV", 1000);
    touch(d.path(), "DCIM/100CANON/notes.txt", 500);
    touch(d.path(), "DCIM/.thumbs/t.jpg", 500);
    touch(d.path(), "Other/outside.jpg", 500);
    let be = fellor_local::LocalBackend::volume("SD", d.path().into());
    let db = tempfile::tempdir().unwrap();
    let ix = ImportIndex::open(&db.path().join("i.db")).unwrap();
    let plan = plan_import(be.clone(), &ix).await.unwrap();
    assert_eq!(names(&plan), vec!["/DCIM/100CANON/IMG_1.CR2.MOV", "/DCIM/100CANON/IMG_2.JPG"], "sorted by mtime");
    assert_eq!(plan.already_imported, 0);
    assert_eq!(plan.total_bytes, plan.new_files.iter().map(|e| e.size.unwrap()).sum::<u64>());
    ix.mark_entry_imported(&plan.new_files[0], "/x").unwrap();
    let plan2 = plan_import(be, &ix).await.unwrap();
    assert_eq!(names(&plan2), vec!["/DCIM/100CANON/IMG_2.JPG"]);
    assert_eq!(plan2.already_imported, 1);
}

#[tokio::test]
async fn plan_ios_media_dcim_and_fallback() {
    let d = tempfile::tempdir().unwrap();
    touch(d.path(), "Media/DCIM/100APPLE/IMG_0001.HEIC", 10);
    touch(d.path(), "Media/DCIM/100APPLE/IMG_0001.MOV", 10);
    touch(d.path(), "VLC/Documents/movie.mp4", 10); // app dir: ignored
    let be: BackendRef = Arc::new(Dev { inner: fellor_local::LocalBackend::rooted("ios:abc", d.path().into()) });
    let ix = ImportIndex::open_in_memory().unwrap();
    let plan = plan_import(be, &ix).await.unwrap();
    assert_eq!(names(&plan), vec!["/Media/DCIM/100APPLE/IMG_0001.HEIC", "/Media/DCIM/100APPLE/IMG_0001.MOV"]);

    // Layout without a "Media" root: fallback finds DCIM at depth ≤ 2.
    let d2 = tempfile::tempdir().unwrap();
    touch(d2.path(), "Phone/DCIM/a.jpg", 10);
    let be: BackendRef = Arc::new(Dev { inner: fellor_local::LocalBackend::rooted("ios:def", d2.path().into()) });
    let plan = plan_import(be, &ix).await.unwrap();
    assert_eq!(names(&plan), vec!["/Phone/DCIM/a.jpg"]);
}

#[tokio::test]
async fn plan_mtp_every_storage() {
    let d = tempfile::tempdir().unwrap();
    touch(d.path(), "Internal shared storage/DCIM/Camera/PXL_1.jpg", 30);
    touch(d.path(), "SD card/DCIM/Camera/PXL_0.mp4", 20);
    touch(d.path(), "Internal shared storage/Download/x.jpg", 10);
    let be: BackendRef = Arc::new(Dev { inner: fellor_local::LocalBackend::rooted("mtp:serial", d.path().into()) });
    let plan = plan_import(be, &ImportIndex::open_in_memory().unwrap()).await.unwrap();
    assert_eq!(names(&plan), vec!["/SD card/DCIM/Camera/PXL_0.mp4", "/Internal shared storage/DCIM/Camera/PXL_1.jpg"]);
}

fn entry_at(secs: Option<u64>) -> Entry {
    Entry {
        name: "a.jpg".into(),
        path: VPath::from_str_path("ios:x", "/DCIM/a.jpg"),
        kind: EntryKind::File,
        size: Some(1),
        mtime: secs.map(|s| UNIX_EPOCH + Duration::from_secs(s)),
        ctime: None,
        mime: Some("image/jpeg"),
        hidden: false,
        native: NativeId::None,
    }
}

#[test]
fn templates() {
    // 2026-11-07 12:00 UTC is 2026-11-07 in every timezone from UTC-11 to UTC+11.
    let e = entry_at(Some(1_794_052_800));
    assert_eq!(dest_subdir("{date}", &e), "2026-11-07");
    assert_eq!(dest_subdir("{year}/{month}", &e), "2026/11");
    assert_eq!(dest_subdir("{device}/{date}", &e), "2026-11-07");
    assert_eq!(dest_subdir_with_device("{device}/{year}-{month}-{day}", &e, "iPhone: Алекс/15"), "iPhone- Алекс-15/2026-11-07");
    assert_eq!(dest_subdir("", &e), "");
    assert_eq!(dest_subdir("Imports/../{year}", &e), "Imports/2026");
    assert_eq!(dest_subdir("{date}", &entry_at(None)), "unknown");
}

#[test]
fn grouping_keeps_order() {
    let mk = |name: &str, secs: u64| Entry { name: name.into(), path: VPath::from_str_path("vol:x", &format!("/DCIM/{name}")), ..entry_at(Some(secs)) };
    let day = 86_400;
    let base = 1_794_052_800;
    let plan = ImportPlan { new_files: vec![mk("a", base), mk("b", base + day), mk("c", base + 60)], total_bytes: 3, already_imported: 0 };
    let g = group_by_subdir(&plan, "{date}");
    assert_eq!(g.len(), 2);
    assert_eq!(g[0].0, "2026-11-07");
    assert_eq!(g[0].1.iter().map(|p| p.name()).collect::<Vec<_>>(), vec!["a", "c"]);
    assert_eq!(g[1].0, "2026-11-08");
}
