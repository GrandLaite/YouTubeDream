//! Основной цикл помощника: команды расширения и состояние компонентов.
//!
//! Расширение шлёт `{"id": 1, "cmd": "info", …}` и получает ответ
//! `{"reply": 1, "ok": true, "data": …}` либо `{"reply": 1, "ok": false,
//! "error": "…"}`. Без запроса помощник шлёт события `components` (состояние
//! yt-dlp, Deno и ffmpeg) и `job` (ход загрузки).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use lader_core::cancel::CancelToken;
use lader_core::components::{self, Component, Installed, Stage};
use lader_core::settings::{self, CONNECTION_CHOICES, COOKIE_BROWSERS, Settings};
use lader_core::ytdlp::{self, ChannelEntry, ChannelInfo, PAGE_SIZE, Tools, VideoInfo};
use lader_core::{format, process, urls};
use serde_json::{Value, json};

use crate::cookies::{self, Spec};
use crate::jobs::{self, Cancel, JobSpec, Queue, View};
use crate::protocol::{self, Out, fail};
use crate::register;

/// Ответ на команду: данные или ошибка `{"error": "…", …}`.
type Reply = Result<Value, Value>;

pub struct Shared {
    pub out: Out,
    pub queue: Queue,
    comp: Mutex<Components>,
    /// Будит тех, кто ждёт окончания поиска компонентов.
    comp_cv: Condvar,
    /// Окно браузера, вызвавшего помощник (на Windows): диалог выбора
    /// папки открывается поверх него, а не где-то за ним.
    parent_window: Option<isize>,
}

#[derive(Default)]
struct Components {
    detected: bool,
    checking: bool,
    found: BTreeMap<Component, Installed>,
    latest: Option<String>,
    check_error: Option<String>,
    install: Option<Install>,
    outcome: Option<InstallOutcome>,
}

/// Идущая установка, по одному компоненту по порядку.
struct Install {
    order: Vec<Component>,
    index: usize,
    stage: Option<Stage>,
    failed: BTreeMap<Component, String>,
    cancel: CancelToken,
    /// Обновление уже работающего yt-dlp, а не установка недостающего.
    update: bool,
}

enum InstallOutcome {
    Done { update: bool },
    Failed { failed: BTreeMap<Component, String>, cancelled: bool },
}

impl Components {
    fn missing(&self) -> Vec<Component> {
        Component::ALL.into_iter().filter(|c| !self.found.contains_key(c)).collect()
    }

    fn update_available(&self) -> bool {
        matches!(
            (self.found.get(&Component::YtDlp), &self.latest),
            (Some(i), Some(l)) if components::is_newer(l, &i.version)
        )
    }

    fn set_latest(&mut self, version: String) {
        if self.latest.as_deref().is_none_or(|known| components::is_newer(&version, known)) {
            self.latest = Some(version);
        }
    }
}

