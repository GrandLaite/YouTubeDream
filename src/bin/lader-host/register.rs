//! Регистрация помощника в браузерах.
//!
//! Браузер находит помощник по манифесту `io.github.grandlaite.lader.json`:
//! на Windows путь к нему лежит в реестре (HKCU, прав администратора не
//! нужно), на macOS и Linux манифест кладётся в папку профиля браузера.
//! Сам исполняемый файл копируется в `%LOCALAPPDATA%\Lader\host`, чтобы
//! скачанный файл можно было удалить или переместить.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::cookies::host_dir;

pub const HOST_NAME: &str = "io.github.grandlaite.lader";
/// ID расширения из папки `extension`: он задаётся ключом `key` в его
/// manifest.json и не зависит от того, откуда расширение загружено.
pub const EXTENSION_ID: &str = "kimlopaoiianjkblnjfakfjjilaojkjc";

const USAGE: &str = "\
lader-host, помощник расширения Lader для Microsoft Edge и Chrome.

Браузер запускает его сам. Вручную нужно только зарегистрировать его:

  lader-host                      зарегистрировать (то же, что --install)
  lader-host --install [--extension-id ID]...
                                  зарегистрировать. ID нужен для расширения, установленного
                                  из магазина или из другой папки (можно несколько)
  lader-host --uninstall          снять регистрацию
";

