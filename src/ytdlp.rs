//! Работа с yt-dlp: метаданные видео и плейлистов, загрузка с прогрессом.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Deserialize;

use crate::cancel::CancelToken;
use crate::process::{self, RunError};
use crate::{fetch, format, postprocess, urls};

/// Файл-архив yt-dlp в папке загрузки: ID уже скачанных видео.
pub const ARCHIVE_FILE: &str = ".ytdl-archive.txt";

/// Сколько видео канала подгружать за раз.
pub const PAGE_SIZE: usize = 30;

const INFO_TIMEOUT: Duration = Duration::from_secs(60);
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(120);

/// Маркеры наших собственных строк в выводе yt-dlp (см. `build_args`).
const PROGRESS_MARK: &str = "[ytdl-progress] ";
const FILE_MARK: &str = "[ytdl-file] ";

/// Внешние программы, с которыми работает приложение.
#[derive(Debug, Clone)]
pub struct Tools {
    pub ytdlp: PathBuf,
    pub ffmpeg: Option<PathBuf>,
    pub deno: Option<PathBuf>,
}

impl Tools {
    fn ytdlp_command(&self) -> Command {
        let mut cmd = process::command(&self.ytdlp);
        // Без этого yt-dlp на русской Windows пишет в пайп в cp1251 и молча
        // выбрасывает символы, которых там нет, — пути к файлам с кириллицей
        // и эмодзи в названии приходили бы битыми.
        cmd.args(["--encoding", "utf-8"]);
        if let Some(ffmpeg) = &self.ffmpeg {
            cmd.arg("--ffmpeg-location").arg(ffmpeg);
        }
        // Deno yt-dlp ищет в PATH. Папку с ним ставим в начало PATH дочернего
        // процесса, а не передаём --js-runtimes: этот флаг есть не во всех
        // версиях yt-dlp, а старая версия с незнакомым флагом не запустится.
        if let Some(dir) = self.deno.as_deref().and_then(Path::parent) {
            let mut dirs = vec![dir.to_path_buf()];
            dirs.extend(std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default());
            if let Ok(path) = std::env::join_paths(dirs) {
                cmd.env("PATH", path);
            }
        }
        cmd
    }

    fn ffmpeg(&self) -> PathBuf {
        self.ffmpeg.clone().unwrap_or_else(|| PathBuf::from("ffmpeg"))
    }
}

// ─── Метаданные одного видео ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Quality {
    pub height: u32,
    pub fps: u32,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub title: String,
    pub thumbnail: String,
    pub duration: u64,
    pub uploader: String,
    pub qualities: Vec<Quality>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawVideo {
    title: Option<String>,
    thumbnail: Option<String>,
    duration: Option<f64>,
    uploader: Option<String>,
    channel: Option<String>,
    formats: Option<Vec<RawFormat>>,
    // Твит с несколькими видео отдаётся как плейлист: formats лежат
    // не в корне, а во вложенных entries.
    entries: Option<Vec<Option<RawVideo>>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawFormat {
    vcodec: Option<String>,
    height: Option<f64>,
    fps: Option<f64>,
}

/// Получает информацию о видео через `yt-dlp -J`.
pub fn get_video_info(tools: &Tools, url: &str, cookies: &Cookies) -> Result<VideoInfo, String> {
    let mut cmd = tools.ytdlp_command();
    cmd.args(["--no-playlist", "--no-warnings", "-J"]);
    cmd.args(cookies.args());
    cmd.args(["--", url]);
    let stdout = run_ytdlp(cmd, INFO_TIMEOUT)?;
    parse_video_info(&stdout)
}

fn parse_video_info(json: &[u8]) -> Result<VideoInfo, String> {
    let mut raw: RawVideo =
        serde_json::from_slice(json).map_err(|e| format!("не удалось разобрать ответ yt-dlp: {e}"))?;

    // Твит с несколькими роликами приходит плейлистом: берём первое видео —
    // именно его потом скачает загрузка с --no-playlist.
    if raw.formats.as_ref().is_none_or(Vec::is_empty)
        && let Some(mut first) = raw.entries.take().into_iter().flatten().flatten().next()
    {
        first.title = first.title.or(raw.title);
        first.uploader = first.uploader.or(raw.uploader);
        first.channel = first.channel.or(raw.channel);
        first.thumbnail = first.thumbnail.or(raw.thumbnail);
        raw = first;
    }

    // Уникальные высоты среди форматов с видеопотоком, для каждой — максимальный fps.
    let mut heights: BTreeMap<u32, u32> = BTreeMap::new();
    for f in raw.formats.unwrap_or_default() {
        if f.vcodec.as_deref().is_none_or(|v| v.is_empty() || v == "none") {
            continue;
        }
        let Some(h) = f.height.filter(|h| *h > 0.0) else { continue };
        let fps = f.fps.unwrap_or(0.0) as u32;
        let entry = heights.entry(h as u32).or_insert(fps);
        *entry = (*entry).max(fps);
    }

    let qualities = heights
        .into_iter()
        .rev()
        .map(|(height, fps)| {
            let mut label = match height {
                h if h >= 2160 => format!("4K ({h}p)"),
                h if h >= 1440 => format!("2K ({h}p)"),
                h => format!("{h}p"),
            };
            if fps > 30 {
                label.push_str(&format!(" {fps}fps"));
            }
            Quality { height, fps, label }
        })
        .collect();

    Ok(VideoInfo {
        title: raw.title.unwrap_or_default(),
        thumbnail: raw.thumbnail.unwrap_or_default(),
        duration: raw.duration.unwrap_or(0.0).max(0.0) as u64,
        uploader: raw.uploader.filter(|u| !u.is_empty()).or(raw.channel).unwrap_or_default(),
        qualities,
    })
}

// ─── Каналы и плейлисты ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ChannelEntry {
    pub id: String,
    pub url: String,
    pub title: String,
    pub duration: u64,
    pub thumbnail: String,
}

