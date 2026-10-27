//! Компоненты, без которых приложение не работает: yt-dlp, Deno и ffmpeg.
//!
//! Как лаунчер игры держит свою Java, приложение держит свои копии этих
//! программ в папке данных пользователя (%LOCALAPPDATA%\Lader\bin,
//! ~/Library/Application Support/Lader/bin, ~/.local/share/Lader/bin)
//! и само их ставит: скачивает с официальных страниц проектов, сверяет
//! SHA-256, пробно запускает и только потом подменяет рабочие файлы. Что
//! установлено, записано в `components.json` рядом с ними. В системе
//! (PATH, реестр) ничего не меняется; уже установленные в системе копии
//! используются, пока своих нет.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cancel::CancelToken;
use crate::process::{self, exe_name};
use crate::{archive, fetch, http, settings};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Component {
    YtDlp,
    Deno,
    Ffmpeg,
}

impl Component {
    pub const ALL: [Component; 3] = [Component::YtDlp, Component::Deno, Component::Ffmpeg];

    pub fn title(self) -> &'static str {
        match self {
            Component::YtDlp => "yt-dlp",
            Component::Deno => "Deno",
            Component::Ffmpeg => "ffmpeg",
        }
    }

    pub fn purpose(self) -> &'static str {
        match self {
            Component::YtDlp => "скачивает видео",
            Component::Deno => "без него не работает YouTube",
            Component::Ffmpeg => "склеивает видео со звуком, делает MP3 и обложки",
        }
    }

    /// Ключ в components.json.
    fn key(self) -> &'static str {
        match self {
            Component::YtDlp => "yt-dlp",
            Component::Deno => "deno",
            Component::Ffmpeg => "ffmpeg",
        }
    }

    /// Программы компонента; первая — основная. ffprobe нужен yt-dlp для
    /// извлечения звука, и искать его он будет рядом с ffmpeg.
    fn programs(self) -> &'static [&'static str] {
        match self {
            Component::YtDlp => &["yt-dlp"],
            Component::Deno => &["deno"],
            Component::Ffmpeg => &["ffmpeg", "ffprobe"],
        }
    }

    /// Примерный объём загрузки — чтобы показать его до установки.
    pub fn approx_size_mb(self) -> u32 {
        match self {
            Component::YtDlp => 18,
            Component::Deno => 40,
            Component::Ffmpeg => FFMPEG_SIZE_MB,
        }
    }
}

// ─── Файлы релизов под текущую платформу ─────────────────────────────────────

#[cfg(all(windows, target_arch = "x86_64"))]
mod platform {
    pub const YTDLP: Option<&str> = Some("yt-dlp.exe");
    pub const DENO: Option<&str> = Some("x86_64-pc-windows-msvc");
    pub const FFMPEG: Option<&str> = Some("ffmpeg-master-latest-win64-gpl-shared.zip");
}
#[cfg(all(windows, target_arch = "aarch64"))]
mod platform {
    pub const YTDLP: Option<&str> = Some("yt-dlp_arm64.exe");
    pub const DENO: Option<&str> = Some("aarch64-pc-windows-msvc");
    pub const FFMPEG: Option<&str> = Some("ffmpeg-master-latest-winarm64-gpl-shared.zip");
}
#[cfg(target_os = "macos")]
mod platform {
    pub const YTDLP: Option<&str> = Some("yt-dlp_macos");
    #[cfg(target_arch = "aarch64")]
    pub const DENO: Option<&str> = Some("aarch64-apple-darwin");
    #[cfg(not(target_arch = "aarch64"))]
    pub const DENO: Option<&str> = Some("x86_64-apple-darwin");
    /// Архитектура в адресах ffmpeg.martin-riedl.de.
    #[cfg(target_arch = "aarch64")]
    pub const FFMPEG: Option<&str> = Some("arm64");
    #[cfg(not(target_arch = "aarch64"))]
    pub const FFMPEG: Option<&str> = Some("amd64");
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod platform {
    pub const YTDLP: Option<&str> = Some("yt-dlp_linux");
    pub const DENO: Option<&str> = Some("x86_64-unknown-linux-gnu");
    pub const FFMPEG: Option<&str> = Some("ffmpeg-master-latest-linux64-gpl.tar.xz");
}
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
mod platform {
    pub const YTDLP: Option<&str> = Some("yt-dlp_linux_aarch64");
    pub const DENO: Option<&str> = Some("aarch64-unknown-linux-gnu");
    pub const FFMPEG: Option<&str> = Some("ffmpeg-master-latest-linuxarm64-gpl.tar.xz");
}
#[cfg(not(any(
    all(windows, any(target_arch = "x86_64", target_arch = "aarch64")),
    target_os = "macos",
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
)))]
mod platform {
    pub const YTDLP: Option<&str> = None;
    pub const DENO: Option<&str> = None;
    pub const FFMPEG: Option<&str> = None;
}