pub fn run(parent_window: Option<isize>) {
    let shared = Arc::new(Shared {
        out: Out::new(std::io::stdout()),
        queue: Queue::default(),
        comp: Mutex::new(Components::default()),
        comp_cv: Condvar::new(),
        parent_window,
    });
    cookies::remove_stale();
    register::remove_old_copies();
    jobs::spawn_worker(shared.clone());
    detect(&shared);

    let mut stdin = std::io::stdin().lock();
    loop {
        match protocol::read_message(&mut stdin) {
            Ok(Some(msg)) => handle(&shared, msg),
            Ok(None) => break,
            Err(e) => {
                eprintln!("lader-host: {e}");
                break;
            }
        }
    }

    // Браузер закрыл соединение (закрыт браузер или отключено расширение):
    // прерываем загрузку и установку, недокачанное уберёт сама отмена.
    shared.queue.shutdown();
    if let Some(inst) = &shared.lock().install {
        inst.cancel.cancel();
    }
    shared.queue.wait_idle(Duration::from_secs(10));
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Components> {
        self.comp.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Программы для yt-dlp. Ждёт окончания поиска компонентов при запуске.
    pub fn tools(&self) -> Result<Tools, Value> {
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut c = self.lock();
        while !c.detected && Instant::now() < deadline {
            c = self.comp_cv.wait_timeout(c, Duration::from_millis(500)).unwrap_or_else(|e| e.into_inner()).0;
        }
        if c.install.is_some() {
            return Err(
                json!({ "code": "installing", "error": "Идёт установка компонентов. Дождитесь её окончания." }),
            );
        }
        let missing = c.missing();
        if !missing.is_empty() {
            return Err(
                json!({ "code": "components", "error": format!("Не хватает компонентов: {}", names(&missing)) }),
            );
        }
        let path = |k| c.found.get(&k).map(|i: &Installed| i.path.clone());
        Ok(Tools {
            ytdlp: path(Component::YtDlp).expect("проверено выше"),
            ffmpeg: path(Component::Ffmpeg),
            deno: path(Component::Deno),
        })
    }

    pub fn emit_components(&self) {
        let snapshot = snapshot(&self.lock());
        self.out.event("components", snapshot);
    }

    /// Ошибка похожа на устаревший yt-dlp. Сразу сверяемся с GitHub,
    /// чтобы расширение могло предложить обновление.
    pub fn refresh_latest_if_outdated(&self, error: &str) {
        if !urls::looks_outdated(error) {
            return;
        }
        if let Ok(version) = components::latest_ytdlp_version() {
            self.lock().set_latest(version);
            self.emit_components();
        }
    }
}

// ─── Команды ─────────────────────────────────────────────────────────────────

fn handle(shared: &Arc<Shared>, msg: Value) {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let cmd = msg["cmd"].as_str().unwrap_or("").to_string();
    // Долгие команды (yt-dlp, сеть, диалог) идут в своих потоках, чтобы
    // отмена и другие запросы не ждали их окончания.
    let long: Option<fn(&Shared, &Value) -> Reply> = match cmd.as_str() {
        "info" => Some(info),
        "more" => Some(more),
        "browse" => Some(browse),
        _ => None,
    };
    if let Some(f) = long {
        let shared = shared.clone();
        thread::spawn(move || {
            let result = f(&shared, &msg);
            shared.out.reply(&id, result);
        });
        return;
    }
    let result = match cmd.as_str() {
        "hello" => Ok(hello()),
        "components" => Ok(snapshot(&shared.lock())),
        "check" => check(shared, msg["user"].as_bool().unwrap_or(false)),
        "install" => install(shared, &msg),
        "cancel_install" => {
            if let Some(inst) = &shared.lock().install {
                let token = inst.cancel.clone();
                thread::spawn(move || token.cancel());
            }
            Ok(Value::Null)
        }
        "open_components_folder" => {
            if let Some(dir) = components::bin_dir() {
                let _ = std::fs::create_dir_all(&dir);
                process::open_in_file_manager(&dir, None);
            }
            Ok(Value::Null)
        }
        "channel_folder" => Ok(channel_folder(msg["base"].as_str().unwrap_or(""), msg["name"].as_str().unwrap_or(""))),
        "mkdir" => mkdir(msg["path"].as_str().unwrap_or("")),
        "enqueue" => enqueue(shared, &msg["job"]),
        "cancel" => Ok(cancel(shared, msg["job_id"].as_str().unwrap_or(""))),
        "open_folder" => {
            open_folder(msg["dir"].as_str().unwrap_or(""), msg["file"].as_str());
            Ok(Value::Null)
        }
        other => Err(fail(format!("неизвестная команда: {other}"))),
    };
    shared.out.reply(&id, result);
}

fn hello() -> Value {
    let defaults = Settings::load();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "defaults": { "out_dir": defaults.out_dir, "connections": defaults.connections },
        "connection_choices": CONNECTION_CHOICES,
        // Cookies Edge и Chrome yt-dlp прочитать не может (App-Bound
        // Encryption), их расширение передаёт само.
        "cookie_browsers": COOKIE_BROWSERS
            .iter()
            .filter(|(_, v)| !v.is_empty() && !matches!(*v, "edge" | "chrome"))
            .map(|(label, value)| json!({ "label": label, "value": value }))
            .collect::<Vec<_>>(),
        "page_size": PAGE_SIZE,
    })
}

fn cookies_of(msg: &Value) -> Result<Spec, Value> {
    Spec::from_json(&msg["cookies"]).map_err(fail)
}