#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub title: String,
    pub uploader: String,
    pub entries: Vec<ChannelEntry>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawPlaylist {
    title: Option<String>,
    uploader: Option<String>,
    channel: Option<String>,
    entries: Option<Vec<Option<RawEntry>>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawEntry {
    #[serde(rename = "_type")]
    kind: Option<String>,
    id: Option<String>,
    url: Option<String>,
    webpage_url: Option<String>,
    title: Option<String>,
    duration: Option<f64>,
    thumbnail: Option<String>,
    thumbnails: Option<Vec<RawThumb>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawThumb {
    url: Option<String>,
}

/// Получает страницу видео канала или плейлиста: `offset` видео пропускаются,
/// берётся не больше `limit`.
pub fn get_channel_videos(
    tools: &Tools,
    url: &str,
    offset: usize,
    limit: usize,
    cookies: &Cookies,
) -> Result<ChannelInfo, String> {
    let items = format!("{}:{}", offset + 1, offset + limit);
    let mut cmd = tools.ytdlp_command();
    cmd.args(["--flat-playlist", "--no-warnings", "-I", &items, "-J"]);
    cmd.args(cookies.args());
    cmd.args(["--", url]);
    let stdout = run_ytdlp(cmd, PLAYLIST_TIMEOUT)?;
    parse_channel(&stdout, urls::is_youtube_url(url))
}

fn parse_channel(json: &[u8], is_youtube: bool) -> Result<ChannelInfo, String> {
    let raw: RawPlaylist =
        serde_json::from_slice(json).map_err(|e| format!("не удалось разобрать плейлист: {e}"))?;

    let entries = raw
        .entries
        .unwrap_or_default()
        .into_iter()
        .flatten()
        .map(|e| {
            let id = e.id.unwrap_or_default();
            let is_video = e.kind.as_deref() != Some("playlist") && looks_like_video_id(&id);
            // Для YouTube собираем каноничную ссылку по ID: yt-dlp иногда отдаёт
            // в url только id. Для остальных сайтов нужен готовый адрес.
            let url = if is_youtube && is_video {
                format!("https://www.youtube.com/watch?v={id}")
            } else {
                e.webpage_url.filter(|u| !u.is_empty()).or(e.url).unwrap_or_default()
            };
            let thumbnail = e
                .thumbnail
                .filter(|t| !t.is_empty())
                .or_else(|| (is_youtube && is_video).then(|| format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg")))
                .or_else(|| e.thumbnails.unwrap_or_default().into_iter().rev().find_map(|t| t.url))
                .unwrap_or_default();
            ChannelEntry {
                id,
                url,
                title: e.title.unwrap_or_default(),
                duration: e.duration.unwrap_or(0.0).max(0.0) as u64,
                thumbnail,
            }
        })
        .filter(|e| !e.url.is_empty())
        .collect();

    Ok(ChannelInfo {
        title: raw.title.unwrap_or_default(),
        uploader: raw.uploader.filter(|u| !u.is_empty()).or(raw.channel).unwrap_or_default(),
        entries,
    })
}

/// ID видео YouTube — 11 символов из [A-Za-z0-9_-]; ID канала (UC…) длиннее.
fn looks_like_video_id(id: &str) -> bool {
    id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Проверяет по архиву yt-dlp, скачивалось ли видео в эту папку.
pub fn is_in_archive(out_dir: &Path, video_id: &str) -> bool {
    if video_id.is_empty() {
        return false;
    }
    let Ok(data) = fs::read_to_string(out_dir.join(ARCHIVE_FILE)) else { return false };
    // Строки архива: "<extractor> <id>", например "youtube dQw4w9WgXcQ".
    data.lines().any(|line| line.split_whitespace().nth(1) == Some(video_id))
}

// ─── Загрузка ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoContainer {
    Mp4,
    Webm,
    Mkv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Mp3,
    M4a,
}

/// Что именно скачиваем.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Video { max_height: u32, container: VideoContainer },
    Audio(AudioFormat),
}

/// Откуда yt-dlp берёт cookies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Cookies {
    #[default]
    None,
    /// `--cookies-from-browser`: yt-dlp сам читает базу cookies браузера.
    Browser(String),
    /// `--cookies`: файл в формате Netscape. Его пишет помощник
    /// браузерного расширения из cookies, которые отдал сам браузер.
    File(PathBuf),
}

impl Cookies {
    /// Значение из настроек (`--cookies-from-browser`); пустое — без cookies.
    pub fn from_browser(browser: &str) -> Self {
        if browser.is_empty() { Cookies::None } else { Cookies::Browser(browser.to_string()) }
    }

    pub fn is_none(&self) -> bool {
        *self == Cookies::None
    }

    fn args(&self) -> Vec<String> {
        match self {
            Cookies::None => Vec::new(),
            Cookies::Browser(browser) => vec!["--cookies-from-browser".into(), browser.clone()],
            Cookies::File(path) => vec!["--cookies".into(), path.to_string_lossy().into_owned()],
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub url: String,
    pub target: Target,
    pub out_dir: PathBuf,
    pub cookies: Cookies,
    /// Сколько соединений на файл: 1 — обычная загрузка средствами yt-dlp,
    /// больше — многопоточная загрузка приложения (см. `fetch`).
    pub connections: usize,
}

/// Какой поток сейчас качается: при раздельных дорожках YouTube отдаёт
/// видео и аудио отдельными файлами, и прогресс идёт дважды от 0 до 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Video,
    Audio,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DownloadProgress {
    pub percent: Option<f64>,
    pub size: Option<String>,
    pub speed: Option<String>,
    pub eta: Option<String>,
    pub stream: Option<Stream>,
    /// Сколько соединений качает прямо сейчас (при многопоточной загрузке).
    pub connections: Option<usize>,
}

/// Промежуточные события загрузки.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// Этап работы: извлечение информации, конвертация, обложка…
    Status(String),
    Download(DownloadProgress),
    /// Склейка видео и аудио в один файл.
    Merging,
}

/// Чем закончилась загрузка.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done { file: Option<PathBuf>, already_downloaded: bool },
    Failed(String),
    Cancelled,
}

/// Формирует аргументы yt-dlp для загрузки.
///
/// Стратегия по контейнерам и кодекам:
///
/// * mp4: H.264 + AAC мультиплексируются в mp4 без перекодирования; фолбеки —
///   любое видео + AAC, затем любое видео + любое аудио (ffmpeg перекодирует
///   OPUS → AAC). `--merge-output-format mp4` гарантирует контейнер.
/// * webm: VP9/AV1 + OPUS — нативные для webm, слияние без потерь.
/// * mkv: лучшее качество, любые кодеки без перекодирования.
/// * mp3 / m4a: только аудио, `--extract-audio` конвертирует.
pub fn build_args(opts: &DownloadOptions) -> Vec<String> {
    args(opts, Mode::Download)
}

/// Как запускается yt-dlp.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode<'a> {
    /// Только выбрать форматы и вывести их адреса (`-j`), ничего не качая.
    Probe,
    /// Обычная загрузка.
    Download,
    /// Дорожки уже скачаны приложением: склеить, поставить обложку, записать
    /// архив — по сохранённому описанию из `Probe`.
    Finish(&'a Path),
}

fn args(opts: &DownloadOptions, mode: Mode) -> Vec<String> {
    let out = &opts.out_dir;
    let mut args: Vec<String> = [
        "--no-playlist",
        "--no-overwrites",
        "--no-warnings",
        "--retries",
        "3",
        "--fragment-retries",
        "3",
        // У X заголовок — это текст твита целиком, из него получаются имена
        // в сотни символов, которые упираются в лимит пути Windows (260).
        "--trim-filenames",
        "120",
    ]
    .map(String::from)
    .to_vec();
    // Видео, нарезанные на фрагменты (HLS у X), yt-dlp и сам качает в
    // несколько потоков — столько же, сколько выбрано для файлов.
    args.push("--concurrent-fragments".into());
    args.push(opts.connections.max(1).to_string());
    if mode != Mode::Probe {
        args.extend(
            [
                "--newline",
                // --print ниже включает «тихий» режим, в котором yt-dlp молчит о
                // прогрессе, склейке и пропуске по архиву. --no-quiet возвращает вывод.
                "--no-quiet",
                // Машиночитаемый прогресс вместо человеческой строки «[download] 45.3% of …»,
                // формат которой меняется от версии к версии и от режима загрузки.
                "--progress-template",
                "download:[ytdl-progress] %(progress.status)s|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s|%(progress.eta)s|%(info.vcodec)s",
                // Реальный путь к итоговому файлу после всех merge/move: строка
                // «[download] Destination» указывает на промежуточный fNNN-файл.
                "--print",
                "after_move:[ytdl-file] %(filepath)s",
            ]
            .map(String::from),
        );
    }

    // Папка — отдельно через -P, а шаблоны имён относительные: --trim-filenames
    // обрезает всё, что получилось из шаблона, и с абсолютным путём в -o
    // при длинной папке резал бы название видео («Me .mp4») и даже _thumbs.
    args.push("-P".into());
    args.push(out.to_string_lossy().into_owned());
    args.extend(["-o", "%(title)s.%(ext)s"].map(String::from));

    // Превью: всегда .jpg (Windows показывает jpg как иконку файла, webp — нет).
    // Кладём в _thumbs/, а для mp4/mp3/m4a потом встраиваем через ffmpeg и
    // удаляем. --embed-thumbnail не используем: для mp4 yt-dlp нужен
    // AtomicParsley или mutagen, которых на типовой Windows нет, и он молча
    // пропускает встраивание.
    args.push("-o".into());
    args.push(format!("thumbnail:{}/%(title)s.%(ext)s", postprocess::THUMBS_DIR));
    args.extend(["--write-thumbnail", "--convert-thumbnails", "jpg"].map(String::from));

    // Архив: повторный запуск пропускает уже скачанные видео по ID.
    args.push("--download-archive".into());
    args.push(out.join(ARCHIVE_FILE).to_string_lossy().into_owned());

    args.extend(opts.cookies.args());

    match opts.target {
        Target::Audio(fmt) => {
            let codec = match fmt {
                AudioFormat::Mp3 => "mp3",
                AudioFormat::M4a => "m4a",
            };
            args.extend(["-f", "bestaudio[ext=m4a]/bestaudio", "--extract-audio", "--audio-format", codec].map(String::from));
            if fmt == AudioFormat::Mp3 {
                args.extend(["--audio-quality", "0"].map(String::from));
            }
        }
        Target::Video { max_height: h, container } => {
            let (selector, merge) = match container {
                VideoContainer::Mp4 => (
                    format!(
                        "bestvideo[height<={h}][vcodec^=avc1]+bestaudio[ext=m4a]\
                         /bestvideo[height<={h}][ext=mp4]+bestaudio[ext=m4a]\
                         /bestvideo[height<={h}]+bestaudio[ext=m4a]\
                         /bestvideo[height<={h}]+bestaudio\
                         /best[height<={h}]"
                    ),
                    "mp4",
                ),
                VideoContainer::Webm => (
                    format!("bestvideo[height<={h}][ext=webm]+bestaudio[ext=webm]/bestvideo[height<={h}]+bestaudio"),
                    "webm",
                ),
                VideoContainer::Mkv => (format!("bestvideo[height<={h}]+bestaudio/best[height<={h}]"), "mkv"),
            };
            args.extend(["-f".into(), selector, "--merge-output-format".into(), merge.into()]);
        }
    }

    match mode {
        Mode::Probe => args.extend(["-j".into(), "--".into(), opts.url.clone()]),
        Mode::Download => args.extend(["--".into(), opts.url.clone()]),
        Mode::Finish(info) => {
            args.push("--load-info-json".into());
            args.push(info.to_string_lossy().into_owned());
        }
    }
    args
}

/// Разобранная строка вывода yt-dlp.
#[derive(Debug, PartialEq)]
enum Line {
    Progress(DownloadProgress),
    FinalFile(PathBuf),
    Destination(PathBuf),
    /// Файл уже есть на диске (Some) или ID записан в архиве (None).
    AlreadyDownloaded(Option<PathBuf>),
    Merging(Option<PathBuf>),
    Status(&'static str),
    Other,
}

static LEGACY_PERCENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[download\]\s+([\d.]+)%").unwrap());
static LEGACY_SIZE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"of\s+~?\s*([\d.]+\s*\w+)").unwrap());
static LEGACY_SPEED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"at\s+([\d.]+\s*\w+/s)").unwrap());
static LEGACY_ETA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"ETA\s+(\S+)").unwrap());
static DESTINATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[download\] Destination: (.+)$").unwrap());
static ALREADY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[download\]\s+(.+?)\s+has already been downloaded").unwrap());
static MERGER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^\[Merger\] Merging formats into "(.+)"$"#).unwrap());
static EXTRACTOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[[\w:.-]+\] (Extracting URL|[^:]+: (Downloading|Extracting|Checking))").unwrap());

