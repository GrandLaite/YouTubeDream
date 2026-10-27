//! Многопоточная загрузка файла по HTTP: несколько соединений одновременно
//! качают разные части файла.
//!
//! Подход — как у aria2 (идеи, не код: aria2 распространяется под GPL):
//! * файл делится на диапазоны прямо по ходу загрузки: освободившееся
//!   соединение делит пополам самый большой ещё не скачанный диапазон и
//!   забирает вторую половину — так быстрые соединения сами выручают
//!   медленные, и к концу все заканчивают примерно одновременно;
//! * диапазоны меньше двух «минимальных кусков» не делятся — мелкие файлы
//!   идут одним соединением;
//! * зависшее соединение (нет данных `STALL` секунд) снимается, остаток его
//!   диапазона отдаётся новому; ошибки повторяются с нарастающей паузой.
//!
//! Сеть читают рабочие потоки, а пишет на диск только координатор: при
//! отмене или зависании он сразу отпускает файл, не дожидаясь потоков.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use crate::cancel::CancelToken;
use crate::http;

/// Сколько ждать данных от соединения, прежде чем признать его зависшим.
const STALL: Duration = Duration::from_secs(30);
/// Сколько раз подряд можно повторить один диапазон.
const MAX_ATTEMPTS: u32 = 6;
const CHUNK: usize = 64 * 1024;

/// Что и откуда качать.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    pub url: String,
    /// Заголовки запроса (User-Agent и т.п. — их выдаёт yt-dlp для каждого формата).
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Сколько соединений одновременно.
    pub connections: usize,
    /// Диапазон делится, только если в нём не меньше двух таких кусков.
    pub min_split: u64,
    /// Наибольший размер одного HTTP-запроса. YouTube режет слишком длинные
    /// запросы, поэтому yt-dlp сам качает его кусками по 10 МБ.
    pub max_request: u64,
}

impl Options {
    pub fn with_connections(connections: usize) -> Self {
        Self { connections: connections.clamp(1, 32), min_split: 2 << 20, max_request: 10 << 20 }
    }
}

/// Состояние загрузки для индикатора.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    pub done: u64,
    pub total: Option<u64>,
    /// Байт в секунду за последние секунды.
    pub speed: f64,
    /// Сколько соединений качает прямо сейчас.
    pub connections: usize,
}

/// Скачивает `request` в `dest`. Пока загрузка идёт, данные лежат во
/// временном файле рядом; `dest` появляется только целиком скачанным.
pub fn download_to_file(
    request: &Request,
    dest: &Path,
    options: Options,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&Progress),
) -> Result<u64, String> {
    let partial = partial_path(dest);
    let result = download_inner(request, &partial, options, cancel, progress);
    match result {
        Ok(size) => {
            let _ = fs::remove_file(dest);
            fs::rename(&partial, dest).map_err(|e| format!("не удалось сохранить {}: {e}", dest.display()))?;
            Ok(size)
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Временное имя недокачанного файла. Не «.part»: такой файл yt-dlp счёл
/// бы своей недокачкой и попытался продолжить, а у нас в нём дыры.
pub fn partial_path(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lader-part", dest.display()))
}

fn download_inner(
    request: &Request,
    partial: &Path,
    options: Options,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&Progress),
) -> Result<u64, String> {
    let probe = probe(request)?;
    let file = File::create(partial).map_err(|e| format!("не удалось создать {}: {e}", partial.display()))?;
    let (segments, options) = match probe {
        Probe::Ranges(total) => {
            file.set_len(total).map_err(|e| format!("не хватает места на диске? {e}"))?;
            (Segments::new(total, true), options)
        }
        // Сервер не умеет отдавать части — одно соединение от начала до конца.
        Probe::Whole(total) => (
            Segments::new(total.unwrap_or(u64::MAX), false),
            Options { connections: 1, max_request: u64::MAX, ..options },
        ),
    };
    let size = Coordinator::new(request, file, segments, options).run(cancel, progress)?;
    Ok(size)
}

enum Probe {
    /// Сервер отдаёт части; известен полный размер.
    Ranges(u64),
    /// Только целиком.
    Whole(Option<u64>),
}

/// Пробный запрос первого байта: умеет ли сервер отдавать части и какой
/// у файла размер.
fn probe(request: &Request) -> Result<Probe, String> {
    let resp = send(request, Some((0, 0)), Duration::from_secs(30))?;
    let status = resp.status().as_u16();
    let total = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit_once('/'))
        .and_then(|(_, total)| total.trim().parse::<u64>().ok());
    Ok(match (status, total) {
        (206, Some(total)) if total > 0 => Probe::Ranges(total),
        _ => Probe::Whole(resp.body().content_length().filter(|_| status == 200)),
    })
}

fn send(request: &Request, range: Option<(u64, u64)>, body_timeout: Duration) -> Result<http::Response, String> {
    let mut req = http::agent().get(&request.url);
    for (name, value) in &request.headers {
        req = req.header(name.as_str(), value.as_str());
    }
    if let Some((start, end)) = range {
        req = req.header("Range", format!("bytes={start}-{end}"));
    }
    req.config()
        .timeout_recv_body(Some(body_timeout))
        .build()
        .call()
        .map_err(http::describe)
}

// ─── Диапазоны ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
struct Segment {
    /// Следующий байт, который нужно скачать.
    pos: u64,
    /// Конец диапазона (не включительно).
    end: u64,
    owner: Option<u64>,
    failures: u32,
    retry_at: Option<Instant>,
}

impl Segment {
    fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.pos)
    }
}