#[cfg(windows)]
const FFMPEG_SIZE_MB: u32 = 85;
#[cfg(target_os = "macos")]
const FFMPEG_SIZE_MB: u32 = 56;
#[cfg(not(any(windows, target_os = "macos")))]
const FFMPEG_SIZE_MB: u32 = 150;

const UNSUPPORTED: &str = "для этой платформы нет готовой сборки";

// ─── Где лежит и что установлено ─────────────────────────────────────────────

/// Найденная копия компонента.
#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    /// Основная программа (yt-dlp, deno, ffmpeg).
    pub path: PathBuf,
    pub version: String,
    /// Своя копия приложения (true) или системная из PATH (false).
    pub managed: bool,
}

/// Папка собственных копий компонентов.
pub fn bin_dir() -> Option<PathBuf> {
    settings::data_dir().map(|d| d.join("bin"))
}

/// Запись о компонентах: `bin/components.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    components: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    version: String,
    /// Все файлы компонента в bin: при обновлении лишние старые удаляются.
    files: Vec<String>,
    /// Откуда скачано.
    source: Vec<String>,
    /// Время установки, секунды Unix.
    installed_at: u64,
}

impl Manifest {
    fn path(bin: &Path) -> PathBuf {
        bin.join("components.json")
    }

    fn load(bin: &Path) -> Self {
        fs::read(Self::path(bin)).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default()
    }