fn parse_line(line: &str) -> Line {
    if let Some(rest) = line.strip_prefix(PROGRESS_MARK) {
        return parse_template_progress(rest).map_or(Line::Other, Line::Progress);
    }
    if let Some(path) = line.strip_prefix(FILE_MARK) {
        return Line::FinalFile(PathBuf::from(path.trim()));
    }
    if let Some(c) = LEGACY_PERCENT.captures(line) {
        let grab = |re: &Regex| re.captures(line).map(|c| c[1].trim().to_string());
        return Line::Progress(DownloadProgress {
            percent: c[1].parse().ok(),
            size: grab(&LEGACY_SIZE),
            speed: grab(&LEGACY_SPEED),
            eta: grab(&LEGACY_ETA),
            stream: None,
            connections: None,
        });
    }
    if let Some(c) = DESTINATION.captures(line) {
        return Line::Destination(PathBuf::from(c[1].trim()));
    }
    if line.contains("has already been recorded in the archive") {
        return Line::AlreadyDownloaded(None);
    }
    if let Some(c) = ALREADY.captures(line) {
        return Line::AlreadyDownloaded(Some(PathBuf::from(c[1].trim())));
    }
    if let Some(c) = MERGER.captures(line) {
        return Line::Merging(Some(PathBuf::from(&c[1])));
    }
    if line.starts_with("[Merger]") {
        return Line::Merging(None);
    }
    Line::Status(match line {
        l if l.starts_with("[info]") && l.contains("format(s)") => "Подготовка загрузки…",
        l if l.starts_with("[info]") && l.contains("thumbnail") => "Загрузка превью…",
        l if l.starts_with("[ThumbnailsConvertor]") => "Обработка превью…",
        l if l.starts_with("[ExtractAudio]") => "Конвертация аудио…",
        l if l.starts_with("[VideoConvertor]") || l.starts_with("[VideoRemuxer]") => "Конвертация видео…",
        l if l.starts_with("[Fixup") => "Исправление контейнера…",
        l if l.starts_with("[Metadata]") => "Запись метаданных…",
        l if l.starts_with("[MoveFiles]") => "Перемещение файлов…",
        l if EXTRACTOR.is_match(l) => "Получение информации о видео…",
        _ => return Line::Other,
    })
}

