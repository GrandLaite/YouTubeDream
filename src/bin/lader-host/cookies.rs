//! Cookies для yt-dlp.
//!
//! `--cookies-from-browser edge|chrome` с 2024 года не работает: Chromium
//! шифрует базу cookies ключом, привязанным к самому браузеру (App-Bound
//! Encryption), и сторонняя программа его не получит. Расширение же
//! получает cookies у браузера штатно (chrome.cookies) и передаёт их сюда;
//! помощник кладёт их во временный файл формата Netscape для `--cookies`
//! и удаляет его, как только yt-dlp закончил.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use lader_core::settings::{self, COOKIE_BROWSERS};
use lader_core::ytdlp::Cookies;
use serde::Deserialize;
use serde_json::Value;

/// Cookie в том виде, в каком его отдаёт `chrome.cookies.getAll`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserCookie {
    pub name: String,
    #[serde(default)]
    pub value: String,
    pub domain: String,
    #[serde(default = "root_path")]
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default)]
    pub host_only: bool,
    /// Секунды Unix; у сессионных cookies отсутствует.
    #[serde(default)]
    pub expiration_date: Option<f64>,
}

fn root_path() -> String {
    "/".into()
}

/// Что выбрал пользователь в расширении.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Spec {
    #[default]
    None,
    /// `--cookies-from-browser`: для браузеров, где это ещё работает (Firefox).
    Browser(String),
    /// Cookies, которые отдал сам браузер с расширением.
    List(Vec<BrowserCookie>),
}

impl Spec {
    /// `{"mode": "none"}`, `{"mode": "browser", "browser": "firefox"}`
    /// или `{"mode": "list", "list": [...]}`. Без поля cookies не используются.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        match v["mode"].as_str().unwrap_or("none") {
            "none" => Ok(Spec::None),
            "browser" => {
                let browser = v["browser"].as_str().unwrap_or("");
                // Значение уходит в аргументы yt-dlp, поэтому пропускаем только известные.
                if browser.is_empty() || !COOKIE_BROWSERS.iter().any(|(_, b)| *b == browser) {
                    return Err(format!("неизвестный браузер для cookies: {browser}"));
                }
                Ok(Spec::Browser(browser.to_string()))
            }
            "list" => {
                let list: Vec<BrowserCookie> =
                    serde_json::from_value(v["list"].clone()).map_err(|e| format!("некорректные cookies: {e}"))?;
                Ok(if list.is_empty() { Spec::None } else { Spec::List(list) })
            }
            other => Err(format!("неизвестный режим cookies: {other}")),
        }
    }

    /// Аргумент для yt-dlp. Файл cookies живёт, пока жив возвращённый `CookieFile`.
    pub fn resolve(&self) -> Result<(Cookies, Option<CookieFile>), String> {
        match self {
            Spec::None => Ok((Cookies::None, None)),
            Spec::Browser(browser) => Ok((Cookies::Browser(browser.clone()), None)),
            Spec::List(list) => {
                let file = CookieFile::write(list)?;
                Ok((Cookies::File(file.path.clone()), Some(file)))
            }
        }
    }
}

/// Папка помощника: `%LOCALAPPDATA%\Lader\host`.
pub fn host_dir() -> Option<PathBuf> {
    settings::data_dir().map(|d| d.join("host"))
}

/// Временный файл cookies; удаляется вместе с этим значением.
pub struct CookieFile {
    path: PathBuf,
}

impl CookieFile {
    fn write(list: &[BrowserCookie]) -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = host_dir().ok_or("не удалось определить папку данных")?;
        fs::create_dir_all(&dir).map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
        let name = format!("cookies-{}-{}.txt", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let path = dir.join(name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|e| format!("не удалось записать cookies: {e}"))?;
        // Сначала создаём guard: при ошибке записи файл всё равно удалится.
        let guard = Self { path };
        file.write_all(netscape(list).as_bytes()).map_err(|e| format!("не удалось записать cookies: {e}"))?;
        Ok(guard)
    }
}