    fn save(&self, bin: &Path) -> Result<(), String> {
        let path = Self::path(bin);
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        fs::write(&tmp, data).map_err(|e| format!("не удалось записать {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &path).map_err(|e| format!("не удалось записать {}: {e}", path.display()))
    }
}

/// Ищет компонент: сначала свою копию, затем системную из PATH.
pub fn find(c: Component) -> Option<Installed> {
    find_in(bin_dir().as_deref(), c)
}

fn find_in(bin: Option<&Path>, c: Component) -> Option<Installed> {
    if let Some(bin) = bin
        && c.programs().iter().all(|p| bin.join(exe_name(p)).is_file())
    {
        let path = bin.join(exe_name(c.programs()[0]));
        // Версия из манифеста: не запускаем каждую программу при старте.
        // Копия yt-dlp от прошлых версий приложения манифеста не имеет.
        let version = Manifest::load(bin)
            .components
            .get(c.key())
            .map(|e| e.version.clone())
            .filter(|v| !v.is_empty())
            .or_else(|| query_version(c, &path).ok());
        if let Some(version) = version {
            return Some(Installed { path, version, managed: true });
        }
    }
    let path = process::find_in_path(c.programs()[0])?;
    // ffprobe yt-dlp ищет в той же папке, что и ffmpeg.
    if c.programs()[1..].iter().any(|p| !path.with_file_name(exe_name(p)).is_file()) {
        return None;
    }
    let version = query_version(c, &path).ok()?;
    Some(Installed { path, version, managed: false })
}

/// Версия программы: `yt-dlp --version`, `deno --version`, `ffmpeg -version`.
pub fn query_version(c: Component, path: &Path) -> Result<String, String> {
    let mut cmd = process::command(path);
    cmd.arg(if c == Component::Ffmpeg { "-version" } else { "--version" });
    // Первый запуск onefile-сборки yt-dlp распаковывается во временную папку
    // и на медленном диске с антивирусом занимает несколько секунд.
    let out = process::run(cmd, Duration::from_secs(60), None).map_err(|e| format!("{}: {e}", c.title()))?;
    let text = process::decode(&out.stdout);
    match parse_version(c, &text) {
        Some(v) if out.status.success() => Ok(v),
        _ => Err(format!("{} не запускается: {}", c.title(), process::decode(&out.stderr).trim())),
    }
}

fn parse_version(c: Component, output: &str) -> Option<String> {
    let first = output.lines().map(str::trim).find(|l| !l.is_empty())?;
    match c {
        Component::YtDlp => Some(first.to_string()),
        // "deno 2.9.7 (stable, release, x86_64-pc-windows-msvc)"
        Component::Deno => first.strip_prefix("deno ")?.split_whitespace().next().map(str::to_string),
        // "ffmpeg version 9.0.2-https://www.martin-riedl.de Copyright …"
        Component::Ffmpeg => first.strip_prefix("ffmpeg version ")?.split_whitespace().next().map(short_ffmpeg_version),
    }
}

/// "N-121345-g6d7e8f0-20261016" (ночная сборка) → "2026.10.16";
/// "n8.1-14-gabc" → "8.1"; "9.0.2-https://…" → "9.0.2".
fn short_ffmpeg_version(v: &str) -> String {
    if let Some(rest) = v.strip_prefix("N-") {
        let date = rest.rsplit('-').next().unwrap_or("");
        if date.len() == 8 && date.chars().all(|c| c.is_ascii_digit()) {
            return format!("{}.{}.{}", &date[..4], &date[4..6], &date[6..]);
        }
        return v.to_string();
    }
    let v = v.strip_prefix('n').filter(|r| r.starts_with(|c: char| c.is_ascii_digit())).unwrap_or(v);
    if v.starts_with(|c: char| c.is_ascii_digit()) {
        return v.split('-').next().unwrap_or(v).to_string();
    }
    v.to_string()
}

/// `candidate` новее `current`. Версии yt-dlp — даты "2026.08.19",
/// у ночных сборок с номером сборки "2026.08.19.232521".
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.trim().trim_start_matches('v').split(['.', '-']).map_while(|p| p.parse().ok()).collect()
    }
    let (a, b) = (parts(candidate), parts(current));
    if a.is_empty() {
        return false;
    }
    let len = a.len().max(b.len());
    let pad = |v: &[u64]| (0..len).map(|i| v.get(i).copied().unwrap_or(0)).collect::<Vec<_>>();
    pad(&a) > pad(&b)
}

// ─── Где взять ───────────────────────────────────────────────────────────────

/// Что скачать, чтобы установить компонент.
#[derive(Debug, Clone)]
pub struct Release {
    pub component: Component,
    /// Версия, если её известно заранее (у ночных сборок ffmpeg — нет).
    pub version: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone)]
pub struct Asset {
    /// Имя файла: под ним он ищется в файле контрольных сумм.
    pub name: String,
    pub url: String,
    pub checksum_url: String,
    pub payload: Payload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// Готовый исполняемый файл.
    Executable,
    Zip,
    TarXz,
}

const GITHUB: &str = "https://github.com";

/// Последняя версия yt-dlp на GitHub — для проверки обновлений.
pub fn latest_ytdlp_version() -> Result<String, String> {
    latest_github_tag("yt-dlp/yt-dlp")
}

/// Тег последнего релиза репозитория на GitHub.
///
/// Сначала — страница релизов: она перенаправляет на тег последней версии,
/// отвечает быстро и не имеет лимитов. API (60 запросов в час на IP, иногда
/// подвисает) — запасной путь.
fn latest_github_tag(repo: &str) -> Result<String, String> {
    let redirect_err = match http::redirect_target(&format!("{GITHUB}/{repo}/releases/latest")) {
        Ok(target) => match tag_from_release_url(&target) {
            Some(tag) => return Ok(tag),
            None => format!("не удалось определить версию из {target}"),
        },
        Err(e) => e,
    };
    let json = http::get_github_json(&format!("https://api.github.com/repos/{repo}/releases/latest"))
        .map_err(|e| format!("{redirect_err}; {e}"))?;
    json["tag_name"].as_str().map(str::to_string).ok_or_else(|| "в ответе GitHub нет tag_name".to_string())
}