/// Разбирает строку `--progress-template`:
/// status|downloaded|total|total_estimate|speed|eta|vcodec. Неизвестные
/// значения yt-dlp печатает как "NA".
fn parse_template_progress(s: &str) -> Option<DownloadProgress> {
    let parts: Vec<&str> = s.trim().split('|').collect();
    if parts.len() < 7 || parts[0] != "downloading" {
        return None;
    }
    let num = |i: usize| parts[i].parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0);
    let downloaded = num(1).unwrap_or(0.0);
    let total = num(2).or(num(3)).filter(|t| *t > 0.0);
    let percent = total.map(|t| (downloaded / t * 100.0).clamp(0.0, 100.0));
    let size = Some(match total {
        Some(t) => format!("{} / {}", format::bytes(downloaded), format::bytes(t)),
        None => format::bytes(downloaded),
    });
    let stream = match parts[6] {
        "none" => Some(Stream::Audio),
        "NA" | "" => None,
        _ => Some(Stream::Video),
    };
    Some(DownloadProgress {
        percent,
        size,
        speed: num(4).map(|v| format!("{}/s", format::bytes(v))),
        eta: num(5).map(|v| format::eta(v as u64)),
        stream,
        connections: None,
    })
}

/// Скачивает одно видео. Блокирует поток до завершения; промежуточные
/// события отдаются в `on_progress` (не чаще ~10 раз в секунду).
///
/// При нескольких соединениях дорожки качает сам Lader (`fetch`), а yt-dlp
/// затем только склеивает их и делает остальную обработку. Если ускоренный
/// путь не подходит или сорвался, работает обычная загрузка yt-dlp.
pub fn download(
    tools: &Tools,
    opts: &DownloadOptions,
    cancel: &CancelToken,
    on_progress: &mut dyn FnMut(Progress),
) -> Outcome {
    let out_dir = &opts.out_dir;
    // Подпапка для превью; для mp4/mp3/m4a после встраивания она опустеет и удалится.
    let _ = fs::create_dir_all(out_dir.join(postprocess::THUMBS_DIR));

    if opts.connections > 1 {
        match accelerate(tools, opts, cancel, on_progress) {
            Accelerated::Ready(info) => {
                let outcome = run_ytdlp_download(tools, opts, Mode::Finish(&info), cancel, on_progress);
                let _ = fs::remove_file(&info);
                return outcome;
            }
            Accelerated::Cancelled(first_part) => {
                postprocess::cleanup_partials(out_dir, first_part.as_deref());
                return Outcome::Cancelled;
            }
            Accelerated::Fallback(reason) => on_progress(Progress::Status(format!(
                "Многопоточная загрузка не удалась ({}), качаем обычным способом…",
                first_line(&reason)
            ))),
            Accelerated::NotApplicable => {}
        }
    }
    run_ytdlp_download(tools, opts, Mode::Download, cancel, on_progress)
}

/// Чем закончилась попытка скачать дорожки самим.
enum Accelerated {
    /// Дорожки скачаны; описание видео для yt-dlp — в этом файле.
    Ready(PathBuf),
    /// Не подходит (не прямые ссылки, уже скачано и т.п.) — пусть качает yt-dlp.
    NotApplicable,
    /// Сорвалось — пусть качает yt-dlp; готовые дорожки он подхватит.
    Fallback(String),
    Cancelled(Option<PathBuf>),
}

/// Дорожка, которую качаем сами.
#[derive(Debug, PartialEq)]
struct Part {
    path: PathBuf,
    request: fetch::Request,
    stream: Stream,
}

