//! Human-readable formatting helpers (Russian locale).

use std::time::{Duration, SystemTime};

/// Decimal (Finder-style) human size: "3,1 МБ".
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["КБ", "МБ", "ГБ", "ТБ", "ПБ"];
    if bytes < 1000 {
        return format!("{bytes} Б");
    }
    let mut v = bytes as f64 / 1000.0;
    let mut unit = 0;
    while v >= 999.95 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    let s = if v >= 100.0 { format!("{v:.0}") } else { format!("{v:.1}") };
    format!("{} {}", s.replace('.', ","), UNITS[unit])
}

pub fn speed(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "—".into();
    }
    format!("{}/с", size(bytes_per_sec as u64))
}

/// "1:40", "1:02:03".
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn eta(remaining_bytes: u64, bytes_per_sec: f64) -> Option<String> {
    if bytes_per_sec <= 1.0 || remaining_bytes == 0 {
        return None;
    }
    Some(duration(Duration::from_secs_f64(remaining_bytes as f64 / bytes_per_sec)))
}

pub fn date(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%d.%m.%Y %H:%M").to_string()
        }
        None => "—".into(),
    }
}

/// Russian plural: `plural(n, "файл", "файла", "файлов")`.
pub fn plural<'a>(n: u64, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    let n10 = n % 10;
    let n100 = n % 100;
    if n10 == 1 && n100 != 11 {
        one
    } else if (2..=4).contains(&n10) && !(12..=14).contains(&n100) {
        few
    } else {
        many
    }
}

pub fn files(n: u64) -> String {
    format!("{n} {}", plural(n, "файл", "файла", "файлов"))
}

pub fn folders(n: u64) -> String {
    format!("{n} {}", plural(n, "папка", "папки", "папок"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size(512), "512 Б");
        assert_eq!(size(3_100_000), "3,1 МБ");
        assert_eq!(size(520_000_000_000), "520 ГБ");
        assert_eq!(size(999_990), "1,0 МБ");
    }

    #[test]
    fn plurals() {
        assert_eq!(files(1), "1 файл");
        assert_eq!(files(142), "142 файла");
        assert_eq!(files(11), "11 файлов");
        assert_eq!(files(38), "38 файлов");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(Duration::from_secs(100)), "1:40");
        assert_eq!(duration(Duration::from_secs(3723)), "1:02:03");
    }
}
