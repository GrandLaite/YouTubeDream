//! Очередь загрузок. Видео качаются строго по одному, как в приложении:
//! уборка после отмены (`postprocess::cleanup_partials`) считает все
//! `.part` в папке своими, а две загрузки в одну папку мешали бы друг другу.
//!
//! Расширение показывает загрузку по событиям `job`: тексты и проценты
//! считаются здесь так же, как в окне приложения (`app.rs`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use lader_core::cancel::CancelToken;
use lader_core::settings;
use lader_core::urls;
use lader_core::ytdlp::{
    self, AudioFormat, BatchEvent, BatchSummary, ChannelEntry, DownloadOptions, Outcome, Progress, Stream, Target,
    VideoContainer,
};
use serde::Serialize;
use serde_json::Value;

use crate::cookies;
use crate::host::{Shared, first_line};

/// Что скачать, в том виде, в каком это прислало расширение.
#[derive(Debug, Clone)]
pub struct JobSpec {
    pub id: String,
    pub kind: Kind,
    pub target: Target,
    pub out_dir: PathBuf,
    pub connections: usize,
    pub cookies: cookies::Spec,
}

#[derive(Debug, Clone)]
pub enum Kind {
    Single { url: String, title: String },
    Batch { entries: Vec<ChannelEntry> },
}

impl JobSpec {
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let id = v["id"].as_str().filter(|s| !s.is_empty()).ok_or("у загрузки нет id")?.to_string();
        let kind = match v["kind"].as_str() {
            Some("single") => {
                let url = v["url"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("нет ссылки на видео")?;
                Kind::Single { url: url.to_string(), title: v["title"].as_str().unwrap_or("").to_string() }
            }
            Some("batch") => {
                let entries: Vec<ChannelEntry> = v["entries"]
                    .as_array()
                    .ok_or("нет списка видео")?
                    .iter()
                    .filter_map(|e| {
                        let url = e["url"].as_str().filter(|u| !u.is_empty())?;
                        Some(ChannelEntry {
                            id: e["id"].as_str().unwrap_or("").to_string(),
                            url: url.to_string(),
                            title: e["title"].as_str().unwrap_or("").to_string(),
                            duration: e["duration"].as_u64().unwrap_or(0),
                            thumbnail: String::new(),
                        })
                    })
                    .collect();
                if entries.is_empty() {
                    return Err("Отметьте хотя бы одно видео для скачивания.".into());
                }
                Kind::Batch { entries }
            }
            _ => return Err("неизвестный вид загрузки".into()),
        };
        let target = parse_target(&v["target"])?;
        let out_dir = settings::normalize_dir(v["out_dir"].as_str().unwrap_or(""));
        if out_dir.is_empty() {
            return Err("Не указана папка для загрузки.".into());
        }
        let connections =
            (v["connections"].as_u64().unwrap_or(settings::DEFAULT_CONNECTIONS as u64) as usize).clamp(1, 16);
        Ok(Self {
            id,
            kind,
            target,
            out_dir: PathBuf::from(out_dir),
            connections,
            cookies: cookies::Spec::from_json(&v["cookies"])?,
        })
    }

    fn title(&self) -> String {
        match &self.kind {
            Kind::Single { title, url } => {
                if title.is_empty() {
                    url.clone()
                } else {
                    title.clone()
                }
            }
            Kind::Batch { entries } => entries.first().map(|e| e.title.clone()).unwrap_or_default(),
        }
    }
}

/// `{"type": "video", "height": 1080, "container": "mp4"}` или
/// `{"type": "audio", "format": "mp3"}`.
pub fn parse_target(v: &Value) -> Result<Target, String> {
    match v["type"].as_str() {
        Some("video") => {
            let max_height = v["height"].as_u64().filter(|h| *h > 0).unwrap_or(9999).min(9999) as u32;
            let container = match v["container"].as_str() {
                Some("mp4") | None => VideoContainer::Mp4,
                Some("webm") => VideoContainer::Webm,
                Some("mkv") => VideoContainer::Mkv,
                Some(other) => return Err(format!("неизвестный формат видео: {other}")),
            };
            Ok(Target::Video { max_height, container })
        }
        Some("audio") => match v["format"].as_str() {
            Some("mp3") | None => Ok(Target::Audio(AudioFormat::Mp3)),
            Some("m4a") => Ok(Target::Audio(AudioFormat::M4a)),
            Some(other) => Err(format!("неизвестный формат звука: {other}")),
        },
        _ => Err("не указано, что скачивать".into()),
    }
}