fn accelerate(tools: &Tools, opts: &DownloadOptions, cancel: &CancelToken, on_progress: &mut dyn FnMut(Progress)) -> Accelerated {
    on_progress(Progress::Status("Получение информации о видео…".into()));
    let mut cmd = tools.ytdlp_command();
    cmd.args(args(opts, Mode::Probe));
    let out = match process::run(cmd, PROBE_TIMEOUT, Some(cancel)) {
        Ok(out) if out.status.success() => out,
        Err(RunError::Cancelled) => return Accelerated::Cancelled(None),
        // Ошибку (недоступное видео и т.п.) внятно покажет обычная загрузка.
        _ => return Accelerated::NotApplicable,
    };
    let text = process::decode(&out.stdout);
    // Видео из архива yt-dlp пропускает молча — JSON не будет.
    let Some(json) = text.lines().find(|l| l.trim_start().starts_with('{')) else { return Accelerated::NotApplicable };
    let Ok(info) = serde_json::from_str::<serde_json::Value>(json) else { return Accelerated::NotApplicable };
    let Some(parts) = plan_parts(&info) else { return Accelerated::NotApplicable };
    if final_exists(&info, opts.target) {
        // «Уже скачано» скажет обычная загрузка.
        return Accelerated::NotApplicable;
    }

    let first = parts.first().map(|p| p.path.clone());
    let options = fetch::Options::with_connections(opts.connections);
    for part in &parts {
        if part.path.is_file() {
            continue; // дорожка осталась от прошлой попытки целиком
        }
        let mut report = |p: &fetch::Progress| on_progress(Progress::Download(fetch_progress(p, part.stream)));
        if let Err(e) = fetch::download_to_file(&part.request, &part.path, options, cancel, &mut report) {
            if cancel.is_cancelled() {
                return Accelerated::Cancelled(first);
            }
            return Accelerated::Fallback(e);
        }
    }

    let info_path = std::env::temp_dir().join(format!("lader-info-{}-{}.json", std::process::id(), unique_suffix()));
    match fs::write(&info_path, json) {
        Ok(()) => Accelerated::Ready(info_path),
        Err(e) => Accelerated::Fallback(e.to_string()),
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

fn unique_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn fetch_progress(p: &fetch::Progress, stream: Stream) -> DownloadProgress {
    let percent = p.total.filter(|t| *t > 0).map(|t| (p.done as f64 / t as f64 * 100.0).clamp(0.0, 100.0));
    let size = Some(match p.total {
        Some(t) => format!("{} / {}", format::bytes(p.done as f64), format::bytes(t as f64)),
        None => format::bytes(p.done as f64),
    });
    let eta = match p.total {
        Some(t) if p.speed > 1.0 => Some(format::eta(((t.saturating_sub(p.done)) as f64 / p.speed) as u64)),
        _ => None,
    };
    DownloadProgress {
        percent,
        size,
        speed: (p.speed > 0.0).then(|| format!("{}/s", format::bytes(p.speed))),
        eta,
        stream: Some(stream),
        connections: Some(p.connections),
    }
}

/// Какие файлы и откуда качать, по выводу `yt-dlp -j`. None — ускорять
/// нечего: например, видео нарезано на фрагменты (HLS), их yt-dlp сам
/// качает в несколько потоков.
fn plan_parts(info: &serde_json::Value) -> Option<Vec<Part>> {
    let filename = info["filename"].as_str()?;
    let info_ext = info["ext"].as_str().unwrap_or("");
    let requested = info["requested_formats"].as_array().filter(|list| !list.is_empty());
    let formats: Vec<&serde_json::Value> = match requested {
        Some(list) => list.iter().collect(),
        None => vec![info],
    };
    formats
        .into_iter()
        .map(|f| {
            if !matches!(f["protocol"].as_str()?, "https" | "http") {
                return None;
            }
            let path = match requested {
                Some(_) => part_path(filename, info_ext, f["format_id"].as_str()?, f["ext"].as_str()?),
                None => PathBuf::from(filename),
            };
            let mut headers: Vec<(String, String)> = f["http_headers"]
                .as_object()
                .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
                .unwrap_or_default();
            if let Some(cookie) = f["cookies"].as_str().and_then(cookie_header) {
                headers.push(("Cookie".into(), cookie));
            }
            let stream = if f["vcodec"].as_str() == Some("none") { Stream::Audio } else { Stream::Video };
            Some(Part { path, request: fetch::Request { url: f["url"].as_str()?.to_string(), headers }, stream })
        })
        .collect()
}

/// Имя дорожки до склейки — ровно как его строит yt-dlp
/// (YoutubeDL.process_info): "Видео.mp4" + формат 140 (m4a) → "Видео.f140.m4a".
fn part_path(filename: &str, info_ext: &str, format_id: &str, format_ext: &str) -> PathBuf {
    let path = Path::new(filename);
    let stem = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext == info_ext => path.with_extension("").to_string_lossy().into_owned(),
        _ => filename.to_string(),
    };
    PathBuf::from(format!("{stem}.f{format_id}.{format_ext}"))
}

/// Итоговый файл уже есть — тогда качать дорожки незачем.
fn final_exists(info: &serde_json::Value, target: Target) -> bool {
    let Some(filename) = info["filename"].as_str() else { return false };
    let path = Path::new(filename);
    let final_ext = match target {
        Target::Audio(AudioFormat::Mp3) => "mp3",
        Target::Audio(AudioFormat::M4a) => "m4a",
        Target::Video { container: VideoContainer::Mp4, .. } => "mp4",
        Target::Video { container: VideoContainer::Webm, .. } => "webm",
        Target::Video { container: VideoContainer::Mkv, .. } => "mkv",
    };
    path.is_file() || path.with_extension(final_ext).is_file()
}

/// Cookies формата из вывода yt-dlp ("name=value; Domain=…; Path=/; Secure")
/// → значение заголовка Cookie ("name=value; name2=value2").
fn cookie_header(cookies: &str) -> Option<String> {
    const ATTRS: [&str; 8] = ["domain", "path", "secure", "expires", "version", "max-age", "httponly", "samesite"];
    let pairs: Vec<&str> = cookies
        .split(';')
        .map(str::trim)
        .filter(|item| {
            let name = item.split('=').next().unwrap_or("").trim().to_ascii_lowercase();
            item.contains('=') && !ATTRS.contains(&name.as_str())
        })
        .collect();
    (!pairs.is_empty()).then(|| pairs.join("; "))
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

/// Запускает yt-dlp и разбирает его вывод: прогресс, склейка, итоговый файл.
fn run_ytdlp_download(
    tools: &Tools,
    opts: &DownloadOptions,
    mode: Mode,
    cancel: &CancelToken,
    on_progress: &mut dyn FnMut(Progress),
) -> Outcome {
    let out_dir = &opts.out_dir;
    // После ускоренной загрузки yt-dlp отчитается «уже скачано» о наших же
    // дорожках — это не повод писать пользователю «Уже скачано».
    let prefetched = matches!(mode, Mode::Finish(_));
    let mut cmd = tools.ytdlp_command();
    cmd.args(args(opts, mode)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(format!("не удалось запустить yt-dlp: {e}")),
    };
    cancel.attach(child.id());

    let stderr = child.stderr.take();
    let stderr_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stderr {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });

    let mut last_file: Option<PathBuf> = None;
    let mut final_file: Option<PathBuf> = None;
    let mut already = false;
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    let mut last_status = "";

    if let Some(stdout) = child.stdout.take() {
        for raw in BufReader::new(stdout).split(b'\n') {
            let Ok(raw) = raw else { break };
            let line = process::decode(&raw);
            let line = line.trim_end_matches(['\r', '\n']);
            match parse_line(line) {
                Line::Progress(p) => {
                    let finished = p.percent.is_some_and(|v| v >= 100.0);
                    if finished || last_emit.elapsed() >= Duration::from_millis(100) {
                        last_emit = Instant::now();
                        on_progress(Progress::Download(p));
                    }
                }
                Line::FinalFile(path) => final_file = Some(path),
                Line::Destination(path) => last_file = Some(path),
                Line::AlreadyDownloaded(path) => {
                    already |= !(prefetched && path.is_some());
                    if path.is_some() {
                        last_file = path;
                    }
                }
                Line::Merging(path) => {
                    if path.is_some() {
                        last_file = path;
                    }
                    on_progress(Progress::Merging);
                }
                Line::Status(text) if text != last_status => {
                    last_status = text;
                    on_progress(Progress::Status(text.to_string()));
                }
                Line::Status(_) | Line::Other => {}
            }
        }
    }

    let status = child.wait();
    cancel.detach();
    let stderr = process::decode(&stderr_reader.join().unwrap_or_default());

    if cancel.is_cancelled() {
        // Убираем недокачанные .part/.ytdl, промежуточные fNNN-файлы и
        // временные обложки, чтобы папка осталась чистой.
        postprocess::cleanup_partials(out_dir, final_file.as_deref().or(last_file.as_deref()));
        return Outcome::Cancelled;
    }
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            let msg = stderr.trim();
            return Outcome::Failed(if msg.is_empty() { format!("yt-dlp завершился с ошибкой ({s})") } else { msg.to_string() });
        }
        Err(e) => return Outcome::Failed(format!("ошибка ожидания yt-dlp: {e}")),
    }

    let file = final_file.or_else(|| last_file.map(|f| resolve_final_file(&f)));

    on_progress(Progress::Status("Встраивание обложки…".into()));
    postprocess::embed_thumbnails_in_dir(&tools.ffmpeg(), out_dir);

    Outcome::Done { file: file.filter(|f| f.exists()), already_downloaded: already }
}