/// Раскладка файла на диапазоны и выбор работы для свободного соединения.
#[derive(Debug)]
struct Segments {
    list: Vec<Segment>,
    total: u64,
    splittable: bool,
}

impl Segments {
    fn new(total: u64, splittable: bool) -> Self {
        let first = Segment { pos: 0, end: total, owner: None, failures: 0, retry_at: None };
        Self { list: vec![first], total, splittable }
    }

    fn done(&self) -> u64 {
        if !self.splittable {
            return self.list[0].pos;
        }
        self.total - self.list.iter().map(Segment::remaining).sum::<u64>()
    }

    fn complete(&self) -> bool {
        self.list.iter().all(|s| s.remaining() == 0)
    }

    /// Работа для свободного соединения: сначала брошенный диапазон
    /// (после ошибки или зависания), иначе — вторая половина самого
    /// большого ещё не скачанного диапазона.
    fn take(&mut self, now: Instant, min_split: u64) -> Option<usize> {
        if let Some(i) = self.list.iter().position(|s| {
            s.owner.is_none() && s.remaining() > 0 && s.retry_at.is_none_or(|t| t <= now)
        }) {
            return Some(i);
        }
        if !self.splittable {
            return None;
        }
        let (i, largest) = self
            .list
            .iter()
            .enumerate()
            .filter(|(_, s)| s.owner.is_some())
            .max_by_key(|(_, s)| s.remaining())?;
        if largest.remaining() < min_split.saturating_mul(2) {
            return None;
        }
        let mid = largest.pos + largest.remaining() / 2;
        let end = largest.end;
        self.list[i].end = mid;
        self.list.push(Segment { pos: mid, end, owner: None, failures: 0, retry_at: None });
        Some(self.list.len() - 1)
    }

    /// Диапазон брошен из-за ошибки: повторим позже, с нарастающей паузой.
    fn fail(&mut self, i: usize, now: Instant) -> u32 {
        let s = &mut self.list[i];
        s.owner = None;
        s.failures += 1;
        s.retry_at = Some(now + Duration::from_millis(500 << s.failures.min(5)));
        if !self.splittable {
            // Без поддержки частей докачать нельзя — только сначала.
            s.pos = 0;
        }
        s.failures
    }
}

// ─── Координатор ─────────────────────────────────────────────────────────────

enum Msg {
    Data { worker: u64, bytes: Vec<u8> },
    /// Сервер закончил отдавать запрошенный диапазон.
    End { worker: u64 },
    Failed { worker: u64, error: String },
}

struct Worker {
    segment: usize,
    /// Конец текущего запроса (не включительно).
    request_end: u64,
    last_data: Instant,
    stop: Arc<AtomicBool>,
}

struct Coordinator<'a> {
    request: &'a Request,
    file: File,
    segments: Segments,
    options: Options,
    workers: std::collections::HashMap<u64, Worker>,
    next_id: u64,
    tx: SyncSender<Msg>,
    rx: mpsc::Receiver<Msg>,
    last_error: String,
}