/// ".../releases/tag/2026.08.19" → "2026.08.19", ".../tag/v2.9.7" → "v2.9.7".
fn tag_from_release_url(url: &str) -> Option<String> {
    let (_, tag) = url.rsplit_once("/tag/")?;
    let tag = tag.trim_end_matches('/');
    let digits = tag.strip_prefix('v').unwrap_or(tag);
    digits.starts_with(|c: char| c.is_ascii_digit()).then(|| tag.to_string())
}

/// Узнаёт, что и откуда качать для последней версии компонента.
pub fn latest_release(c: Component) -> Result<Release, String> {
    match c {
        Component::YtDlp => {
            let asset = platform::YTDLP.ok_or(UNSUPPORTED)?;
            let tag = latest_github_tag("yt-dlp/yt-dlp")?;
            let base = format!("{GITHUB}/yt-dlp/yt-dlp/releases/download/{tag}");
            Ok(Release {
                component: c,
                version: tag,
                assets: vec![Asset {
                    name: asset.into(),
                    url: format!("{base}/{asset}"),
                    checksum_url: format!("{base}/SHA2-256SUMS"),
                    payload: Payload::Executable,
                }],
            })
        }
        Component::Deno => {
            let triple = platform::DENO.ok_or(UNSUPPORTED)?;
            let tag = latest_github_tag("denoland/deno")?;
            let name = format!("deno-{triple}.zip");
            let url = format!("{GITHUB}/denoland/deno/releases/download/{tag}/{name}");
            Ok(Release {
                component: c,
                version: tag.trim_start_matches('v').to_string(),
                assets: vec![Asset { checksum_url: format!("{url}.sha256sum"), url, name, payload: Payload::Zip }],
            })
        }
        Component::Ffmpeg => ffmpeg_release(),
    }
}

/// Windows и Linux: сборки, которые проект yt-dlp готовит специально под
/// себя (с исправлениями для HLS и т.п.). На Windows — вариант с DLL:
/// 85 МБ вместо 191 у статической сборки.
#[cfg(not(target_os = "macos"))]
fn ffmpeg_release() -> Result<Release, String> {
    let name = platform::FFMPEG.ok_or(UNSUPPORTED)?;
    let base = format!("{GITHUB}/yt-dlp/FFmpeg-Builds/releases/download/latest");
    Ok(Release {
        component: Component::Ffmpeg,
        version: String::new(),
        assets: vec![Asset {
            name: name.into(),
            url: format!("{base}/{name}"),
            checksum_url: format!("{base}/checksums.sha256"),
            payload: if name.ends_with(".zip") { Payload::Zip } else { Payload::TarXz },
        }],
    })
}

/// macOS: у yt-dlp сборок нет; берём сервер сборок Martin Riedl — родные
/// сборки для Apple Silicon и Intel с SHA-256 рядом с каждым файлом.
#[cfg(target_os = "macos")]
fn ffmpeg_release() -> Result<Release, String> {
    let arch = platform::FFMPEG.ok_or(UNSUPPORTED)?;
    let latest = format!("https://ffmpeg.martin-riedl.de/redirect/latest/macos/{arch}/release/ffmpeg.zip");
    let url = http::redirect_target(&latest)?;
    let dir = url.strip_suffix("/ffmpeg.zip").ok_or_else(|| format!("неожиданный адрес сборки: {url}"))?;
    let asset = |program: &str| Asset {
        name: format!("{program}.zip"),
        url: format!("{dir}/{program}.zip"),
        checksum_url: format!("{dir}/{program}.zip.sha256"),
        payload: Payload::Zip,
    };
    Ok(Release {
        component: Component::Ffmpeg,
        version: martin_riedl_version(dir).unwrap_or_default(),
        assets: vec![asset("ffmpeg"), asset("ffprobe")],
    })
}

/// ".../download/macos/arm64/1789931890_9.0.2" → "9.0.2".
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn martin_riedl_version(dir: &str) -> Option<String> {
    let last = dir.trim_end_matches('/').rsplit('/').next()?;
    let (_, version) = last.split_once('_')?;
    (!version.is_empty()).then(|| version.to_string())
}