// ─── Очередь ─────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct Queue {
    state: Mutex<State>,
    cv: Condvar,
}

#[derive(Default)]
struct State {
    pending: VecDeque<JobSpec>,
    current: Option<(String, CancelToken)>,
    /// Идёт установка компонентов: yt-dlp может подмениться посреди загрузки.
    paused: bool,
    shutdown: bool,
}

/// Чем закончилась просьба отменить загрузку.
pub enum Cancel {
    /// Ещё ждала очереди и просто убрана из неё.
    Dequeued(JobSpec),
    Running,
    Unknown,
}

impl Queue {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push(&self, spec: JobSpec) -> Result<(), String> {
        let mut s = self.lock();
        let id = spec.id.as_str();
        if s.current.as_ref().is_some_and(|(cur, _)| cur == id) || s.pending.iter().any(|p| p.id == id) {
            return Err("такая загрузка уже есть в очереди".into());
        }
        s.pending.push_back(spec);
        self.cv.notify_all();
        Ok(())
    }

    pub fn cancel(&self, id: &str) -> Cancel {
        let mut s = self.lock();
        if let Some(pos) = s.pending.iter().position(|p| p.id == id) {
            return Cancel::Dequeued(s.pending.remove(pos).expect("позиция из position"));
        }
        match &s.current {
            Some((cur, token)) if cur == id => {
                // taskkill занимает заметное время, не держим очередь под мьютексом.
                let token = token.clone();
                drop(s);
                thread::spawn(move || token.cancel());
                Cancel::Running
            }
            _ => Cancel::Unknown,
        }
    }

    #[cfg(test)]
    pub fn running(&self) -> bool {
        self.lock().current.is_some()
    }

    /// Ставит очередь на паузу на время установки компонентов. Не выходит,
    /// если загрузка уже идёт: подменять yt-dlp под ней нельзя.
    pub fn try_pause(&self) -> bool {
        let mut s = self.lock();
        if s.current.is_some() {
            return false;
        }
        s.paused = true;
        true
    }

    pub fn resume(&self) {
        self.lock().paused = false;
        self.cv.notify_all();
    }

    /// Браузер отключился: текущую загрузку отменяем, очередь сбрасываем.
    pub fn shutdown(&self) {
        let mut s = self.lock();
        s.shutdown = true;
        s.pending.clear();
        if let Some((_, token)) = &s.current {
            token.cancel();
        }
        self.cv.notify_all();
    }