fn info(shared: &Shared, msg: &Value) -> Reply {
    let url = msg["url"].as_str().unwrap_or("").trim().to_string();
    if url.is_empty() {
        return Err(json!({ "code": "empty", "title": "Введите ссылку",
            "error": "Вставьте ссылку на видео, плейлист или канал YouTube либо на твит X с видео." }));
    }
    // Лента профиля X не поддерживается yt-dlp. Говорим об этом сразу,
    // а не через невнятную ошибку «Unsupported URL».
    if urls::is_twitter_profile_url(&url) {
        return Err(json!({ "code": "twitter_profile", "title": "Это ссылка на профиль X",
            "error": "Выгрузка всей ленты не поддерживается — откройте нужный твит и вставьте ссылку вида x.com/username/status/…" }));
    }
    let tools = shared.tools()?;
    let (cookies, _guard) = cookies_of(msg)?.resolve().map_err(fail)?;
    if urls::is_channel_url(&url) {
        let list_url = urls::normalize_channel_url(&url);
        match ytdlp::get_channel_videos(&tools, &list_url, 0, PAGE_SIZE, &cookies) {
            Ok(info) => Ok(channel_json(&url, &list_url, info)),
            Err(e) => Err(ytdlp_error(shared, "Не удалось загрузить список видео", &e)),
        }
    } else {
        match ytdlp::get_video_info(&tools, &url, &cookies) {
            Ok(info) => Ok(video_json(&url, info)),
            Err(e) => Err(ytdlp_error(shared, "Не удалось получить информацию о видео", &e)),
        }
    }
}

fn more(shared: &Shared, msg: &Value) -> Reply {
    let list_url = msg["list_url"].as_str().unwrap_or("");
    let offset = msg["offset"].as_u64().unwrap_or(0) as usize;
    let tools = shared.tools()?;
    let (cookies, _guard) = cookies_of(msg)?.resolve().map_err(fail)?;
    match ytdlp::get_channel_videos(&tools, list_url, offset, PAGE_SIZE, &cookies) {
        Ok(info) => Ok(json!({
            "entries": info.entries.iter().map(entry_json).collect::<Vec<_>>(),
            "more": info.entries.len() >= PAGE_SIZE,
        })),
        Err(e) => Err(ytdlp_error(shared, "Не удалось загрузить ещё видео", &e)),
    }
}

/// Ошибка yt-dlp с признаками, по которым расширение подскажет выход:
/// включить cookies или обновить yt-dlp.
fn ytdlp_error(shared: &Shared, title: &str, raw: &str) -> Value {
    shared.refresh_latest_if_outdated(raw);
    json!({
        "title": title,
        "error": ytdlp::clean_error(raw),
        "auth": urls::needs_auth_error(raw),
        "outdated": urls::looks_outdated(raw),
    })
}

fn video_json(url: &str, info: VideoInfo) -> Value {
    json!({
        "kind": "video",
        "url": url,
        "title": info.title,
        "uploader": info.uploader,
        "duration": format::duration(info.duration),
        "thumbnail": info.thumbnail,
        "qualities": info.qualities.iter().map(|q| json!({ "height": q.height, "fps": q.fps, "label": q.label })).collect::<Vec<_>>(),
    })
}

fn entry_json(e: &ChannelEntry) -> Value {
    json!({
        "id": e.id,
        "url": e.url,
        "title": e.title,
        "duration": e.duration,
        "duration_text": format::duration(e.duration),
        "thumbnail": e.thumbnail,
    })
}

fn channel_json(url: &str, list_url: &str, info: ChannelInfo) -> Value {
    let title = if info.title.is_empty() { info.uploader.clone() } else { info.title.clone() };
    let folder = if info.uploader.is_empty() { info.title.clone() } else { info.uploader.clone() };
    json!({
        "kind": "channel",
        "url": url,
        "list_url": list_url,
        "title": title,
        "folder": folder,
        "more": info.entries.len() >= PAGE_SIZE,
        "entries": info.entries.iter().map(entry_json).collect::<Vec<_>>(),
    })
}

/// Предлагать ли подпапку с названием канала, как в приложении. База
/// всегда корневая папка, а если она уже названа как канал, подпапка не нужна.
fn channel_folder(base: &str, channel_name: &str) -> Value {
    let name = format::sanitize_folder_name(channel_name);
    let base = base.trim();
    if name == "channel" || base.is_empty() {
        return json!({ "ask": false });
    }
    let base = PathBuf::from(settings::normalize_dir(base));
    if base.file_name().is_some_and(|n| n.to_string_lossy() == name) {
        return json!({ "ask": false, "dir": base.to_string_lossy() });
    }
    json!({ "ask": true, "name": name, "base": base.to_string_lossy(), "target": base.join(&name).to_string_lossy() })
}