impl<'a> Coordinator<'a> {
    fn new(request: &'a Request, file: File, segments: Segments, options: Options) -> Self {
        let (tx, rx) = mpsc::sync_channel(64);
        Self {
            request,
            file,
            segments,
            options,
            workers: Default::default(),
            next_id: 0,
            tx,
            rx,
            last_error: String::new(),
        }
    }

    fn run(mut self, cancel: &CancelToken, progress: &mut dyn FnMut(&Progress)) -> Result<u64, String> {
        let mut speed = Speed::default();
        let mut last_report = Instant::now() - Duration::from_secs(1);
        let result = loop {
            if cancel.is_cancelled() {
                break Err("отменено".to_string());
            }
            let now = Instant::now();
            while self.workers.len() < self.options.connections {
                let Some(i) = self.segments.take(now, self.options.min_split) else { break };
                self.start(i);
            }
            if self.segments.complete() {
                break Ok(());
            }

            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Msg::Data { worker, bytes }) => {
                    if let Err(e) = self.write(worker, &bytes) {
                        break Err(e);
                    }
                }
                Ok(Msg::End { worker }) => self.on_end(worker),
                Ok(Msg::Failed { worker, error }) => {
                    if let Err(e) = self.on_failed(worker, error) {
                        break Err(e);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break Err("загрузка прервалась".into()),
            }

            if let Err(e) = self.drop_stalled() {
                break Err(e);
            }
            let done = self.segments.done();
            speed.sample(done);
            if last_report.elapsed() >= Duration::from_millis(250) {
                last_report = Instant::now();
                progress(&self.progress(done, speed.value()));
            }
        };
        for w in self.workers.values() {
            w.stop.store(true, Ordering::Relaxed);
        }
        result?;
        let size = self.segments.done();
        if !self.segments.splittable && self.segments.total == u64::MAX {
            self.file.set_len(size).map_err(|e| e.to_string())?;
        }
        self.file.sync_all().map_err(|e| format!("ошибка записи на диск: {e}"))?;
        progress(&self.progress(size, speed.value()));
        Ok(size)
    }

    fn progress(&self, done: u64, speed: f64) -> Progress {
        let total = (self.segments.total != u64::MAX).then_some(self.segments.total);
        Progress { done, total, speed, connections: self.workers.len() }
    }

    /// Запускает запрос очередной порции диапазона `i`.
    fn start(&mut self, i: usize) {
        let id = self.next_id;
        self.next_id += 1;
        let seg = &mut self.segments.list[i];
        seg.owner = Some(id);
        seg.retry_at = None;
        let start = seg.pos;
        let request_end = seg.end.min(start.saturating_add(self.options.max_request));
        let range = (self.segments.splittable).then(|| (start, request_end - 1));
        let stop = Arc::new(AtomicBool::new(false));
        self.workers.insert(id, Worker { segment: i, request_end, last_data: Instant::now(), stop: stop.clone() });

        let request = self.request.clone();
        let tx = self.tx.clone();
        thread::spawn(move || fetch_range(id, &request, range, &stop, &tx));
    }

    fn write(&mut self, worker: u64, bytes: &[u8]) -> Result<(), String> {
        // Данные от снятого соединения (зависшего или уже ненужного) — мимо.
        let Some(w) = self.workers.get_mut(&worker) else { return Ok(()) };
        let seg = &mut self.segments.list[w.segment];
        // Диапазон могли укоротить, отдав половину другому соединению.
        let n = (bytes.len() as u64).min(seg.end.saturating_sub(seg.pos)) as usize;
        if !self.segments.splittable && seg.pos.saturating_add(n as u64) > self.segments.total {
            return Err("сервер прислал больше данных, чем обещал".into());
        }
        write_at(&self.file, &bytes[..n], seg.pos).map_err(|e| format!("ошибка записи на диск: {e}"))?;
        seg.pos += n as u64;
        seg.failures = 0;
        w.last_data = Instant::now();
        if seg.remaining() == 0 {
            // Диапазон готов: соединение больше не нужно.
            seg.owner = None;
            w.stop.store(true, Ordering::Relaxed);
            self.workers.remove(&worker);
        }
        Ok(())
    }