/// Ищет SHA-256 файла в файле контрольных сумм. Форматы бывают разные:
/// "hash  имя" на строку (yt-dlp, ffmpeg) или одна сумма на файл, в том
/// числе в виде вывода PowerShell "Hash : A0C3…" (Deno для Windows).
fn find_checksum(sums: &str, name: &str) -> Option<String> {
    let is_hash = |s: &str| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit());
    let by_name = sums.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let (hash, file) = (it.next()?, it.next()?);
        (is_hash(hash) && file.trim_start_matches('*') == name).then(|| hash.to_lowercase())
    });
    if by_name.is_some() {
        return by_name;
    }
    let all: Vec<&str> = sums.split(|c: char| !c.is_ascii_hexdigit()).filter(|s| is_hash(s)).collect();
    match all.as_slice() {
        [single] => Some(single.to_lowercase()),
        _ => None,
    }
}

// ─── Установка ───────────────────────────────────────────────────────────────

/// Этап установки — для индикатора в UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stage {
    /// Узнаём последнюю версию.
    Resolving,
    /// Скачивание файла `part` из `parts`.
    Downloading { done: u64, total: Option<u64>, part: usize, parts: usize },
    Unpacking,
    /// Пробный запуск новой версии.
    Verifying,
}

/// Ставит последнюю версию компонента в собственную папку приложения.
/// `connections` — во сколько соединений качать (как у загрузки видео).
pub fn install(
    c: Component,
    connections: usize,
    cancel: &CancelToken,
    on_stage: &mut dyn FnMut(Stage),
) -> Result<Installed, String> {
    let bin = bin_dir().ok_or("не удалось определить папку данных приложения")?;
    on_stage(Stage::Resolving);
    let release = latest_release(c)?;
    install_release(&bin, &release, connections, cancel, on_stage)
}

/// Скачивает файлы релиза, проверяет и атомарно подменяет компонент в `bin`.
pub fn install_release(
    bin: &Path,
    release: &Release,
    connections: usize,
    cancel: &CancelToken,
    on_stage: &mut dyn FnMut(Stage),
) -> Result<Installed, String> {
    let c = release.component;
    let work = bin.join(format!(".install-{}", c.key()));
    let _ = fs::remove_dir_all(&work);
    let staged = work.join("staged");
    fs::create_dir_all(&staged).map_err(|e| format!("не удалось создать {}: {e}", staged.display()))?;
    let result = stage_and_replace(bin, &work, &staged, release, connections, cancel, on_stage);
    let _ = fs::remove_dir_all(&work);
    result
}

fn stage_and_replace(
    bin: &Path,
    work: &Path,
    staged: &Path,
    release: &Release,
    connections: usize,
    cancel: &CancelToken,
    on_stage: &mut dyn FnMut(Stage),
) -> Result<Installed, String> {
    let c = release.component;
    let parts = release.assets.len();
    for (i, asset) in release.assets.iter().enumerate() {
        let sums = http::get_text(&asset.checksum_url).map_err(|e| format!("не удалось получить контрольную сумму: {e}"))?;
        let expected =
            find_checksum(&sums, &asset.name).ok_or_else(|| format!("нет контрольной суммы для {}", asset.name))?;
        let file = work.join(&asset.name);
        download_verified(&asset.url, &file, &expected, connections, cancel, &mut |done, total| {
            on_stage(Stage::Downloading { done, total, part: i + 1, parts })
        })?;
        let wanted = |name: &str| is_component_file(c, name);
        match asset.payload {
            Payload::Executable => {
                fs::rename(&file, staged.join(exe_name(c.programs()[0]))).map_err(|e| e.to_string())?;
            }
            Payload::Zip => {
                on_stage(Stage::Unpacking);
                archive::extract_zip(&file, staged, &wanted)?;
            }
            Payload::TarXz => {
                on_stage(Stage::Unpacking);
                archive::extract_tar_xz(&file, staged, &wanted)?;
            }
        }
        let _ = fs::remove_file(&file);
    }

    on_stage(Stage::Verifying);
    for program in c.programs() {
        let path = staged.join(exe_name(program));
        if !path.is_file() {
            return Err(format!("в скачанном архиве нет {}", exe_name(program)));
        }
        make_runnable(&path)?;
    }
    let version = query_version(c, &staged.join(exe_name(c.programs()[0])))
        .map_err(|e| format!("новая версия не запускается: {e}"))?;
    if c == Component::YtDlp && is_newer(&release.version, &version) {
        return Err(format!("скачана версия {version}, ожидалась {}", release.version));
    }

    let mut files: Vec<String> = fs::read_dir(staged)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();

    let mut manifest = Manifest::load(bin);
    let previous = manifest.components.get(c.key()).map(|e| e.files.clone()).unwrap_or_default();
    for name in &files {
        replace_file(&staged.join(name), &bin.join(name))?;
    }
    // Старые файлы, которых нет в новой версии (DLL с другим номером).
    for old in previous.iter().filter(|f| !files.contains(f)) {
        let _ = fs::remove_file(bin.join(old));
    }
    let installed_at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let source = release.assets.iter().map(|a| a.url.clone()).collect();
    manifest.components.insert(c.key().into(), Entry { version: version.clone(), files, source, installed_at });
    manifest.save(bin)?;

    Ok(Installed { path: bin.join(exe_name(c.programs()[0])), version, managed: true })
}