/// Если известен только промежуточный файл ("Video.f137.mp4"), ищем итоговый
/// рядом: без суффикса формата, с любым из итоговых расширений.
fn resolve_final_file(path: &Path) -> PathBuf {
    if path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let base = path.with_file_name(postprocess::strip_format_suffix(&stem));
    ["mp4", "webm", "mkv", "mp3", "m4a", "opus"]
        .iter()
        .map(|ext| PathBuf::from(format!("{}.{ext}", base.display())))
        .find(|p| p.exists())
        .unwrap_or_else(|| path.to_path_buf())
}

// ─── Пакетная загрузка ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum BatchEvent {
    Started { index: usize, total: usize, title: String },
    Progress(Progress),
    VideoDone { index: usize, already_downloaded: bool },
    VideoFailed { index: usize, error: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchSummary {
    pub total: usize,
    /// Индексы видео с ошибками и тексты ошибок.
    pub failed: Vec<(usize, String)>,
    pub cancelled: bool,
}

/// Последовательно скачивает видео. Ошибка одного видео не прерывает
/// остальные; отмена прерывает весь пакет.
pub fn download_batch(
    tools: &Tools,
    entries: &[ChannelEntry],
    opts: &DownloadOptions,
    cancel: &CancelToken,
    on_event: &mut dyn FnMut(BatchEvent),
) -> BatchSummary {
    let total = entries.len();
    let mut summary = BatchSummary { total, failed: Vec::new(), cancelled: false };
    for (index, entry) in entries.iter().enumerate() {
        if cancel.is_cancelled() {
            summary.cancelled = true;
            return summary;
        }
        on_event(BatchEvent::Started { index, total, title: entry.title.clone() });

        // Быстрый пропуск по архиву: не запускаем yt-dlp вовсе и экономим
        // секунды на извлечении метаданных каждого видео.
        if is_in_archive(&opts.out_dir, &entry.id) {
            on_event(BatchEvent::VideoDone { index, already_downloaded: true });
            continue;
        }

        let video_opts = DownloadOptions { url: entry.url.clone(), ..opts.clone() };
        match download(tools, &video_opts, cancel, &mut |p| on_event(BatchEvent::Progress(p))) {
            Outcome::Done { already_downloaded, .. } => on_event(BatchEvent::VideoDone { index, already_downloaded }),
            Outcome::Failed(error) => {
                on_event(BatchEvent::VideoFailed { index, error: error.clone() });
                summary.failed.push((index, error));
            }
            Outcome::Cancelled => {
                summary.cancelled = true;
                return summary;
            }
        }
    }
    summary
}

// ─── Вспомогательное ─────────────────────────────────────────────────────────

fn run_ytdlp(cmd: Command, timeout: Duration) -> Result<Vec<u8>, String> {
    match process::run(cmd, timeout, None) {
        Ok(out) if out.status.success() => Ok(out.stdout),
        Ok(out) => {
            let msg = process::decode(&out.stderr).trim().to_string();
            Err(if msg.is_empty() { format!("yt-dlp завершился с ошибкой ({})", out.status) } else { msg })
        }
        Err(RunError::Spawn(e)) => Err(format!("не удалось запустить yt-dlp: {e}")),
        Err(RunError::TimedOut) => Err(format!("yt-dlp не ответил за {} с", timeout.as_secs())),
        Err(RunError::Cancelled) => Err("операция отменена".into()),
    }
}

static REPORT_BOILERPLATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)[;.]?\s*please report this issue on.*?yt-dlp\s+-U|\s*Confirm you are on the latest version using\s+yt-dlp\s+-U").unwrap()
});