    fn on_end(&mut self, worker: u64) {
        let Some(w) = self.workers.remove(&worker) else { return };
        let seg = &mut self.segments.list[w.segment];
        seg.owner = None;
        if !self.segments.splittable && self.segments.total == u64::MAX {
            // Размер был неизвестен — конец ответа и есть конец файла.
            seg.end = seg.pos;
            return;
        }
        if seg.pos < w.request_end.min(seg.end) {
            // Сервер оборвал ответ раньше, чем отдал запрошенное.
            self.last_error = "сервер оборвал соединение".into();
            self.segments.fail(w.segment, Instant::now());
        }
        // Иначе порция скачана; остаток диапазона заберёт следующий запрос.
    }

    fn on_failed(&mut self, worker: u64, error: String) -> Result<(), String> {
        let Some(w) = self.workers.remove(&worker) else { return Ok(()) };
        self.last_error = error;
        if self.segments.fail(w.segment, Instant::now()) > MAX_ATTEMPTS {
            return Err(self.last_error.clone());
        }
        Ok(())
    }

    /// Снимает соединения, от которых давно нет данных; их остаток заберут другие.
    fn drop_stalled(&mut self) -> Result<(), String> {
        let stalled: Vec<u64> =
            self.workers.iter().filter(|(_, w)| w.last_data.elapsed() >= STALL).map(|(id, _)| *id).collect();
        for id in stalled {
            if let Some(w) = self.workers.remove(&id) {
                w.stop.store(true, Ordering::Relaxed);
                self.last_error = format!("сервер перестал отвечать: нет данных {} с", STALL.as_secs());
                if self.segments.fail(w.segment, Instant::now()) > MAX_ATTEMPTS {
                    return Err(self.last_error.clone());
                }
            }
        }
        Ok(())
    }
}

/// Рабочий поток: один HTTP-запрос, данные — координатору.
fn fetch_range(id: u64, request: &Request, range: Option<(u64, u64)>, stop: &AtomicBool, tx: &SyncSender<Msg>) {
    // Брошенный (зависший) поток рано или поздно освободится по этому таймауту.
    let resp = match send(request, range, Duration::from_secs(3600)) {
        Ok(r) => r,
        Err(error) => {
            let _ = tx.send(Msg::Failed { worker: id, error });
            return;
        }
    };
    if range.is_some() && resp.status().as_u16() != 206 {
        let error = format!("сервер вернул HTTP {} вместо части файла", resp.status().as_u16());
        let _ = tx.send(Msg::Failed { worker: id, error });
        return;
    }
    let mut reader = resp.into_body().into_reader();
    let mut buf = vec![0u8; CHUNK];
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match reader.read(&mut buf) {
            Ok(0) => {
                let _ = tx.send(Msg::End { worker: id });
                return;
            }
            Ok(n) => {
                if tx.send(Msg::Data { worker: id, bytes: buf[..n].to_vec() }).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx.send(Msg::Failed { worker: id, error: format!("обрыв загрузки: {e}") });
                return;
            }
        }
    }
}

/// Скорость по скользящему окну в несколько секунд.
#[derive(Default)]
struct Speed {
    samples: std::collections::VecDeque<(Instant, u64)>,
}

impl Speed {
    const WINDOW: Duration = Duration::from_secs(4);

    fn sample(&mut self, done: u64) {
        let now = Instant::now();
        self.samples.push_back((now, done));
        while self.samples.len() > 2 && self.samples.front().is_some_and(|(t, _)| now - *t > Self::WINDOW) {
            self.samples.pop_front();
        }
    }

    fn value(&self) -> f64 {
        match (self.samples.front(), self.samples.back()) {
            (Some((t0, d0)), Some((t1, d1))) if t1 > t0 => (d1 - d0) as f64 / (*t1 - *t0).as_secs_f64(),
            _ => 0.0,
        }
    }
}