/// Файл из архива, который нужно оставить. Сборка ffmpeg для Windows
/// использует DLL — они должны лежать рядом с ffmpeg.exe.
fn is_component_file(c: Component, name: &str) -> bool {
    c.programs().iter().any(|p| exe_name(p) == name)
        || (c == Component::Ffmpeg && cfg!(windows) && name.to_ascii_lowercase().ends_with(".dll"))
}

fn download_verified(
    url: &str,
    dest: &Path,
    expected: &str,
    connections: usize,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let request = fetch::Request { url: url.to_string(), headers: Vec::new() };
    fetch::download_to_file(&request, dest, fetch::Options::with_connections(connections), cancel, &mut |p| {
        progress(p.done, p.total)
    })?;
    // Части приходят не по порядку, поэтому сумму считаем по готовому файлу.
    let actual = sha256_file(dest)?;
    if !actual.eq_ignore_ascii_case(expected) {
        let _ = fs::remove_file(dest);
        return Err("контрольная сумма не совпала — файл повреждён при загрузке".into());
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("не удалось прочитать {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Атомарно подменяет файл. Запущенный exe или загруженную DLL Windows
/// удалить не даёт, но переименовать — даёт, поэтому старая версия сначала
/// отодвигается в .old. Антивирус может ненадолго держать свежий файл —
/// повторяем.
fn replace_file(new: &Path, target: &Path) -> Result<(), String> {
    let old = PathBuf::from(format!("{}.old", target.display()));
    let _ = fs::remove_file(&old);
    let mut last_err = None;
    for attempt in 0..10 {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(300));
        }
        if target.exists()
            && let Err(e) = fs::rename(target, &old)
        {
            last_err = Some(e);
            continue;
        }
        match fs::rename(new, target) {
            Ok(()) => {
                let _ = fs::remove_file(&old);
                return Ok(());
            }
            Err(e) => {
                // Вернём старую версию на место, чтобы не остаться без программы.
                if old.exists() && !target.exists() {
                    let _ = fs::rename(&old, target);
                }
                last_err = Some(e);
            }
        }
    }
    Err(format!(
        "не удалось заменить {}: {}",
        target.display(),
        last_err.map_or_else(|| "неизвестная ошибка".into(), |e| e.to_string())
    ))
}

#[cfg(unix)]
fn make_runnable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    macos_allow(path);
    Ok(())
}

#[cfg(not(unix))]
fn make_runnable(_: &Path) -> Result<(), String> {
    Ok(())
}

