//! `fellor doctor` — environment diagnostics.
use fellor_vfs::{BackendRegistry, DeviceState};

const MTP_GRABBERS: [&str; 4] = ["Android File Transfer", "OpenMTP", "MacDroid", "Android File Transfer Agent"];

fn ok(msg: &str) {
    println!("  [ok]  {msg}");
}
fn warn(msg: &str) {
    println!("  [!!]  {msg}");
}

pub fn state_hint(s: &DeviceState) -> (&'static str, String) {
    match s {
        DeviceState::Ready => ("готово", String::new()),
        DeviceState::NeedsTrust => ("нужно доверие", "разблокируйте устройство и нажмите «Доверять этому компьютеру»".into()),
        DeviceState::Locked => ("заблокировано", "разблокируйте экран устройства".into()),
        DeviceState::NeedsMtpMode => ("режим зарядки", "на телефоне выберите USB-режим «Передача файлов»".into()),
        DeviceState::Busy { by } => (
            "занято",
            format!("устройство занято{} — закройте эту программу", by.as_ref().map(|b| format!(" программой «{b}»")).unwrap_or_default()),
        ),
        DeviceState::Connecting => ("подключение", "подождите несколько секунд".into()),
        DeviceState::Error(e) => ("ошибка", e.clone()),
    }
}

/// Names of running processes known to grab MTP devices.
pub fn running_mtp_grabbers() -> Vec<String> {
    let Ok(out) = std::process::Command::new("ps").args(["-axo", "comm="]).output() else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut found: Vec<String> = MTP_GRABBERS
        .iter()
        .filter(|name| text.lines().any(|l| l.rsplit('/').next().is_some_and(|base| base.trim() == **name) || l.contains(&format!("{name}.app"))))
        .map(|s| s.to_string())
        .collect();
    found.dedup();
    found
}

pub async fn run(reg: &BackendRegistry) -> bool {
    let mut healthy = true;
    println!("usbmuxd (iPhone/iPad):");
    match std::os::unix::net::UnixStream::connect("/var/run/usbmuxd") {
        Ok(_) => ok("/var/run/usbmuxd доступен"),
        Err(e) => {
            healthy = false;
            warn(&format!("/var/run/usbmuxd недоступен: {e} — iPhone не будут видны (перезапустите Mac или `sudo launchctl kickstart -k system/com.apple.usbmuxd`)"));
        }
    }

    println!("Устройства:");
    let mut all = reg.all();
    all.sort_by_key(|b| b.id());
    for b in &all {
        let i = b.info();
        let (state, hint) = state_hint(&i.state);
        let line = format!("{:<28} {:<24} {:?}, {state}", i.id, i.name, i.kind);
        if hint.is_empty() {
            ok(&line);
        } else {
            warn(&format!("{line} — {hint}"));
        }
    }
    if !all.iter().any(|b| b.id().starts_with("ios:") || b.id().starts_with("mtp:")) {
        println!("        (телефонов не найдено — проверьте кабель; кабель только для зарядки не передаёт данные)");
    }

    println!("Программы, перехватывающие Android-устройства:");
    let grabbers = running_mtp_grabbers();
    if grabbers.is_empty() {
        ok("не запущены");
    } else {
        for g in grabbers {
            warn(&format!("запущена «{g}» — закройте её, иначе Fellor не получит доступ к телефону"));
        }
    }

    println!("Доступ к папкам:");
    if let Some(pics) = dirs::picture_dir() {
        let probe = pics.join(format!(".fellor-doctor-{}", std::process::id()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                ok(&format!("запись в {} разрешена", pics.display()));
            }
            Err(e) => {
                healthy = false;
                warn(&format!("нет записи в {}: {}", pics.display(), fellor_local::map_io(e, &pics)));
            }
        }
    }
    let ix = fellor_index::ImportIndex::default_path();
    match fellor_index::ImportIndex::open(&ix) {
        Ok(_) => ok(&format!("индекс импорта: {}", ix.display())),
        Err(e) => {
            healthy = false;
            warn(&format!("индекс импорта {}: {e}", ix.display()));
        }
    }
    healthy
}