fn mkdir(path: &str) -> Reply {
    let path = settings::normalize_dir(path);
    std::fs::create_dir_all(&path)
        .map(|()| json!({ "path": path }))
        .map_err(|e| json!({ "title": "Не удалось создать папку", "error": format!("{path}: {e}") }))
}

fn enqueue(shared: &Shared, job: &Value) -> Reply {
    let spec = JobSpec::from_json(job).map_err(fail)?;
    let mut view = View::new(&spec, "queued");
    shared.queue.push(spec).map_err(fail)?;
    view.emit(shared, true);
    Ok(Value::Null)
}

fn cancel(shared: &Shared, id: &str) -> Value {
    match shared.queue.cancel(id) {
        Cancel::Dequeued(spec) => {
            let mut view = View::new(&spec, "queued");
            view.cancelled("Убрано из очереди".into());
            view.emit(shared, true);
            json!({ "state": "cancelled" })
        }
        Cancel::Running => json!({ "state": "cancelling" }),
        Cancel::Unknown => json!({ "state": "unknown" }),
    }
}

fn open_folder(dir: &str, file: Option<&str>) {
    let dir = PathBuf::from(settings::normalize_dir(dir));
    let dir = std::path::absolute(&dir).unwrap_or(dir);
    let file = file.map(PathBuf::from).filter(|f| f.is_file());
    process::open_in_file_manager(&dir, file.as_deref());
}

fn browse(shared: &Shared, msg: &Value) -> Reply {
    let start = msg["start"].as_str().unwrap_or("");
    let mut dialog = rfd::FileDialog::new().set_title("Папка для загрузок");
    if !start.is_empty() && Path::new(start).is_dir() {
        dialog = dialog.set_directory(start);
    }
    #[cfg(windows)]
    let parent = shared.parent_window.map(ParentWindow);
    #[cfg(windows)]
    if let Some(parent) = &parent {
        dialog = dialog.set_parent(parent);
    }
    #[cfg(not(windows))]
    let _ = shared.parent_window;
    let picked = dialog.pick_folder().map(|p| settings::normalize_dir(&p.to_string_lossy()));
    Ok(json!({ "path": picked }))
}

/// Окно браузера по HWND, который браузер передал в `--parent-window`.
#[cfg(windows)]
struct ParentWindow(isize);

#[cfg(windows)]
impl raw_window_handle::HasWindowHandle for ParentWindow {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        use raw_window_handle::{HandleError, RawWindowHandle, Win32WindowHandle, WindowHandle};
        let hwnd = std::num::NonZeroIsize::new(self.0).ok_or(HandleError::Unavailable)?;
        // SAFETY: окно браузера живёт дольше диалога, который он же и открыл.
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(Win32WindowHandle::new(hwnd))) })
    }
}

#[cfg(windows)]
impl raw_window_handle::HasDisplayHandle for ParentWindow {
    fn display_handle(&self) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::windows())
    }
}

// ─── Компоненты ──────────────────────────────────────────────────────────────

/// Ищет yt-dlp, Deno и ffmpeg, затем сверяет версию yt-dlp с последней.
fn detect(shared: &Arc<Shared>) {
    shared.lock().checking = true;
    let shared = shared.clone();
    thread::spawn(move || {
        let found: BTreeMap<Component, Installed> =
            Component::ALL.iter().filter_map(|&c| components::find(c).map(|i| (c, i))).collect();
        {
            let mut c = shared.lock();
            if c.install.is_none() {
                c.found = found;
            }
            c.detected = true;
        }
        shared.comp_cv.notify_all();
        shared.emit_components();
        let latest = components::latest_ytdlp_version();
        {
            let mut c = shared.lock();
            c.checking = false;
            match latest {
                Ok(version) => {
                    c.check_error = None;
                    c.set_latest(version);
                }
                Err(e) => c.check_error = Some(e),
            }
        }
        shared.emit_components();
    });
}

fn check(shared: &Arc<Shared>, user: bool) -> Reply {
    {
        let mut c = shared.lock();
        if c.checking || c.install.is_some() {
            return Ok(Value::Null);
        }
        if user && matches!(c.outcome, Some(InstallOutcome::Failed { .. })) {
            c.outcome = None;
        }
    }
    // Компоненты могли поставить или обновить вручную, поэтому ищем заново.
    detect(shared);
    shared.emit_components();
    Ok(Value::Null)
}