/// На Apple Silicon запускаются только подписанные программы. Скачанные нами
/// файлы карантинной метки не получают, но если сборка без подписи —
/// подписываем её локально (ad-hoc), как это сделал бы сам разработчик.
#[cfg(target_os = "macos")]
fn macos_allow(path: &Path) {
    use std::process::{Command, Stdio};
    let quiet = |cmd: &mut Command| cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    let _ = quiet(Command::new("xattr").args(["-d", "com.apple.quarantine"]).arg(path));
    if !quiet(Command::new("codesign").arg("--verify").arg(path)).is_ok_and(|s| s.success()) {
        let _ = quiet(Command::new("codesign").args(["--force", "--sign", "-"]).arg(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("2026.08.19", "2026.03.17"));
        assert!(is_newer("2026.10.01", "2026.09.30"));
        assert!(is_newer("2026.08.19.120000", "2026.08.19"));
        assert!(!is_newer("2026.08.19", "2026.08.19"));
        assert!(!is_newer("2026.08.19", "2026.08.19.232521"));
        assert!(!is_newer("2026.03.17", "2026.08.19"));
        assert!(!is_newer("", "2026.08.19"));
        assert!(is_newer("2026.08.19", ""));
    }

    #[test]
    fn extracts_tags_from_release_urls() {
        let tag = |u: &str| tag_from_release_url(u);
        assert_eq!(tag("https://github.com/yt-dlp/yt-dlp/releases/tag/2026.08.19").as_deref(), Some("2026.08.19"));
        assert_eq!(tag("https://github.com/denoland/deno/releases/tag/v2.9.7").as_deref(), Some("v2.9.7"));
        assert_eq!(tag("https://github.com/yt-dlp/yt-dlp/releases"), None);
        assert_eq!(tag("https://github.com/login?return_to=/tag/x"), None);
    }

    #[test]
    fn finds_checksums_in_all_formats() {
        // yt-dlp и ffmpeg: "hash  имя" на строку.
        let sums = "1fa6733c37ea6fb51c99ad8fe785e7b7e5f3246c9b980230329d4fb72ed8d4d6  yt-dlp\n\
                    66674953FE251B89F4D08C5F0E35E0728679BD67AB3D7D05C0562AF101DD3E7A  yt-dlp.exe\n";
        assert_eq!(
            find_checksum(sums, "yt-dlp.exe").as_deref(),
            Some("66674953fe251b89f4d08c5f0e35e0728679bd67ab3d7d05c0562af101dd3e7a")
        );
        assert_eq!(find_checksum(sums, "yt-dlp_x86.exe"), None);

        // Deno для Windows: вывод PowerShell Get-FileHash.
        let ps = "\r\nAlgorithm : SHA256\r\nHash      : A0C3101B4158D1DFB7D6A78A7BF0F3DE80C96BB423C152BEEC8BEB22786F2238\r\n\
                  Path      : C:\\a\\deno\\deno\\target\\release\\deno-x86_64-pc-windows-msvc.zip\r\n\r\n";
        assert_eq!(
            find_checksum(ps, "deno-x86_64-pc-windows-msvc.zip").as_deref(),
            Some("a0c3101b4158d1dfb7d6a78a7bf0f3de80c96bb423c152beec8beb22786f2238")
        );

        // Одна сумма на файл в обычном формате (Deno для macOS, ffmpeg для macOS).
        let one = "5cd46d6268f6f78f5d88bdc7159d20bd44cdaa4b3303474839f87ec6fe7ae25c  deno-aarch64-apple-darwin.zip";
        assert!(find_checksum(one, "deno-aarch64-apple-darwin.zip").is_some());
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version(Component::YtDlp, "2026.08.19\n").as_deref(), Some("2026.08.19"));
        assert_eq!(
            parse_version(Component::Deno, "deno 2.9.7 (stable, release, x86_64-pc-windows-msvc)\nv8 14.0\n").as_deref(),
            Some("2.9.7")
        );
        let ff = |line: &str| parse_version(Component::Ffmpeg, line);
        assert_eq!(ff("ffmpeg version N-121345-g6d7e8f0-20261016 Copyright (c) 2000-2026").as_deref(), Some("2026.10.16"));
        assert_eq!(ff("ffmpeg version 9.0.2-https://www.martin-riedl.de Copyright").as_deref(), Some("9.0.2"));
        assert_eq!(ff("ffmpeg version n8.1-14-gabc Copyright").as_deref(), Some("8.1"));
        assert_eq!(ff("ffmpeg version 8.0-essentials_build-www.gyan.dev Copyright").as_deref(), Some("8.0"));
        assert_eq!(ff("garbage"), None);
    }

    #[test]
    fn reads_martin_riedl_version() {
        assert_eq!(
            martin_riedl_version("https://ffmpeg.martin-riedl.de/download/macos/arm64/1789931890_9.0.2").as_deref(),
            Some("9.0.2")
        );
        assert_eq!(martin_riedl_version("https://ffmpeg.martin-riedl.de/download/macos/arm64/latest"), None);
    }

    #[test]
    fn selects_component_files() {
        assert!(is_component_file(Component::Deno, &exe_name("deno")));
        assert!(!is_component_file(Component::Deno, "README.md"));
        assert!(is_component_file(Component::Ffmpeg, &exe_name("ffprobe")));
        assert!(!is_component_file(Component::Ffmpeg, &exe_name("ffplay")));
        assert_eq!(is_component_file(Component::Ffmpeg, "avcodec-62.dll"), cfg!(windows));
    }

    #[test]
    fn finds_managed_copy_by_manifest() {
        let bin = std::env::temp_dir().join(format!("ytdl-components-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&bin);
        fs::create_dir_all(&bin).unwrap();
        for p in ["ffmpeg", "ffprobe"] {
            fs::write(bin.join(exe_name(p)), b"stub").unwrap();
        }
        let mut manifest = Manifest::default();
        manifest.components.insert(
            "ffmpeg".into(),
            Entry { version: "9.0.2".into(), files: vec![], source: vec![], installed_at: 0 },
        );
        manifest.save(&bin).unwrap();

        let found = find_in(Some(&bin), Component::Ffmpeg).unwrap();
        assert_eq!(found, Installed { path: bin.join(exe_name("ffmpeg")), version: "9.0.2".into(), managed: true });

        // Без ffprobe своя копия неполная и не считается установленной.
        fs::remove_file(bin.join(exe_name("ffprobe"))).unwrap();
        assert!(find_in(Some(&bin), Component::Ffmpeg).is_none_or(|i| !i.managed));
        fs::remove_dir_all(&bin).unwrap();
    }

    /// Узнаёт, откуда качать каждый компонент. Ходит в сеть, но качает только
    /// файлы контрольных сумм: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn resolves_releases_with_checksums() {
        for c in Component::ALL {
            let release = latest_release(c).unwrap_or_else(|e| panic!("{}: {e}", c.title()));
            assert!(!release.assets.is_empty());
            for asset in &release.assets {
                let sums = http::get_text(&asset.checksum_url).unwrap();
                assert!(find_checksum(&sums, &asset.name).is_some(), "{}: {}", c.title(), asset.name);
            }
        }
    }

    /// Полный цикл для yt-dlp: скачивание → SHA-256 → пробный запуск →
    /// подмена старого файла. Ходит в сеть: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn installs_latest_ytdlp_over_old_file() {
        let bin = std::env::temp_dir().join(format!("ytdl-install-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&bin);
        fs::create_dir_all(&bin).unwrap();
        let target = bin.join(exe_name("yt-dlp"));
        fs::write(&target, b"old version").unwrap();

        let release = latest_release(Component::YtDlp).expect("GitHub reachable");
        let mut stages = Vec::new();
        let installed =
            install_release(&bin, &release, 8, &CancelToken::new(), &mut |s| stages.push(s)).expect("install");

        assert_eq!(installed.version, release.version);
        assert_eq!(query_version(Component::YtDlp, &target).unwrap(), release.version);
        assert!(stages.contains(&Stage::Verifying));
        assert!(stages.iter().any(|s| matches!(s, Stage::Downloading { total: Some(_), .. })));
        assert!(!bin.join(".install-yt-dlp").exists());
        let found = find_in(Some(&bin), Component::YtDlp).unwrap();
        assert!(found.managed);
        assert_eq!(found.version, release.version);
        fs::remove_dir_all(&bin).unwrap();
    }
}