impl Drop for CookieFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Файлы cookies, оставшиеся от помощника, который не успел их убрать
/// (браузер убил процесс). Свежие не трогаем, ими может пользоваться
/// помощник другого браузера, запущенный параллельно.
pub fn remove_stale() {
    let Some(dir) = host_dir() else { return };
    let Ok(entries) = fs::read_dir(dir) else { return };
    let day = Duration::from_secs(24 * 3600);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > day);
        if name.starts_with("cookies-") && name.ends_with(".txt") && old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Файл cookies в формате Netscape, который понимает yt-dlp (`--cookies`).
pub fn netscape(list: &[BrowserCookie]) -> String {
    let clean = |s: &str| s.chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect::<String>();
    let mut out = String::from("# Netscape HTTP Cookie File\n# Временный файл Lader: удаляется после загрузки.\n\n");
    for c in list {
        let mut domain = clean(&c.domain);
        if domain.is_empty() || c.name.is_empty() {
            continue;
        }
        // У cookie для поддоменов домен начинается с точки, у cookie
        // одного хоста без неё. yt-dlp сверяет флаг с точкой.
        if !c.host_only && !domain.starts_with('.') {
            domain.insert(0, '.');
        }
        let subdomains = if domain.starts_with('.') { "TRUE" } else { "FALSE" };
        let prefix = if c.http_only { "#HttpOnly_" } else { "" };
        let expires = c.expiration_date.filter(|e| e.is_finite() && *e > 0.0).map_or(0, |e| e as u64);
        out.push_str(&format!(
            "{prefix}{domain}\t{subdomains}\t{}\t{}\t{expires}\t{}\t{}\n",
            clean(&c.path),
            if c.secure { "TRUE" } else { "FALSE" },
            clean(&c.name),
            clean(&c.value),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn writes_netscape_lines() {
        let list: Vec<BrowserCookie> = serde_json::from_value(json!([
            { "name": "SID", "value": "abc", "domain": ".youtube.com", "path": "/", "secure": true,
              "httpOnly": true, "hostOnly": false, "expirationDate": 1900000000.5, "session": false },
            { "name": "PREF", "value": "f6=8", "domain": "www.youtube.com", "hostOnly": true, "session": true },
            { "name": "ct0", "value": "x\ty", "domain": "x.com", "hostOnly": false, "expirationDate": 1800000000 }
        ]))
        .unwrap();
        let text = netscape(&list);
        let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty() && !l.starts_with("# ")).collect();
        assert_eq!(lines[0], "#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1900000000\tSID\tabc");
        assert_eq!(lines[1], "www.youtube.com\tFALSE\t/\tFALSE\t0\tPREF\tf6=8");
        assert_eq!(lines[2], ".x.com\tTRUE\t/\tFALSE\t1800000000\tct0\txy");
    }

    #[test]
    fn parses_specs() {
        assert_eq!(Spec::from_json(&json!(null)).unwrap(), Spec::None);
        assert_eq!(
            Spec::from_json(&json!({ "mode": "browser", "browser": "firefox" })).unwrap(),
            Spec::Browser("firefox".into())
        );
        assert!(Spec::from_json(&json!({ "mode": "browser", "browser": "firefox --exec rm" })).is_err());
        assert_eq!(Spec::from_json(&json!({ "mode": "list", "list": [] })).unwrap(), Spec::None);
        let spec =
            Spec::from_json(&json!({ "mode": "list", "list": [{ "name": "a", "value": "b", "domain": ".x.com" }] }))
                .unwrap();
        assert!(matches!(spec, Spec::List(ref l) if l.len() == 1 && l[0].path == "/"));
    }

    #[test]
    fn cookie_file_is_removed_on_drop() {
        let list = vec![BrowserCookie {
            name: "a".into(),
            value: "b".into(),
            domain: ".x.com".into(),
            path: "/".into(),
            secure: false,
            http_only: false,
            host_only: false,
            expiration_date: None,
        }];
        let (cookies, guard) = Spec::List(list).resolve().unwrap();
        let Cookies::File(path) = cookies else { panic!("ожидался файл") };
        assert!(fs::read_to_string(&path).unwrap().contains(".x.com\tTRUE"));
        drop(guard);
        assert!(!path.exists());
    }
}
