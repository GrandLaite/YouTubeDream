//! Человекочитаемые длительности, размеры и имена папок.

/// 75 → "1:15", 3725 → "1:02:05", 0 → "".
pub fn duration(sec: u64) -> String {
    if sec == 0 {
        return String::new();
    }
    let (h, m, s) = (sec / 3600, (sec % 3600) / 60, sec % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Размер в двоичных единицах, как их пишет сам yt-dlp: "12.34MiB".
pub fn bytes(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{value:.0}{}", UNITS[0]) } else { format!("{value:.2}{}", UNITS[unit]) }
}

/// Оставшееся время: 47 → "00:47", 3725 → "1:02:05".
pub fn eta(sec: u64) -> String {
    let (h, m, s) = (sec / 3600, (sec % 3600) / 60, sec % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

/// Убирает символы, недопустимые в именах папок Windows.
pub fn sanitize_folder_name(s: &str) -> String {
    let replaced: String = s
        .chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    let trimmed = replaced.trim().trim_matches('.');
    if trimmed.is_empty() { "channel".to_string() } else { trimmed.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(0), "");
        assert_eq!(duration(75), "1:15");
        assert_eq!(duration(3725), "1:02:05");
        assert_eq!(eta(47), "00:47");
        assert_eq!(eta(3725), "1:02:05");
    }

    #[test]
    fn byte_sizes() {
        assert_eq!(bytes(512.0), "512B");
        assert_eq!(bytes(433081.0), "422.93KiB");
        assert_eq!(bytes(5.0 * 1024.0 * 1024.0), "5.00MiB");
    }

    #[test]
    fn folder_names() {
        assert_eq!(sanitize_folder_name("  AC/DC: Live?  "), "AC_DC_ Live_");
        assert_eq!(sanitize_folder_name("..."), "channel");
        assert_eq!(sanitize_folder_name("Канал"), "Канал");
    }
}