    /// Ждёт, пока текущая загрузка уберёт за собой после отмены.
    pub fn wait_idle(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut s = self.lock();
        while s.current.is_some() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            s = self.cv.wait_timeout(s, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn next(&self) -> Option<(JobSpec, CancelToken)> {
        let mut s = self.lock();
        loop {
            if s.shutdown {
                return None;
            }
            if !s.paused
                && let Some(spec) = s.pending.pop_front()
            {
                let token = CancelToken::new();
                s.current = Some((spec.id.clone(), token.clone()));
                return Some((spec, token));
            }
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn finish(&self) {
        self.lock().current = None;
        self.cv.notify_all();
    }
}

pub fn spawn_worker(shared: Arc<Shared>) {
    thread::spawn(move || {
        while let Some((spec, cancel)) = shared.queue.next() {
            run(&shared, spec, &cancel);
            shared.queue.finish();
        }
    });
}

// ─── Состояние загрузки для расширения ───────────────────────────────────────

/// Не чаще, чем раз в столько, отправляем прогресс: браузеру и интерфейсу
/// хватает четырёх обновлений в секунду.
const EMIT_EVERY: Duration = Duration::from_millis(250);

#[derive(Debug, Serialize)]
pub struct View {
    id: String,
    kind: &'static str,
    /// queued, running, cancelling, done, failed, cancelled.
    state: &'static str,
    heading: String,
    subtitle: String,
    status: String,
    progress: f64,
    known: bool,
    percent: String,
    size: String,
    speed: String,
    eta: String,
    /// Сколько соединений качает прямо сейчас (многопоточная загрузка).
    connections: usize,
    total: usize,
    index: usize,
    finished: usize,
    failed: Vec<Failure>,
    dir: String,
    file: Option<String>,
    can_open: bool,
    error: Option<String>,
    /// Ошибка похожа на требование авторизации, помогут cookies.
    auth: bool,
    /// Ошибка похожа на последствия устаревшего yt-dlp.
    outdated: bool,
    #[serde(skip)]
    titles: Vec<String>,
    #[serde(skip)]
    last_emit: Option<Instant>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    index: usize,
    title: String,
    error: String,
}

impl View {
    pub fn new(spec: &JobSpec, state: &'static str) -> Self {
        let (kind, titles) = match &spec.kind {
            Kind::Single { .. } => ("single", Vec::new()),
            Kind::Batch { entries } => ("batch", entries.iter().map(|e| e.title.clone()).collect()),
        };
        let total = titles.len().max(1);
        let mut view = Self {
            id: spec.id.clone(),
            kind,
            state,
            heading: String::new(),
            subtitle: spec.title(),
            status: String::new(),
            progress: 0.0,
            known: true,
            percent: String::new(),
            size: String::new(),
            speed: String::new(),
            eta: String::new(),
            connections: 0,
            total,
            index: 0,
            finished: 0,
            failed: Vec::new(),
            dir: spec.out_dir.to_string_lossy().into_owned(),
            file: None,
            can_open: false,
            error: None,
            auth: false,
            outdated: false,
            titles,
            last_emit: None,
        };
        match state {
            "queued" => {
                view.heading = "В очереди".into();
                view.status = "Начнётся после текущей загрузки".into();
            }
            _ => view.begin(),
        }
        view
    }

    fn is_batch(&self) -> bool {
        self.kind == "batch"
    }

    fn begin(&mut self) {
        self.state = "running";
        self.heading =
            if self.is_batch() { format!("Видео 1 из {}", self.total) } else { "Загрузка…".into() };
        self.status = "Запуск yt-dlp…".into();
        self.reset_progress();
    }

    fn reset_progress(&mut self) {
        self.progress = 0.0;
        self.known = true;
        self.percent = "0%".into();
        self.size.clear();
        self.speed.clear();
        self.eta.clear();
        self.connections = 0;
    }

    pub fn emit(&mut self, shared: &Shared, force: bool) {
        if !force && self.last_emit.is_some_and(|t| t.elapsed() < EMIT_EVERY) {
            return;
        }
        self.last_emit = Some(Instant::now());
        if let Ok(body) = serde_json::to_value(&*self) {
            shared.out.event("job", body);
        }
    }

    fn on_progress(&mut self, progress: Progress) -> bool {
        match progress {
            Progress::Status(text) => {
                let changed = self.status != text;
                self.status = text;
                changed
            }
            Progress::Download(p) => {
                let finished = p.percent.is_some_and(|v| v >= 100.0);
                match p.percent {
                    Some(pct) => {
                        self.known = true;
                        self.progress = pct / 100.0;
                        self.percent = format!("{pct:.1}%");
                    }
                    None => {
                        self.known = false;
                        self.percent.clear();
                    }
                }
                if let Some(size) = p.size {
                    self.size = size;
                }
                if let Some(speed) = p.speed {
                    self.speed = speed;
                }
                if let Some(eta) = p.eta {
                    self.eta = eta;
                }
                self.connections = p.connections.unwrap_or(0);
                self.status = match p.stream {
                    Some(Stream::Video) => "Загрузка видео",
                    Some(Stream::Audio) => "Загрузка звука",
                    None => "Загрузка…",
                }
                .into();
                finished
            }
            Progress::Merging => {
                if !self.is_batch() {
                    self.heading = "Объединение…".into();
                }
                self.status = "Склейка видео и звука…".into();
                self.known = true;
                self.progress = 0.99;
                self.percent = "99%".into();
                self.speed.clear();
                self.eta.clear();
                self.connections = 0;
                true
            }
        }
    }

    fn on_batch_event(&mut self, event: BatchEvent) -> bool {
        match event {
            BatchEvent::Started { index, total, title } => {
                self.index = index;
                self.heading = format!("Видео {} из {total}", index + 1);
                if !self.failed.is_empty() {
                    self.heading.push_str(&format!(" · ошибок: {}", self.failed.len()));
                }
                self.subtitle = title;
                self.status = "Подготовка…".into();
                self.reset_progress();
                true
            }
            BatchEvent::Progress(p) => self.on_progress(p),
            BatchEvent::VideoDone { already_downloaded, .. } => {
                self.finished += 1;
                if already_downloaded {
                    self.status = "Уже скачано — пропущено".into();
                }
                true
            }
            BatchEvent::VideoFailed { index, error } => {
                self.note_error(&error);
                let title = self.titles.get(index).cloned().unwrap_or_default();
                self.failed.push(Failure { index, title, error: ytdlp::clean_error(&error) });
                true
            }
        }
    }

    fn note_error(&mut self, raw: &str) {
        self.auth |= urls::needs_auth_error(raw);
        self.outdated |= urls::looks_outdated(raw);
    }

    fn finish_single(&mut self, outcome: Outcome) {
        self.speed.clear();
        self.eta.clear();
        self.connections = 0;
        match outcome {
            Outcome::Done { file, already_downloaded } => {
                self.state = "done";
                self.heading = "Готово".into();
                self.known = true;
                self.progress = 1.0;
                self.percent = "100%".into();
                self.status = match (&file, already_downloaded) {
                    (_, true) => "Уже скачано — пропущено".to_string(),
                    (Some(f), false) => {
                        format!("Сохранено: {}", f.file_name().unwrap_or_default().to_string_lossy())
                    }
                    (None, false) => "Загрузка завершена".to_string(),
                };
                self.can_open = true;
                self.file = file.map(|f| absolute(&f).to_string_lossy().into_owned());
            }
            Outcome::Failed(error) => {
                self.state = "failed";
                self.heading = "Ошибка".into();
                self.progress = 0.0;
                self.percent.clear();
                let clean = ytdlp::clean_error(&error);
                self.status = first_line(&clean);
                self.error = Some(clean);
                self.note_error(&error);
            }
            Outcome::Cancelled => self.cancelled("Недокачанные файлы удалены".into()),
        }
    }

    fn finish_batch(&mut self, summary: BatchSummary) {
        self.speed.clear();
        self.eta.clear();
        self.connections = 0;
        if summary.cancelled {
            let status = format!("Скачано {} из {}", self.finished, summary.total);
            self.cancelled(status);
            self.can_open = self.finished > 0;
            return;
        }
        let failed = summary.failed.len();
        let ok = summary.total - failed;
        self.heading = format!("Готово: {ok} из {}", summary.total);
        if failed > 0 {
            self.heading.push_str(&format!(" (ошибок: {failed})"));
        }
        self.state = if ok == 0 { "failed" } else { "done" };
        self.subtitle.clear();
        self.known = true;
        self.progress = 1.0;
        self.percent = "100%".into();
        self.status = match (failed, ok) {
            (0, _) => "Все видео скачаны!".to_string(),
            (_, 0) => "Ни одно видео не скачалось.".to_string(),
            _ => format!("Скачано {ok}, пропущено {failed}."),
        };
        self.can_open = true;
    }

    pub fn cancelled(&mut self, status: String) {
        self.state = "cancelled";
        self.heading = "Отменено".into();
        self.percent.clear();
        self.status = status;
    }

    pub fn cancelling(&mut self) {
        self.state = "cancelling";
        self.heading = "Отмена…".into();
    }

    pub fn fail(&mut self, error: String) {
        self.state = "failed";
        self.heading = "Ошибка".into();
        self.status = first_line(&error);
        self.error = Some(error);
    }
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn run(shared: &Shared, spec: JobSpec, cancel: &CancelToken) {
    let mut view = View::new(&spec, "running");
    view.emit(shared, true);

    let tools = match shared.tools() {
        Ok(tools) => tools,
        Err(e) => {
            view.fail(e["error"].as_str().unwrap_or("Не хватает компонентов").to_string());
            view.emit(shared, true);
            return;
        }
    };
    // Файл cookies живёт до конца загрузки (guard) и удаляется сразу после.
    let (cookies, _guard) = match spec.cookies.resolve() {
        Ok(c) => c,
        Err(e) => {
            view.fail(e);
            view.emit(shared, true);
            return;
        }
    };
    let opts = DownloadOptions {
        url: String::new(),
        target: spec.target,
        out_dir: spec.out_dir.clone(),
        cookies,
        connections: spec.connections,
    };
    let cancelling = |view: &mut View| {
        if cancel.is_cancelled() && view.state == "running" {
            view.cancelling();
            return true;
        }
        false
    };

    match spec.kind {
        Kind::Single { url, .. } => {
            let opts = DownloadOptions { url, ..opts };
            let outcome = ytdlp::download(&tools, &opts, cancel, &mut |p| {
                let force = view.on_progress(p) | cancelling(&mut view);
                view.emit(shared, force);
            });
            if let Outcome::Failed(error) = &outcome {
                shared.refresh_latest_if_outdated(error);
            }
            view.finish_single(outcome);
        }
        Kind::Batch { entries } => {
            let summary = ytdlp::download_batch(&tools, &entries, &opts, cancel, &mut |e| {
                let force = view.on_batch_event(e) | cancelling(&mut view);
                view.emit(shared, force);
            });
            if let Some((_, error)) = summary.failed.first() {
                shared.refresh_latest_if_outdated(error);
            }
            view.finish_batch(summary);
        }
    }
    view.emit(shared, true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lader_core::ytdlp::DownloadProgress;
    use serde_json::json;

    fn spec(v: Value) -> Result<JobSpec, String> {
        JobSpec::from_json(&v)
    }

    #[test]
    fn parses_job_specs() {
        let s = spec(json!({ "id": "a", "kind": "single", "url": " https://youtu.be/x ", "title": "Клип",
            "target": { "type": "video", "height": 720, "container": "webm" }, "out_dir": "/tmp/v/", "connections": 64 }))
        .unwrap();
        assert!(matches!(s.kind, Kind::Single { ref url, .. } if url == "https://youtu.be/x"));
        assert_eq!(s.target, Target::Video { max_height: 720, container: VideoContainer::Webm });
        assert_eq!(s.connections, 16);
        assert_eq!(s.cookies, cookies::Spec::None);
        #[cfg(unix)]
        assert_eq!(s.out_dir, PathBuf::from("/tmp/v"));

        let b = spec(json!({ "id": "b", "kind": "batch", "target": { "type": "audio", "format": "m4a" }, "out_dir": "/tmp",
            "entries": [{ "id": "dQw4w9WgXcQ", "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ", "title": "A" }, { "url": "" }] }))
        .unwrap();
        assert!(matches!(b.kind, Kind::Batch { ref entries } if entries.len() == 1 && entries[0].id == "dQw4w9WgXcQ"));
        assert_eq!(b.target, Target::Audio(AudioFormat::M4a));

        assert!(
            spec(
                json!({ "id": "c", "kind": "batch", "entries": [], "target": { "type": "audio" }, "out_dir": "/tmp" })
            )
            .is_err()
        );
        assert!(
            spec(json!({ "id": "d", "kind": "single", "url": "u", "target": { "type": "video" }, "out_dir": " " }))
                .is_err()
        );
        assert!(
            spec(json!({ "id": "e", "kind": "single", "url": "u", "target": { "type": "gif" }, "out_dir": "/tmp" }))
                .is_err()
        );
    }

    #[test]
    fn queue_order_cancel_and_pause() {
        let q = Queue::default();
        let mk = |id: &str| {
            spec(json!({ "id": id, "kind": "single", "url": "u", "target": { "type": "audio" }, "out_dir": "/tmp" }))
                .unwrap()
        };
        q.push(mk("1")).unwrap();
        q.push(mk("2")).unwrap();
        assert!(q.push(mk("2")).is_err());
        assert!(matches!(q.cancel("2"), Cancel::Dequeued(s) if s.id == "2"));
        let (first, _) = q.next().unwrap();
        assert_eq!(first.id, "1");
        assert!(q.running());
        assert!(!q.try_pause(), "пауза при идущей загрузке");
        assert!(matches!(q.cancel("1"), Cancel::Running));
        q.finish();
        assert!(q.try_pause());
        q.push(mk("3")).unwrap();
        q.resume();
        assert_eq!(q.next().unwrap().0.id, "3");
        q.finish();
        q.shutdown();
        assert!(q.next().is_none());
    }

    #[test]
    fn view_texts_match_the_app() {
        let s = spec(json!({ "id": "a", "kind": "batch", "target": { "type": "audio" }, "out_dir": "/tmp",
            "entries": [{ "url": "u1", "title": "Первое" }, { "url": "u2", "title": "Второе" }] }))
        .unwrap();
        let mut v = View::new(&s, "running");
        assert_eq!(v.heading, "Видео 1 из 2");
        v.on_batch_event(BatchEvent::Started { index: 0, total: 2, title: "Первое".into() });
        v.on_progress(Progress::Download(DownloadProgress {
            percent: Some(42.0),
            size: Some("1.00MiB / 2.38MiB".into()),
            speed: Some("1.00MiB/s".into()),
            eta: Some("00:01".into()),
            stream: Some(Stream::Audio),
            connections: Some(4),
        }));
        assert_eq!((v.percent.as_str(), v.status.as_str(), v.connections), ("42.0%", "Загрузка звука", 4));
        v.on_batch_event(BatchEvent::VideoFailed { index: 0, error: "ERROR: Sign in to confirm your age".into() });
        v.on_batch_event(BatchEvent::Started { index: 1, total: 2, title: "Второе".into() });
        assert_eq!(v.heading, "Видео 2 из 2 · ошибок: 1");
        v.on_batch_event(BatchEvent::VideoDone { index: 1, already_downloaded: true });
        v.finish_batch(BatchSummary {
            total: 2,
            failed: vec![(0, "ERROR: Sign in to confirm your age".into())],
            cancelled: false,
        });
        assert_eq!(v.state, "done");
        assert_eq!(v.heading, "Готово: 1 из 2 (ошибок: 1)");
        assert_eq!(v.status, "Скачано 1, пропущено 1.");
        assert!(v.auth && !v.outdated);
        assert_eq!(v.failed[0].title, "Первое");

        let s =
            spec(json!({ "id": "b", "kind": "single", "url": "u", "target": { "type": "audio" }, "out_dir": "/tmp" }))
                .unwrap();
        let mut v = View::new(&s, "queued");
        assert_eq!(v.heading, "В очереди");
        v.begin();
        v.on_progress(Progress::Merging);
        assert_eq!((v.heading.as_str(), v.percent.as_str()), ("Объединение…", "99%"));
        v.finish_single(Outcome::Failed(
            "ERROR: [youtube] x: Unable to extract nsig; please report this issue on https://github.com/yt-dlp/yt-dlp/issues?q= . Confirm you are on the latest version using yt-dlp -U".into(),
        ));
        assert_eq!(v.state, "failed");
        assert_eq!(v.status, "ERROR: [youtube] x: Unable to extract nsig");
        assert!(v.outdated);
    }
}