#[cfg(windows)]
fn write_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    #[test]
    fn splits_largest_range_in_half() {
        let now = Instant::now();
        let mut s = Segments::new(100, true);
        assert_eq!(s.take(now, 10), Some(0));
        s.list[0].owner = Some(1);
        // Второе соединение забирает вторую половину файла.
        assert_eq!(s.take(now, 10), Some(1));
        assert_eq!((s.list[0].pos, s.list[0].end), (0, 50));
        assert_eq!((s.list[1].pos, s.list[1].end), (50, 100));
        s.list[1].owner = Some(2);
        s.list[0].pos = 40; // первое соединение уже почти доделало свою половину
        // Третье делит самую большую оставшуюся — вторую половину.
        assert_eq!(s.take(now, 10), Some(2));
        assert_eq!((s.list[1].end, s.list[2].pos, s.list[2].end), (75, 75, 100));
        s.list[2].owner = Some(3);
        // Куски меньше двух минимальных не делятся.
        assert_eq!(s.take(now, 20), None);
    }

    #[test]
    fn failed_range_waits_and_is_retried_first() {
        let now = Instant::now();
        let mut s = Segments::new(100, true);
        s.take(now, 10);
        s.list[0].owner = Some(1);
        s.list[0].pos = 30;
        assert_eq!(s.fail(0, now), 1);
        assert_eq!(s.list[0].pos, 30, "докачка с места обрыва");
        assert_eq!(s.take(now, 1000), None, "пауза перед повтором");
        assert_eq!(s.take(now + Duration::from_secs(5), 1000), Some(0));
    }

    #[test]
    fn whole_file_restarts_from_zero() {
        let now = Instant::now();
        let mut s = Segments::new(100, false);
        s.take(now, 10);
        s.list[0].owner = Some(1);
        s.list[0].pos = 60;
        s.fail(0, now);
        assert_eq!(s.list[0].pos, 0);
        s.list[0].owner = Some(2);
        assert_eq!(s.take(now + Duration::from_secs(5), 1), None, "без частей не делится");
    }

    /// Мини-сервер: отдаёт `data`, частями (206) или только целиком.
    fn serve(data: Arc<Vec<u8>>, ranges: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let data = data.clone();
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut range = None;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        let lower = line.to_ascii_lowercase();
                        if let Some(v) = lower.strip_prefix("range: bytes=") {
                            let (a, b) = v.trim().split_once('-').unwrap();
                            range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()));
                        }
                    }
                    let mut stream = stream;
                    let head = match range.filter(|_| ranges) {
                        Some((a, b)) => {
                            let b = b.min(data.len() - 1);
                            let body = &data[a..=b];
                            let h = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{}\r\nConnection: close\r\n\r\n",
                                body.len(),
                                data.len()
                            );
                            (h, body.to_vec())
                        }
                        None => (
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", data.len()),
                            data.to_vec(),
                        ),
                    };
                    let _ = stream.write_all(head.0.as_bytes());
                    let _ = stream.write_all(&head.1);
                });
            }
        });
        format!("http://{addr}/file")
    }

    fn test_data(len: usize) -> Arc<Vec<u8>> {
        Arc::new((0..len).map(|i| (i * 31 % 251) as u8).collect())
    }

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lader-fetch-{}-{name}", std::process::id()))
    }

    #[test]
    fn downloads_in_many_connections() {
        let data = test_data(3 * 1024 * 1024 + 123);
        let url = serve(data.clone(), true);
        let dest = temp("ranges.bin");
        let options = Options { connections: 6, min_split: 64 * 1024, max_request: 256 * 1024 };
        let mut max_connections = 0;
        let size = download_to_file(&Request { url, headers: vec![] }, &dest, options, &CancelToken::new(), &mut |p| {
            max_connections = max_connections.max(p.connections)
        })
        .unwrap();
        assert_eq!(size, data.len() as u64);
        assert_eq!(fs::read(&dest).unwrap(), *data);
        assert!(max_connections > 1, "качали в одно соединение");
        assert!(!partial_path(&dest).exists());
        fs::remove_file(&dest).unwrap();
    }

    #[test]
    fn falls_back_to_single_connection_without_ranges() {
        let data = test_data(700 * 1024);
        let url = serve(data.clone(), false);
        let dest = temp("whole.bin");
        let size = download_to_file(
            &Request { url, headers: vec![] },
            &dest,
            Options::with_connections(8),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(size, data.len() as u64);
        assert_eq!(fs::read(&dest).unwrap(), *data);
        fs::remove_file(&dest).unwrap();
    }

    #[test]
    fn cancel_removes_partial_file() {
        let data = test_data(2 * 1024 * 1024);
        let url = serve(data, true);
        let dest = temp("cancel.bin");
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = download_to_file(&Request { url, headers: vec![] }, &dest, Options::with_connections(4), &cancel, &mut |_| {})
            .unwrap_err();
        assert_eq!(err, "отменено");
        assert!(!dest.exists() && !partial_path(&dest).exists());
    }
}
