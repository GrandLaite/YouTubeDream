//! Настройки, переживающие перезапуск: папка загрузок и браузер для cookies.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const APP_DIR: &str = "Lader";
/// Прежнее имя приложения: из этих папок переносим настройки и компоненты.
const LEGACY_APP_DIR: &str = "YouTubeDream";

/// Браузеры для `--cookies-from-browser`: подпись в UI и значение для yt-dlp.
/// Пустое значение — cookies не используются.
pub const COOKIE_BROWSERS: [(&str, &str); 8] = [
    ("Без cookies", ""),
    ("Chrome", "chrome"),
    ("Edge", "edge"),
    ("Firefox", "firefox"),
    ("Brave", "brave"),
    ("Opera", "opera"),
    ("Vivaldi", "vivaldi"),
    ("Chromium", "chromium"),
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Корневая папка загрузок (внутри неё создаются папки каналов).
    pub out_dir: String,
    /// Значение для `--cookies-from-browser`; пустая строка — без cookies.
    pub cookies_browser: String,
    /// Во сколько соединений качать файл (1 — без многопоточности).
    pub connections: usize,
}

/// Варианты числа потоков в интерфейсе. Больше 16 не даём: YouTube может
/// начать отвечать отказами на слишком много соединений с одного адреса.
pub const CONNECTION_CHOICES: [usize; 5] = [1, 2, 4, 8, 16];
pub const DEFAULT_CONNECTIONS: usize = 8;

impl Default for Settings {
    fn default() -> Self {
        let downloads = dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            out_dir: downloads.to_string_lossy().into_owned(),
            cookies_browser: String::new(),
            connections: DEFAULT_CONNECTIONS,
        }
    }
}

/// Папка настроек: %APPDATA%\Lader.
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(APP_DIR))
}

/// Папка данных: %LOCALAPPDATA%\Lader (здесь живут свои копии компонентов).
pub fn data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join(APP_DIR))
}

/// Приложение раньше называлось YouTubeDream. Если папок с новым именем ещё
/// нет, переименовываем старые целиком — так сохраняются и настройки, и уже
/// скачанные компоненты. На macOS обе папки совпадают (Application Support).
pub fn migrate_legacy_dirs() {
    for base in [dirs::config_dir(), dirs::data_local_dir()].into_iter().flatten() {
        let (old, new) = (base.join(LEGACY_APP_DIR), base.join(APP_DIR));
        if old.is_dir() && !new.exists() {
            let _ = fs::rename(&old, &new);
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("settings.json"))
}

impl Settings {
    /// Читает настройки. При первом запуске переносит их из Fyne-версии
    /// приложения, чтобы не потерять выбранную папку.
    pub fn load() -> Self {
        if let Some(path) = settings_path()
            && let Ok(data) = fs::read(&path)
        {
            return serde_json::from_slice(&data).unwrap_or_default();
        }
        let migrated = migrate_from_fyne();
        let settings = migrated.unwrap_or_default();
        let _ = settings.save();
        settings
    }

    pub fn save(&self) -> Result<(), String> {
        let path = settings_path().ok_or("не удалось определить папку настроек")?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let data = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        // Пишем во временный файл и переименовываем: при падении посреди
        // записи старые настройки останутся целыми.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, data).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }
}

/// Fyne хранил настройки в %APPDATA%\fyne\com.ytdl.app\preferences.json:
/// {"outDir": "D:/Downloader/", "cookiesBrowser": "Chrome"}.
fn migrate_from_fyne() -> Option<Settings> {
    #[derive(Deserialize)]
    struct FynePrefs {
        #[serde(rename = "outDir")]
        out_dir: Option<String>,
        #[serde(rename = "cookiesBrowser")]
        cookies_browser: Option<String>,
    }
    let path = dirs::config_dir()?.join("fyne").join("com.ytdl.app").join("preferences.json");
    let prefs: FynePrefs = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let mut settings = Settings::default();
    if let Some(dir) = prefs.out_dir.filter(|d| !d.trim().is_empty()) {
        settings.out_dir = normalize_dir(&dir);
    }
    if let Some(label) = prefs.cookies_browser {
        settings.cookies_browser = COOKIE_BROWSERS
            .iter()
            .find(|(l, _)| *l == label)
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
    }
    Some(settings)
}

/// Приводит путь к каноничному для ОС виду: на Windows — обратные слэши
/// и без хвостового разделителя. Explorer особенно придирчив: с прямыми
/// слэшами или относительным путём он молча открывает «Документы».
pub fn normalize_dir(dir: &str) -> String {
    let mut s = dir.trim().to_string();
    if cfg!(windows) {
        s = s.replace('/', "\\");
    }
    let sep = std::path::MAIN_SEPARATOR;
    // "D:\" — корень диска, его хвостовой слэш значимый.
    while s.len() > 1 && s.ends_with(sep) && !(cfg!(windows) && s.len() == 3 && s.as_bytes()[1] == b':') {
        s.pop();
    }
    let path = Path::new(&s);
    if path.is_relative()
        && !s.is_empty()
        && let Ok(cwd) = std::env::current_dir()
    {
        return cwd.join(path).to_string_lossy().into_owned();
    }
    s
}

/// Подпись браузера в UI по значению для yt-dlp.
pub fn browser_index(value: &str) -> usize {
    COOKIE_BROWSERS.iter().position(|(_, v)| *v == value).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn normalizes_windows_dirs() {
        assert_eq!(normalize_dir("D:/Downloader/"), "D:\\Downloader");
        assert_eq!(normalize_dir("D:\\"), "D:\\");
        assert_eq!(normalize_dir(" C:/Users/x/Videos "), "C:\\Users\\x\\Videos");
    }

    #[test]
    fn browser_lookup() {
        assert_eq!(browser_index(""), 0);
        assert_eq!(browser_index("firefox"), 3);
        assert_eq!(browser_index("unknown"), 0);
    }
}