/// Запуск из командной строки или двойным щелчком. Возвращает код выхода.
pub fn cli(args: &[String]) -> i32 {
    let interactive = args.is_empty();
    let mut extra = Vec::new();
    let mut action = "install";
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--install" => action = "install",
            "--uninstall" => action = "uninstall",
            "--extension-id" => {
                i += 1;
                match args.get(i).filter(|id| is_extension_id(id)) {
                    Some(id) => extra.push(id.clone()),
                    None => {
                        eprintln!("После --extension-id нужен ID расширения: 32 латинские буквы a–p.");
                        return 2;
                    }
                }
            }
            "-h" | "--help" | "/?" => {
                print!("{USAGE}");
                return 0;
            }
            other => {
                eprintln!("Неизвестный параметр: {other}\n\n{USAGE}");
                return 2;
            }
        }
        i += 1;
    }

    let result = if action == "install" { install(&extra) } else { uninstall() };
    let code = match result {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(e) => {
            eprintln!("Ошибка: {e}");
            1
        }
    };
    if interactive {
        // Двойной щелчок в Проводнике: окно консоли не должно закрыться
        // раньше, чем человек прочитает результат.
        println!("\nНажмите Enter, чтобы закрыть окно.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    code
}

/// ID расширения Chromium состоит из 32 букв от a до p.
pub fn is_extension_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

fn origin(id: &str) -> String {
    format!("chrome-extension://{id}/")
}

/// Регистрирует помощник для текущего пользователя.
pub fn install(extra_ids: &[String]) -> Result<String, String> {
    let dir = host_dir().ok_or("не удалось определить папку данных пользователя")?;
    fs::create_dir_all(&dir).map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
    let exe = copy_self(&dir)?;
    let manifest_path = dir.join(format!("{HOST_NAME}.json"));

    // Ранее добавленные ID (расширение из магазина) не теряем при обновлении.
    let mut origins = vec![origin(EXTENSION_ID)];
    let previous = fs::read(&manifest_path).ok().and_then(|d| serde_json::from_slice::<Value>(&d).ok());
    for o in previous.iter().flat_map(|m| m["allowed_origins"].as_array().cloned().unwrap_or_default()) {
        if let Some(o) = o.as_str().filter(|o| o.starts_with("chrome-extension://")) {
            origins.push(o.to_string());
        }
    }
    origins.extend(extra_ids.iter().map(|id| origin(id)));
    let mut seen = std::collections::HashSet::new();
    origins.retain(|o| seen.insert(o.clone()));

    let manifest = manifest_json(&exe, &origins);
    let data = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    fs::write(&manifest_path, &data).map_err(|e| format!("не удалось записать {}: {e}", manifest_path.display()))?;

    let places = register_manifest(&manifest_path, &data)?;
    let mut text = format!("Помощник Lader установлен: {}\n\nЗарегистрирован в:\n", exe.display());
    for p in &places {
        text.push_str(&format!("  {p}\n"));
    }
    text.push_str("\nРазрешённые расширения:\n");
    for o in &origins {
        text.push_str(&format!("  {o}\n"));
    }
    text.push_str("\nЕсли расширение уже открыто, нажмите в нём «Проверить снова».");
    Ok(text)
}

pub fn manifest_json(exe: &Path, origins: &[String]) -> Value {
    json!({
        "name": HOST_NAME,
        "description": "Lader: скачивание видео через yt-dlp для браузерного расширения",
        "path": exe.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": origins,
    })
}

/// Снимает регистрацию. Файлы в `%LOCALAPPDATA%\Lader\host` остаются:
/// помощник может быть запущен прямо сейчас.
pub fn uninstall() -> Result<String, String> {
    let removed = unregister_manifest();
    if let Some(dir) = host_dir() {
        let _ = fs::remove_file(dir.join(format!("{HOST_NAME}.json")));
    }
    Ok(if removed.is_empty() {
        "Помощник не был зарегистрирован.".into()
    } else {
        format!("Регистрация снята:\n  {}", removed.join("\n  "))
    })
}

fn exe_file_name() -> String {
    lader_core::process::exe_name("lader-host")
}

/// Копирует себя в папку помощника. Работающий exe на Windows нельзя
/// перезаписать или удалить, но можно переименовать. Так обновление
/// проходит, даже если браузер прямо сейчас держит старую копию.
fn copy_self(dir: &Path) -> Result<PathBuf, String> {
    let current = std::env::current_exe().map_err(|e| format!("не удалось определить путь к себе: {e}"))?;
    let target = dir.join(exe_file_name());
    if same_file(&current, &target) {
        return Ok(target);
    }
    let staged = dir.join(format!("{}.new", exe_file_name()));
    fs::copy(&current, &staged).map_err(|e| format!("не удалось скопировать в {}: {e}", dir.display()))?;
    if target.exists() && fs::remove_file(&target).is_err() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let old = dir.join(format!("lader-host-old-{stamp}{}", std::env::consts::EXE_SUFFIX));
        fs::rename(&target, &old).map_err(|e| format!("не удалось заменить {}: {e}", target.display()))?;
    }
    fs::rename(&staged, &target).map_err(|e| format!("не удалось заменить {}: {e}", target.display()))?;
    Ok(target)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Убирает копии, переименованные при обновлении (см. `copy_self`).
pub fn remove_old_copies() {
    let Some(dir) = host_dir() else { return };
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("lader-host-old-") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

// ─── Windows: реестр ─────────────────────────────────────────────────────────

#[cfg(windows)]
const REGISTRY_KEYS: [(&str, &str); 3] = [
    ("Microsoft Edge", r"Software\Microsoft\Edge\NativeMessagingHosts"),
    ("Google Chrome", r"Software\Google\Chrome\NativeMessagingHosts"),
    ("Chromium", r"Software\Chromium\NativeMessagingHosts"),
];

#[cfg(windows)]
fn reg(args: &[&str]) -> Result<(), String> {
    let out = lader_core::process::command("reg")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("не удалось запустить reg.exe: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(lader_core::process::decode(&out.stderr).trim().to_string()) }
}

#[cfg(windows)]
fn register_manifest(manifest: &Path, _data: &[u8]) -> Result<Vec<String>, String> {
    let path = manifest.to_string_lossy().into_owned();
    let mut places = Vec::new();
    for (browser, key) in REGISTRY_KEYS {
        let full = format!(r"HKCU\{key}\{HOST_NAME}");
        reg(&["add", &full, "/ve", "/t", "REG_SZ", "/d", &path, "/f"]).map_err(|e| format!("{full}: {e}"))?;
        places.push(format!("{browser}: {full}"));
    }
    Ok(places)
}

#[cfg(windows)]
fn unregister_manifest() -> Vec<String> {
    REGISTRY_KEYS
        .iter()
        .filter_map(|(browser, key)| {
            let full = format!(r"HKCU\{key}\{HOST_NAME}");
            reg(&["delete", &full, "/f"]).ok().map(|()| format!("{browser}: {full}"))
        })
        .collect()
}

// ─── macOS и Linux: папки профилей ───────────────────────────────────────────

/// Браузер и его папка с манифестами помощников. Для Edge папка создаётся
/// всегда, для остальных только если браузер установлен (есть профиль).
#[cfg(unix)]
fn manifest_dirs() -> Vec<(&'static str, PathBuf, bool)> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    #[cfg(target_os = "macos")]
    let (base, list) = (
        home.join("Library/Application Support"),
        [
            ("Microsoft Edge", "Microsoft Edge", true),
            ("Microsoft Edge Beta", "Microsoft Edge Beta", false),
            ("Google Chrome", "Google/Chrome", false),
            ("Chromium", "Chromium", false),
        ],
    );
    #[cfg(not(target_os = "macos"))]
    let (base, list) = (
        home.join(".config"),
        [
            ("Microsoft Edge", "microsoft-edge", true),
            ("Microsoft Edge Beta", "microsoft-edge-beta", false),
            ("Google Chrome", "google-chrome", false),
            ("Chromium", "chromium", false),
        ],
    );
    list.into_iter().map(|(name, sub, always)| (name, base.join(sub), always)).collect()
}

#[cfg(unix)]
fn register_manifest(_manifest: &Path, data: &[u8]) -> Result<Vec<String>, String> {
    let mut places = Vec::new();
    for (browser, profile, always) in manifest_dirs() {
        if !always && !profile.is_dir() {
            continue;
        }
        let dir = profile.join("NativeMessagingHosts");
        fs::create_dir_all(&dir).map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
        let file = dir.join(format!("{HOST_NAME}.json"));
        fs::write(&file, data).map_err(|e| format!("не удалось записать {}: {e}", file.display()))?;
        places.push(format!("{browser}: {}", file.display()));
    }
    Ok(places)
}

#[cfg(unix)]
fn unregister_manifest() -> Vec<String> {
    manifest_dirs()
        .into_iter()
        .filter_map(|(browser, profile, _)| {
            let file = profile.join("NativeMessagingHosts").join(format!("{HOST_NAME}.json"));
            fs::remove_file(&file).ok().map(|()| format!("{browser}: {}", file.display()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_ids() {
        assert!(is_extension_id(EXTENSION_ID));
        assert!(!is_extension_id("ddjphdnapbhpfocepkibnckjlodjhga"));
        assert!(!is_extension_id("ddjphdnapbhpfocepkibnckjlodjhgaz"));
    }

    #[test]
    fn manifest_has_required_fields() {
        let m = manifest_json(Path::new("/opt/lader-host"), &[origin(EXTENSION_ID)]);
        assert_eq!(m["name"], HOST_NAME);
        assert_eq!(m["type"], "stdio");
        assert_eq!(m["path"], "/opt/lader-host");
        assert_eq!(m["allowed_origins"][0], "chrome-extension://kimlopaoiianjkblnjfakfjjilaojkjc/");
    }
}
