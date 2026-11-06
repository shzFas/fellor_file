//! Who is holding the USB interface? (Best effort, macOS process names.)

/// Programs known to claim MTP/PTP interfaces exclusively.
pub const KNOWN_HOLDERS: &[&str] = &["Android File Transfer Agent", "Android File Transfer", "OpenMTP", "MacDroid"];

/// Hint used when nothing else is found but the interface is a Still Image
/// class interface: macOS' own camera daemon grabs those.
pub const PTPCAMERAD_HINT: &str = "ptpcamerad / Захват изображений";

/// Match `ps -axo comm` output (one executable path per line) against the
/// known list. Returns the first match in [`KNOWN_HOLDERS`] order.
pub fn match_processes(ps_output: &str) -> Option<&'static str> {
    let names: Vec<&str> = ps_output
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.rsplit('/').next().unwrap_or(l))
        .collect();
    KNOWN_HOLDERS.iter().copied().find(|k| names.iter().any(|n| n.eq_ignore_ascii_case(k)))
}

/// Best guess at the program holding the device.
pub async fn detect_holder(still_image_class: bool) -> Option<String> {
    let out = tokio::process::Command::new("/bin/ps").args(["-axo", "comm"]).output().await.ok();
    let found = out.as_ref().and_then(|o| match_processes(&String::from_utf8_lossy(&o.stdout)));
    match found {
        Some(n) => Some(n.to_owned()),
        None if still_image_class => Some(PTPCAMERAD_HINT.to_owned()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_known_programs() {
        let ps = "COMM\n/sbin/launchd\n/Applications/Android File Transfer.app/Contents/Resources/Android File Transfer Agent.app/Contents/MacOS/Android File Transfer Agent\n/usr/libexec/ptpcamerad\n";
        assert_eq!(match_processes(ps), Some("Android File Transfer Agent"));
        assert_eq!(match_processes("/Applications/OpenMTP.app/Contents/MacOS/OpenMTP\n"), Some("OpenMTP"));
        assert_eq!(match_processes("/usr/libexec/ptpcamerad\n/bin/zsh\n"), None);
    }
}