fn install(shared: &Arc<Shared>, msg: &Value) -> Reply {
    let connections =
        (msg["connections"].as_u64().unwrap_or(settings::DEFAULT_CONNECTIONS as u64) as usize).clamp(1, 16);
    let (order, update) = {
        let c = shared.lock();
        if c.install.is_some() {
            return Ok(Value::Null);
        }
        match msg["which"].as_str() {
            Some("missing") => (c.missing(), false),
            Some(key) => {
                let comp = from_key(key).ok_or_else(|| fail(format!("неизвестный компонент: {key}")))?;
                (vec![comp], comp == Component::YtDlp && c.update_available())
            }
            None => return Err(fail("не указано, что устанавливать")),
        }
    };
    if order.is_empty() {
        return Ok(Value::Null);
    }
    if !shared.queue.try_pause() {
        return Err(fail("Идёт загрузка. Компоненты можно обновить, когда она закончится."));
    }
    let cancel = CancelToken::new();
    {
        let mut c = shared.lock();
        c.outcome = None;
        c.install = Some(Install {
            order: order.clone(),
            index: 0,
            stage: None,
            failed: BTreeMap::new(),
            cancel: cancel.clone(),
            update,
        });
    }
    shared.emit_components();

    let shared = shared.clone();
    thread::spawn(move || {
        for (index, comp) in order.into_iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            if let Some(inst) = shared.lock().install.as_mut() {
                inst.index = index;
                inst.stage = None;
            }
            shared.emit_components();
            let mut last = Instant::now() - Duration::from_secs(1);
            let result = components::install(comp, connections, &cancel, &mut |stage| {
                let downloading = matches!(stage, Stage::Downloading { .. });
                if let Some(inst) = shared.lock().install.as_mut() {
                    inst.stage = Some(stage);
                }
                if !downloading || last.elapsed() >= Duration::from_millis(250) {
                    last = Instant::now();
                    shared.emit_components();
                }
            });
            {
                let mut c = shared.lock();
                match result {
                    Ok(installed) => {
                        let version = installed.version.clone();
                        c.found.insert(comp, installed);
                        if comp == Component::YtDlp {
                            c.set_latest(version);
                        }
                    }
                    Err(e) if !cancel.is_cancelled() => {
                        if let Some(inst) = c.install.as_mut() {
                            inst.failed.insert(comp, e);
                        }
                    }
                    Err(_) => {}
                }
            }
            shared.emit_components();
        }
        {
            let mut c = shared.lock();
            if let Some(inst) = c.install.take() {
                let cancelled = inst.cancel.is_cancelled();
                c.outcome = Some(if cancelled || !inst.failed.is_empty() {
                    InstallOutcome::Failed { failed: inst.failed, cancelled }
                } else {
                    InstallOutcome::Done { update: inst.update }
                });
            }
        }
        shared.queue.resume();
        shared.emit_components();
    });
    Ok(Value::Null)
}

fn key(c: Component) -> &'static str {
    match c {
        Component::YtDlp => "yt-dlp",
        Component::Deno => "deno",
        Component::Ffmpeg => "ffmpeg",
    }
}

fn from_key(key: &str) -> Option<Component> {
    Component::ALL.into_iter().find(|&c| self::key(c) == key)
}

/// Состояние компонентов для расширения. Подписи (строка статуса, баннер)
/// расширение собирает само, так же как окно приложения.
fn snapshot(c: &Components) -> Value {
    let failed_map = |failed: &BTreeMap<Component, String>| {
        failed.iter().map(|(k, e)| (key(*k).to_string(), Value::from(first_line(e)))).collect::<serde_json::Map<_, _>>()
    };
    json!({
        "detected": c.detected,
        "checking": c.checking,
        "latest": c.latest,
        "check_error": c.check_error,
        "update_available": c.update_available(),
        "missing": c.missing().into_iter().map(key).collect::<Vec<_>>(),
        "bin_dir": components::bin_dir().map(|d| d.display().to_string()),
        "items": Component::ALL.iter().map(|&k| json!({
            "key": key(k),
            "title": k.title(),
            "purpose": k.purpose(),
            "approx_mb": k.approx_size_mb(),
            "installed": c.found.get(&k).map(|i| json!({
                "version": i.version,
                "path": i.path.display().to_string(),
                "managed": i.managed,
            })),
        })).collect::<Vec<_>>(),
        "install": c.install.as_ref().map(|i| json!({
            "order": i.order.iter().map(|&k| key(k)).collect::<Vec<_>>(),
            "index": i.index,
            "current": i.order.get(i.index).map(|&k| key(k)),
            "update": i.update,
            "stage": stage_text(i.stage),
            "fraction": stage_fraction(i.stage),
            "failed": failed_map(&i.failed),
        })),
        "outcome": match &c.outcome {
            None => Value::Null,
            Some(InstallOutcome::Done { update }) => json!({ "type": "done", "update": update }),
            Some(InstallOutcome::Failed { failed, cancelled }) => json!({
                "type": "failed",
                "cancelled": cancelled,
                "failed": failed_map(failed),
            }),
        },
    })
}