/// Убирает из ошибки yt-dlp дежурное «please report this issue… Confirm you
/// are on the latest version using yt-dlp -U»: про обновление приложение
/// сообщает само и предлагает кнопку, а запускать `yt-dlp -U` в терминале
/// пользователю не нужно.
pub fn clean_error(msg: &str) -> String {
    REPORT_BOILERPLATE.replace_all(msg, "").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_template_progress() {
        let line = "[ytdl-progress] downloading|1024|433081|NA|9071.38|47|avc1.4D400C";
        let Line::Progress(p) = parse_line(line) else { panic!("not progress") };
        assert!((p.percent.unwrap() - 0.2364).abs() < 0.001);
        assert_eq!(p.size.as_deref(), Some("1.00KiB / 422.93KiB"));
        assert_eq!(p.speed.as_deref(), Some("8.86KiB/s"));
        assert_eq!(p.eta.as_deref(), Some("00:47"));
        assert_eq!(p.stream, Some(Stream::Video));

        let Line::Progress(p) = parse_line("[ytdl-progress] downloading|2048|NA|4096|NA|NA|none") else {
            panic!("not progress")
        };
        assert_eq!(p.percent, Some(50.0));
        assert_eq!(p.stream, Some(Stream::Audio));
        assert_eq!(p.speed, None);

        assert_eq!(parse_line("[ytdl-progress] finished|433081|433081|NA|NA|NA|avc1"), Line::Other);
    }

    #[test]
    fn parses_legacy_progress() {
        let Line::Progress(p) = parse_line("[download]  45.3% of ~ 10.00MiB at  1.00MiB/s ETA 00:10 (frag 3/9)") else {
            panic!("not progress")
        };
        assert_eq!(p.percent, Some(45.3));
        assert_eq!(p.size.as_deref(), Some("10.00MiB"));
        assert_eq!(p.speed.as_deref(), Some("1.00MiB/s"));
        assert_eq!(p.eta.as_deref(), Some("00:10"));
    }

    #[test]
    fn parses_service_lines() {
        assert_eq!(
            parse_line("[ytdl-file] C:\\Video\\Me at the zoo.mp4"),
            Line::FinalFile(PathBuf::from("C:\\Video\\Me at the zoo.mp4"))
        );
        assert_eq!(
            parse_line("[download] Destination: t2\\Me at the zoo.f133.mp4"),
            Line::Destination(PathBuf::from("t2\\Me at the zoo.f133.mp4"))
        );
        assert_eq!(parse_line("[download] jNQXAC9IVRw: has already been recorded in the archive"), Line::AlreadyDownloaded(None));
        assert_eq!(
            parse_line("[download] C:\\v\\Clip.mp4 has already been downloaded"),
            Line::AlreadyDownloaded(Some(PathBuf::from("C:\\v\\Clip.mp4")))
        );
        assert_eq!(
            parse_line("[Merger] Merging formats into \"t2\\Me at the zoo.mp4\""),
            Line::Merging(Some(PathBuf::from("t2\\Me at the zoo.mp4")))
        );
        assert_eq!(parse_line("[youtube] jNQXAC9IVRw: Downloading webpage"), Line::Status("Получение информации о видео…"));
        assert_eq!(parse_line("[youtube] Extracting URL: https://youtu.be/x"), Line::Status("Получение информации о видео…"));
        assert_eq!(parse_line("[info] jNQXAC9IVRw: Downloading 1 format(s): 133+140"), Line::Status("Подготовка загрузки…"));
        assert_eq!(parse_line("[ExtractAudio] Destination: a.mp3"), Line::Status("Конвертация аудио…"));
        assert_eq!(parse_line("[hlsnative] Downloading m3u8 manifest"), Line::Other);
        assert_eq!(parse_line("Deleting original file x.webp (pass -k to keep)"), Line::Other);
    }

    fn opts(target: Target) -> DownloadOptions {
        DownloadOptions {
            url: "https://youtu.be/x".into(),
            target,
            out_dir: PathBuf::from("out"),
            cookies: Cookies::None,
            connections: 8,
        }
    }

    /// Настоящая загрузка с YouTube обычным способом и в 8 потоков: склейка,
    /// обложка и архив должны получиться одинаковыми. Ходит в сеть и качает
    /// ~60 МБ: `cargo test downloads_youtube -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn downloads_youtube_in_one_and_many_connections() {
        use crate::components::{self, Component};
        let path = |c| components::find(c).map(|i| i.path);
        let tools = Tools {
            ytdlp: path(Component::YtDlp).expect("yt-dlp"),
            ffmpeg: path(Component::Ffmpeg),
            deno: path(Component::Deno),
        };
        for connections in [8, 1] {
            let dir = std::env::temp_dir().join(format!("lader-e2e-{connections}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            let opts = DownloadOptions {
                url: "https://www.youtube.com/watch?v=dLw6aqKbpsg".into(),
                target: Target::Video { max_height: 360, container: VideoContainer::Mp4 },
                out_dir: dir.clone(),
                cookies: Cookies::None,
                connections,
            };
            let started = Instant::now();
            let mut most = 0;
            let outcome = download(&tools, &opts, &CancelToken::new(), &mut |p| {
                if let Progress::Download(d) = p {
                    most = most.max(d.connections.unwrap_or(1));
                }
            });
            let elapsed = started.elapsed();
            let Outcome::Done { file: Some(file), already_downloaded: false } = &outcome else {
                panic!("{connections} потоков: {outcome:?}")
            };
            let size = fs::metadata(file).unwrap().len();
            println!(
                "потоков {connections}: {:.1} с, {:.1} МБ, {:.0} КБ/с, одновременно соединений до {most}",
                elapsed.as_secs_f64(),
                size as f64 / 1048576.0,
                size as f64 / 1024.0 / elapsed.as_secs_f64()
            );
            assert!(file.extension().is_some_and(|e| e == "mp4"));
            let left: Vec<String> = fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.contains(".f1") || n.ends_with("part"))
                .collect();
            assert!(left.is_empty(), "остались промежуточные файлы: {left:?}");
            assert!(is_in_archive(&dir, "dLw6aqKbpsg"));
            if connections > 1 {
                assert!(most > 1, "качали в одно соединение");
            }
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn probe_args_only_select_formats() {
        let o = opts(Target::Video { max_height: 1080, container: VideoContainer::Mp4 });
        let probe = args(&o, Mode::Probe);
        assert!(probe.contains(&"-j".to_string()));
        // --no-quiet/--print перемешали бы JSON с сообщениями.
        assert!(!probe.contains(&"--no-quiet".to_string()) && !probe.contains(&"--print".to_string()));
        assert!(probe.windows(2).any(|w| w == ["--concurrent-fragments", "8"]));
        assert_eq!(&probe[probe.len() - 2..], ["--", "https://youtu.be/x"]);

        let finish = args(&o, Mode::Finish(Path::new("info.json")));
        assert!(finish.windows(2).any(|w| w == ["--load-info-json", "info.json"]));
        assert!(!finish.contains(&"https://youtu.be/x".to_string()));
        assert!(finish.contains(&"--no-quiet".to_string()));
    }

    #[test]
    fn names_parts_like_ytdlp() {
        assert_eq!(part_path("C:\\v\\Клип.mp4", "mp4", "137", "mp4"), PathBuf::from("C:\\v\\Клип.f137.mp4"));
        assert_eq!(part_path("/v/Клип.mp4", "mp4", "140", "m4a"), PathBuf::from("/v/Клип.f140.m4a"));
        assert_eq!(part_path("/v/a.b", "mkv", "1", "webm"), PathBuf::from("/v/a.b.f1.webm"));
    }

    #[test]
    fn builds_cookie_header() {
        assert_eq!(
            cookie_header("auth=abc; Domain=.x.com; Path=/; Secure; Expires=1900000000; ct0=def; Domain=.x.com").as_deref(),
            Some("auth=abc; ct0=def")
        );
        assert_eq!(cookie_header("Domain=.x.com; Secure"), None);
    }

    #[test]
    fn plans_parts_from_probe_json() {
        let json = r#"{"filename":"/v/Клип.mp4","ext":"mp4","requested_formats":[
            {"format_id":"137","ext":"mp4","protocol":"https","url":"https://g/v","vcodec":"avc1","http_headers":{"User-Agent":"UA"}},
            {"format_id":"140","ext":"m4a","protocol":"https","url":"https://g/a","vcodec":"none","cookies":"k=v; Path=/"}]}"#;
        let parts = plan_parts(&serde_json::from_str(json).unwrap()).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].path, PathBuf::from("/v/Клип.f137.mp4"));
        assert_eq!(parts[0].stream, Stream::Video);
        assert_eq!(parts[0].request.headers, vec![("User-Agent".to_string(), "UA".to_string())]);
        assert_eq!(parts[1].path, PathBuf::from("/v/Клип.f140.m4a"));
        assert_eq!(parts[1].stream, Stream::Audio);
        assert_eq!(parts[1].request.headers, vec![("Cookie".to_string(), "k=v".to_string())]);

        // Один файл без склейки (звук для MP3) — качается под именем из yt-dlp.
        let single = r#"{"filename":"/v/Песня.m4a","ext":"m4a","protocol":"https","url":"https://g/a","vcodec":"none"}"#;
        let parts = plan_parts(&serde_json::from_str(single).unwrap()).unwrap();
        assert_eq!(parts[0].path, PathBuf::from("/v/Песня.m4a"));

        // Фрагменты (HLS у X) yt-dlp качает сам — ускорять нечего.
        let hls = r#"{"filename":"/v/t.mp4","ext":"mp4","protocol":"m3u8_native","url":"https://x/p.m3u8"}"#;
        assert!(plan_parts(&serde_json::from_str(hls).unwrap()).is_none());
    }

    #[test]
    fn mp4_args_prefer_h264_and_aac() {
        let args = build_args(&opts(Target::Video { max_height: 720, container: VideoContainer::Mp4 }));
        let f = &args[args.iter().position(|a| a == "-f").unwrap() + 1];
        assert!(f.starts_with("bestvideo[height<=720][vcodec^=avc1]+bestaudio[ext=m4a]/"));
        assert!(f.ends_with("/best[height<=720]"));
        assert!(args.windows(2).any(|w| w == ["--merge-output-format", "mp4"]));
        assert!(args.contains(&"--no-quiet".to_string()));
        // Папка — через -P, шаблоны имён — относительные (см. --trim-filenames).
        assert!(args.windows(2).any(|w| w == ["-P", "out"]));
        assert!(args.windows(2).any(|w| w == ["-o", "%(title)s.%(ext)s"]));
        assert!(args.windows(2).any(|w| w == ["-o", "thumbnail:_thumbs/%(title)s.%(ext)s"]));
        assert_eq!(&args[args.len() - 2..], ["--", "https://youtu.be/x"]);
        assert!(!args.contains(&"--cookies-from-browser".to_string()));
    }

    #[test]
    fn audio_args_and_cookies() {
        let mut o = opts(Target::Audio(AudioFormat::Mp3));
        o.cookies = Cookies::from_browser("firefox");
        let args = build_args(&o);
        assert!(args.windows(2).any(|w| w == ["--audio-format", "mp3"]));
        assert!(args.windows(2).any(|w| w == ["--audio-quality", "0"]));
        assert!(args.windows(2).any(|w| w == ["--cookies-from-browser", "firefox"]));
        assert!(!args.contains(&"--merge-output-format".to_string()));

        o.cookies = Cookies::File(PathBuf::from("c.txt"));
        let args = build_args(&o);
        assert!(args.windows(2).any(|w| w == ["--cookies", "c.txt"]));
        assert!(!args.contains(&"--cookies-from-browser".to_string()));
        assert!(Cookies::from_browser("").is_none());
    }

    #[test]
    fn parses_video_info_with_qualities() {
        let json = r#"{"title":"Клип","thumbnail":"https://i.ytimg.com/x.webp","duration":212.5,"uploader":null,"channel":"Канал",
            "formats":[{"vcodec":"none","height":null},{"vcodec":"avc1","height":1080,"fps":30},
            {"vcodec":"vp9","height":1080,"fps":60},{"vcodec":"av01","height":2160,"fps":24},{"vcodec":"avc1","height":360}]}"#;
        let info = parse_video_info(json.as_bytes()).unwrap();
        assert_eq!(info.title, "Клип");
        assert_eq!(info.uploader, "Канал");
        assert_eq!(info.duration, 212);
        let labels: Vec<_> = info.qualities.iter().map(|q| q.label.as_str()).collect();
        assert_eq!(labels, ["4K (2160p)", "1080p 60fps", "360p"]);
    }

    #[test]
    fn multi_video_tweet_uses_first_entry() {
        let json = br#"{"title":"tweet","uploader":"user","entries":[null,{"formats":[{"vcodec":"avc1","height":720}]}]}"#;
        let info = parse_video_info(json).unwrap();
        assert_eq!(info.title, "tweet");
        assert_eq!(info.uploader, "user");
        assert_eq!(info.qualities.len(), 1);
    }

    #[test]
    fn parses_channel_entries() {
        let json = br#"{"title":"MrBeast - Videos","channel":"MrBeast","entries":[
            {"_type":"url","id":"dQw4w9WgXcQ","url":"https://www.youtube.com/watch?v=dQw4w9WgXcQ","title":"A","duration":61},
            {"_type":"playlist","id":"UCX6OQ3DkcsbYNE6H8uQQuVA","url":"https://www.youtube.com/@MrBeast/shorts","title":"Shorts"}]}"#;
        let info = parse_channel(json, true).unwrap();
        assert_eq!(info.uploader, "MrBeast");
        assert_eq!(info.entries[0].url, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(info.entries[0].thumbnail, "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg");
        assert_eq!(info.entries[1].url, "https://www.youtube.com/@MrBeast/shorts");
    }

    #[test]
    fn archive_lookup() {
        let dir = std::env::temp_dir().join(format!("ytdl-archive-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(ARCHIVE_FILE), "youtube jNQXAC9IVRw\ntwitter 1234567890\n").unwrap();
        assert!(is_in_archive(&dir, "jNQXAC9IVRw"));
        assert!(is_in_archive(&dir, "1234567890"));
        assert!(!is_in_archive(&dir, "jNQXAC9IVR"));
        assert!(!is_in_archive(&dir, ""));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn strips_report_boilerplate() {
        let msg = "ERROR: [youtube] abc: Unable to extract uploader id; please report this issue on  https://github.com/yt-dlp/yt-dlp/issues?q= , filling out the appropriate issue template. Confirm you are on the latest version using  yt-dlp -U";
        assert_eq!(clean_error(msg), "ERROR: [youtube] abc: Unable to extract uploader id");
        assert_eq!(clean_error("ERROR: Video unavailable"), "ERROR: Video unavailable");
    }
}