pub fn first_line(s: &str) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}

/// "Deno, ffmpeg".
fn names(list: &[Component]) -> String {
    list.iter().map(|c| c.title()).collect::<Vec<_>>().join(", ")
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} МБ", bytes as f64 / (1024.0 * 1024.0))
}

fn stage_text(stage: Option<Stage>) -> String {
    match stage {
        None | Some(Stage::Resolving) => "Узнаём последнюю версию…".into(),
        Some(Stage::Downloading { done, total, part, parts }) => {
            let file = if parts > 1 { format!(" (файл {part} из {parts})") } else { String::new() };
            match total {
                Some(total) if total > 0 => format!("Скачивание: {} из {}{file}", megabytes(done), megabytes(total)),
                _ if done == 0 => "Подключение…".into(),
                _ => format!("Скачивание: {}{file}", megabytes(done)),
            }
        }
        Some(Stage::Unpacking) => "Распаковка…".into(),
        Some(Stage::Verifying) => "Проверка и пробный запуск…".into(),
    }
}

fn stage_fraction(stage: Option<Stage>) -> Option<f32> {
    match stage {
        Some(Stage::Downloading { done, total: Some(total), .. }) if total > 0 => Some(done as f32 / total as f32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_channel_folder_like_the_app() {
        let v = channel_folder("/tmp/YT/", "AC/DC: Live?");
        assert_eq!(v["ask"], true);
        assert_eq!(v["name"], "AC_DC_ Live_");
        #[cfg(unix)]
        assert_eq!(v["target"], "/tmp/YT/AC_DC_ Live_");
        assert_eq!(channel_folder("/tmp/Канал", "Канал")["ask"], false);
        assert_eq!(channel_folder("", "Канал")["ask"], false);
        assert_eq!(channel_folder("/tmp", "...")["ask"], false);
    }

    #[test]
    fn snapshot_reports_missing_and_install() {
        let mut c = Components { detected: true, ..Default::default() };
        c.found.insert(
            Component::YtDlp,
            Installed { path: "/b/yt-dlp".into(), version: "2025.01.01".into(), managed: true },
        );
        c.latest = Some("2026.10.10".into());
        c.install = Some(Install {
            order: vec![Component::Deno, Component::Ffmpeg],
            index: 1,
            stage: Some(Stage::Downloading { done: 1 << 20, total: Some(4 << 20), part: 1, parts: 1 }),
            failed: BTreeMap::from([(Component::Deno, "сеть\nподробности".into())]),
            cancel: CancelToken::new(),
            update: false,
        });
        let s = snapshot(&c);
        assert_eq!(s["missing"], json!(["deno", "ffmpeg"]));
        assert_eq!(s["update_available"], true);
        assert_eq!(s["install"]["current"], "ffmpeg");
        assert_eq!(s["install"]["stage"], "Скачивание: 1.0 МБ из 4.0 МБ");
        assert_eq!(s["install"]["fraction"], 0.25);
        assert_eq!(s["install"]["failed"]["deno"], "сеть");
        assert_eq!(s["items"][0]["installed"]["version"], "2025.01.01");
        assert!(s["items"][1]["installed"].is_null());
    }

    #[test]
    fn hello_lists_only_readable_browsers() {
        let h = hello();
        let browsers: Vec<&str> =
            h["cookie_browsers"].as_array().unwrap().iter().map(|b| b["value"].as_str().unwrap()).collect();
        assert!(browsers.contains(&"firefox"));
        assert!(!browsers.contains(&"edge") && !browsers.contains(&"chrome") && !browsers.contains(&""));
    }
}
