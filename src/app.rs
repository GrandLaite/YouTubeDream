//! Контроллер окна: связывает Slint-интерфейс с ядром загрузчика.
//!
//! Всё состояние живёт в `Controller` на UI-потоке. Долгие операции
//! (yt-dlp, сеть) идут в рабочих потоках и возвращают результат через
//! `post`, который выполняет замыкание на UI-потоке.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use slint::{
    CloseRequestResponse, ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString,
    Timer, TimerMode, VecModel,
};
use lader_core::cancel::CancelToken;
use lader_core::components::{self, Component, Installed, Stage};
use lader_core::settings::{self, CONNECTION_CHOICES, COOKIE_BROWSERS, Settings};
use lader_core::ytdlp::{
    self, AudioFormat, BatchEvent, BatchSummary, ChannelEntry, ChannelInfo, Cookies, DownloadOptions, Outcome,
    PAGE_SIZE, Progress, Stream, Target, Tools, VideoContainer, VideoInfo,
};
use lader_core::process::open_in_file_manager;
use lader_core::{format, http, urls};

use crate::{AppState, AppWindow, ChannelRow, ComponentRow, DialogIcon, JobState, Screen, Tone};

const VIDEO_CONTAINERS: [VideoContainer; 3] = [VideoContainer::Mp4, VideoContainer::Webm, VideoContainer::Mkv];
const AUDIO_FORMATS: [AudioFormat; 2] = [AudioFormat::Mp3, AudioFormat::M4a];
/// Высоты для пунктов «Лучшее», «1080p», …, «360p» пакетной загрузки.
const BATCH_HEIGHTS: [u32; 5] = [9999, 1080, 720, 480, 360];
/// Как часто перепроверять наличие новой версии yt-dlp, пока окно открыто.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);

thread_local! {
    static CONTROLLER: RefCell<Option<Controller>> = const { RefCell::new(None) };
}

/// Выполняет действие над контроллером на UI-потоке. Если контроллер уже
/// занят (вложенный вызов), действие откладывается до следующей итерации
/// цикла событий, а не теряется.
fn with(f: impl FnOnce(&mut Controller) + 'static) {
    CONTROLLER.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => {
            if let Some(c) = guard.as_mut() {
                f(c);
            }
        }
        Err(_) => Timer::single_shot(Duration::ZERO, move || with(f)),
    });
}

/// Передаёт действие из рабочего потока на UI-поток.
fn post(f: impl FnOnce(&mut Controller) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || with(f));
}

pub fn init(ui: &AppWindow) {
    let controller = Controller::new(ui.clone_strong());
    CONTROLLER.with(|cell| *cell.borrow_mut() = Some(controller));
    wire_callbacks(ui);
    with(Controller::start);
}

fn wire_callbacks(ui: &AppWindow) {
    let s = ui.global::<AppState>();
    s.on_open_components(|| with(Controller::open_components));
    s.on_close_components(|| with(Controller::close_components));
    s.on_check_updates(|| with(|c| c.check_updates(true)));
    s.on_install_missing(|| with(Controller::install_missing));
    s.on_install_component(|i| with(move |c| c.install_component(i)));
    s.on_cancel_install(|| with(Controller::cancel_install));
    s.on_open_components_folder(|| with(Controller::open_components_folder));
    s.on_banner_action(|| with(Controller::banner_action));
    s.on_banner_dismiss(|| with(Controller::banner_dismiss));
    s.on_fetch(|| with(Controller::fetch));
    s.on_cookies_changed(|i| with(move |c| c.set_cookies(i)));
    s.on_connections_changed(|i| with(move |c| c.set_connections(i)));
    s.on_root_dir_edited(|text| with(move |c| c.root_dir_edited(text.to_string())));
    s.on_browse(|| with(Controller::browse));
    s.on_quality_selected(|i| with(move |c| c.quality_selected(i)));
    s.on_download(|| with(Controller::download));
    s.on_batch_quality_selected(|i| with(move |c| c.batch_quality_selected(i)));
    s.on_toggle_row(|i| with(move |c| c.toggle_row(i)));
    s.on_select_all(|v| with(move |c| c.select_all(v)));
    s.on_load_more(|| with(Controller::load_more));
    s.on_download_batch(|| with(Controller::download_batch));
    s.on_cancel(|| with(Controller::cancel));
    s.on_open_folder(|| with(Controller::open_folder));
    s.on_new_download(|| with(Controller::new_download));
    s.on_dialog_action(|primary| with(move |c| c.dialog_action(primary)));

    // Ответ нужен синхронно, поэтому без `with`.
    ui.window().on_close_requested(|| {
        CONTROLLER.with(|cell| match cell.try_borrow_mut() {
            Ok(mut guard) => guard.as_mut().map_or(CloseRequestResponse::HideWindow, Controller::close_requested),
            Err(_) => CloseRequestResponse::KeepWindowShown,
        })
    });
}

// ─── Состояние ───────────────────────────────────────────────────────────────

/// Что повторить после установки компонентов или по кнопке «Повторить».
#[derive(Clone)]
enum Retry {
    Fetch(String),
    Single { opts: DownloadOptions, title: String },
    Batch { entries: Vec<ChannelEntry>, opts: DownloadOptions },
}

/// Идущая установка компонентов — по одному, по порядку.
struct Install {
    order: Vec<Component>,
    index: usize,
    stage: Option<Stage>,
    failed: BTreeMap<Component, String>,
    cancel: CancelToken,
    /// Обновление уже работающего yt-dlp, а не установка недостающего.
    update: bool,
}

/// Чем закончилась последняя установка — для баннера.
enum InstallOutcome {
    Done { update: bool },
    Failed { failed: BTreeMap<Component, String>, cancelled: bool },
}

/// Содержимое баннера над окном. `key` различает «одинаковые» баннеры:
/// скрытый крестиком баннер не возвращается, пока не сменится ключ.
struct Banner {
    key: String,
    tone: Tone,
    title: String,
    text: String,
    button: String,
    /// Some — показать индикатор; внутри — доля, если она известна.
    progress: Option<Option<f32>>,
    dismissable: bool,
}

enum Action {
    Nothing,
    /// Обновить yt-dlp, затем повторить действие.
    UpdateThen(Retry),
    Retry(Retry),
    CreateChannelFolder(PathBuf),
    UseBatchDir(PathBuf),
    Exit,
}

struct Dialog {
    icon: DialogIcon,
    title: String,
    text: String,
    primary: String,
    cancel: String,
    on_primary: Action,
    on_cancel: Action,
}

impl Dialog {
    fn message(icon: DialogIcon, title: &str, text: impl Into<String>) -> Self {
        Self {
            icon,
            title: title.into(),
            text: text.into(),
            primary: "OK".into(),
            cancel: String::new(),
            on_primary: Action::Nothing,
            on_cancel: Action::Nothing,
        }
    }
}

struct LoadedVideo {
    url: String,
    info: VideoInfo,
}

struct ChannelState {
    /// Адрес, по которому подгружаются страницы (с нормализованной вкладкой).
    url: String,
    entries: Vec<ChannelEntry>,
    loading_more: bool,
}

enum JobKind {
    Single { opts: DownloadOptions, title: String },
    Batch { entries: Vec<ChannelEntry>, opts: DownloadOptions, failed: usize, finished: usize },
}

struct Job {
    cancel: CancelToken,
    kind: JobKind,
}

struct Controller {
    ui: AppWindow,
    settings: Settings,
    save_timer: Timer,
    check_timer: Timer,
    retry_check_timer: Timer,
    banner_timer: Timer,

    // Компоненты
    detected: bool,
    found: BTreeMap<Component, Installed>,
    latest_ytdlp: Option<String>,
    checking: bool,
    /// Сколько фоновых проверок версии подряд не удалось.
    check_failures: u32,
    install: Option<Install>,
    install_seq: u64,
    outcome: Option<InstallOutcome>,
    dismissed_banner: Option<String>,
    /// Действие, которое выполнится после успешной установки компонентов.
    pending: Option<Retry>,
    /// Ссылка, нажатая до того, как закончился поиск компонентов при запуске.
    waiting_fetch: Option<String>,
    resumed: bool,
    component_rows: Rc<VecModel<ComponentRow>>,
    screen_before_components: Screen,
    /// Экран компонентов открылся сам из-за нехватки — закрыть его, когда всё поставится.
    components_auto_close: bool,

    // Получение информации
    fetching: bool,
    screen_before_fetch: Screen,
    video: Option<LoadedVideo>,
    channel: Option<ChannelState>,
    rows: Rc<VecModel<ChannelRow>>,
    thumbs: ThumbLoader,
    single_audio: bool,
    batch_audio: bool,
    browsing: bool,

    // Загрузка
    job: Option<Job>,
    back_screen: Screen,
    last_file: Option<PathBuf>,
    active_dir: PathBuf,
    exit_after_job: bool,

    dialog: Option<Dialog>,
    dialog_queue: VecDeque<Dialog>,
}

impl Controller {
    fn new(ui: AppWindow) -> Self {
        Self {
            ui,
            settings: Settings::load(),
            save_timer: Timer::default(),
            check_timer: Timer::default(),
            retry_check_timer: Timer::default(),
            banner_timer: Timer::default(),
            detected: false,
            found: BTreeMap::new(),
            latest_ytdlp: None,
            checking: false,
            check_failures: 0,
            install: None,
            install_seq: 0,
            outcome: None,
            dismissed_banner: None,
            pending: None,
            waiting_fetch: None,
            resumed: false,
            component_rows: Rc::new(VecModel::default()),
            screen_before_components: Screen::Empty,
            components_auto_close: false,
            fetching: false,
            screen_before_fetch: Screen::Empty,
            video: None,
            channel: None,
            rows: Rc::new(VecModel::default()),
            thumbs: ThumbLoader::new(),
            single_audio: false,
            batch_audio: false,
            browsing: false,
            job: None,
            back_screen: Screen::Empty,
            last_file: None,
            active_dir: PathBuf::new(),
            exit_after_job: false,
            dialog: None,
            dialog_queue: VecDeque::new(),
        }
    }

    fn start(&mut self) {
        let s = self.ui.global::<AppState>();
        let labels: Vec<SharedString> = COOKIE_BROWSERS.iter().map(|(label, _)| (*label).into()).collect();
        s.set_cookie_options(ModelRc::new(VecModel::from(labels)));
        s.set_cookies_index(settings::browser_index(&self.settings.cookies_browser) as i32);
        let choices: Vec<SharedString> = CONNECTION_CHOICES.iter().map(|n| n.to_string().into()).collect();
        s.set_connection_options(ModelRc::new(VecModel::from(choices)));
        let current = CONNECTION_CHOICES.iter().position(|&n| n == self.settings.connections);
        let default = CONNECTION_CHOICES.iter().position(|&n| n == settings::DEFAULT_CONNECTIONS).unwrap_or(0);
        s.set_connections_index(current.unwrap_or(default) as i32);
        let dir: SharedString = self.settings.out_dir.as_str().into();
        s.set_root_dir(dir.clone());
        s.set_single_dir(dir.clone());
        s.set_batch_dir(dir);
        s.set_channel_rows(ModelRc::from(self.rows.clone()));
        for c in Component::ALL {
            self.component_rows.push(self.row(c));
        }
        s.set_component_rows(ModelRc::from(self.component_rows.clone()));
        s.set_components_folder(
            components::bin_dir().map(|d| d.display().to_string()).unwrap_or_default().into(),
        );

        self.detect_tools();
        // Приложение могут держать открытым днями — перепроверяем периодически.
        self.check_timer
            .start(TimerMode::Repeated, UPDATE_CHECK_INTERVAL, || with(|c| c.check_updates(false)));
    }

    // ─── Компоненты: поиск и проверка версии ─────────────────────────────────

    /// Ищет yt-dlp, Deno и ffmpeg, затем сверяет версию yt-dlp с последней.
    fn detect_tools(&mut self) {
        self.checking = true;
        self.refresh();
        thread::spawn(|| {
            let found: BTreeMap<Component, Installed> =
                Component::ALL.iter().filter_map(|&c| components::find(c).map(|i| (c, i))).collect();
            post(move |c| c.on_detected(found));
            let latest = components::latest_ytdlp_version();
            post(move |c| c.on_latest(latest, false));
        });
    }

    fn on_detected(&mut self, found: BTreeMap<Component, Installed>) {
        let first = !self.detected;
        self.detected = true;
        if self.install.is_none() {
            self.found = found;
        }
        self.refresh();
        if let Some(url) = self.waiting_fetch.take() {
            // Пользователь нажал «Получить» раньше, чем закончился поиск.
            self.end_fetch(false);
            self.run_fetch(url);
        } else if first && !self.missing().is_empty() {
            // Как лаунчер игры: при первом запуске сразу показываем, чего не хватает.
            self.open_components_screen(true);
        }
    }

    fn check_updates(&mut self, user: bool) {
        if self.checking || self.install.is_some() {
            return;
        }
        if user {
            self.dismissed_banner = None;
            if matches!(self.outcome, Some(InstallOutcome::Failed { .. })) {
                self.outcome = None;
            }
            // По явному запросу ищем компоненты заново: их могли поставить
            // или обновить вручную, пока приложение было открыто.
            if self.job.is_none() && !self.fetching {
                self.detect_tools();
                return;
            }
        }
        self.checking = true;
        self.refresh();
        thread::spawn(move || {
            let latest = components::latest_ytdlp_version();
            post(move |c| c.on_latest(latest, user));
        });
    }

    fn on_latest(&mut self, result: Result<String, String>, user: bool) {
        self.checking = false;
        match result {
            Ok(version) => {
                self.check_failures = 0;
                self.set_latest(version);
            }
            Err(e) => {
                if user {
                    self.show_dialog(Dialog::message(
                        DialogIcon::Error,
                        "Не удалось проверить обновления",
                        format!("GitHub недоступен: {e}"),
                    ));
                }
                // Сеть могла моргнуть при запуске — пробуем ещё, с нарастающей
                // паузой, а не ждём следующей плановой проверки через сутки.
                if let Some(&delay) = [30u64, 120, 600].get(self.check_failures as usize) {
                    self.retry_check_timer.start(TimerMode::SingleShot, Duration::from_secs(delay), || {
                        with(|c| c.check_updates(false))
                    });
                }
                self.check_failures += 1;
            }
        }
        self.refresh();
    }

    fn set_latest(&mut self, version: String) {
        let newer = self.latest_ytdlp.as_deref().is_none_or(|known| components::is_newer(&version, known));
        if newer {
            self.latest_ytdlp = Some(version);
        }
    }

    fn update_available(&self) -> bool {
        matches!(
            (self.found.get(&Component::YtDlp), &self.latest_ytdlp),
            (Some(i), Some(l)) if components::is_newer(l, &i.version)
        )
    }

    fn missing(&self) -> Vec<Component> {
        Component::ALL.into_iter().filter(|c| !self.found.contains_key(c)).collect()
    }

    fn tools(&self) -> Option<Tools> {
        let path = |c| self.found.get(&c).map(|i: &Installed| i.path.clone());
        Some(Tools { ytdlp: path(Component::YtDlp)?, ffmpeg: path(Component::Ffmpeg), deno: path(Component::Deno) })
    }

    /// Программы для действия. Если каких-то компонентов не хватает —
    /// открывает экран установки; после неё действие продолжится само.
    fn ready_tools(&mut self, retry: Retry) -> Option<Tools> {
        if self.missing().is_empty()
            && let Some(tools) = self.tools()
        {
            return Some(tools);
        }
        self.pending = Some(retry);
        self.open_components_screen(true);
        None
    }

    fn run_retry(&mut self, retry: Retry) {
        match retry {
            Retry::Fetch(url) => {
                self.ui.global::<AppState>().set_url(url.as_str().into());
                self.run_fetch(url);
            }
            Retry::Single { opts, title } => self.start_single(opts, title),
            Retry::Batch { entries, opts } => self.start_batch(entries, opts),
        }
    }

    // ─── Компоненты: установка ───────────────────────────────────────────────

    fn install_missing(&mut self) {
        let order = self.missing();
        self.start_install(order, false);
    }

    fn update_ytdlp(&mut self) {
        self.start_install(vec![Component::YtDlp], true);
    }

    fn install_component(&mut self, index: i32) {
        let Some(&c) = Component::ALL.get(index.max(0) as usize) else { return };
        let update = c == Component::YtDlp && self.update_available();
        self.start_install(vec![c], update);
    }

    fn start_install(&mut self, order: Vec<Component>, update: bool) {
        if order.is_empty() || self.install.is_some() || self.job.is_some() || self.fetching {
            return;
        }
        self.outcome = None;
        self.resumed = false;
        self.banner_timer.stop();
        self.install_seq += 1;
        let cancel = CancelToken::new();
        let connections = self.settings.connections;
        self.install = Some(Install {
            order: order.clone(),
            index: 0,
            stage: None,
            failed: BTreeMap::new(),
            cancel: cancel.clone(),
            update,
        });
        self.refresh();
        thread::spawn(move || {
            for (index, c) in order.into_iter().enumerate() {
                if cancel.is_cancelled() {
                    break;
                }
                post(move |ctl| ctl.on_install_step(index));
                let result = components::install(c, connections, &cancel, &mut |stage| {
                    post(move |ctl| ctl.on_install_stage(c, stage))
                });
                post(move |ctl| ctl.on_component_installed(c, result));
            }
            post(Controller::on_install_finished);
        });
    }

    fn on_install_step(&mut self, index: usize) {
        if let Some(inst) = self.install.as_mut() {
            inst.index = index;
            inst.stage = None;
        }
        self.refresh();
    }

    fn on_install_stage(&mut self, c: Component, stage: Stage) {
        let Some(inst) = self.install.as_mut() else { return };
        if inst.order.get(inst.index) == Some(&c) {
            inst.stage = Some(stage);
            self.refresh();
        }
    }

    fn on_component_installed(&mut self, c: Component, result: Result<Installed, String>) {
        let Some(inst) = self.install.as_mut() else { return };
        match result {
            Ok(installed) => {
                if c == Component::YtDlp {
                    let version = installed.version.clone();
                    self.found.insert(c, installed);
                    self.set_latest(version);
                } else {
                    self.found.insert(c, installed);
                }
            }
            Err(e) if !inst.cancel.is_cancelled() => {
                inst.failed.insert(c, e);
            }
            Err(_) => {}
        }
        self.refresh();
    }

    fn on_install_finished(&mut self) {
        let Some(inst) = self.install.take() else { return };
        let cancelled = inst.cancel.is_cancelled();
        if cancelled || !inst.failed.is_empty() {
            self.outcome = Some(InstallOutcome::Failed { failed: inst.failed, cancelled });
            self.refresh();
            return;
        }
        self.outcome = Some(InstallOutcome::Done { update: inst.update });
        let complete = self.missing().is_empty();
        let retry = if complete { self.pending.take() } else { None };
        self.resumed = retry.is_some();
        // Экран открылся сам из-за нехватки компонентов — теперь всё на месте,
        // возвращаемся туда, откуда пришли.
        if complete && self.components_auto_close && self.ui.global::<AppState>().get_screen() == Screen::Components {
            self.close_components();
        }
        self.banner_timer
            .start(TimerMode::SingleShot, Duration::from_secs(6), || with(Controller::finish_done_banner));
        self.refresh();
        if let Some(retry) = retry {
            self.run_retry(retry);
        }
    }

    /// Убирает баннер «готово» через несколько секунд.
    fn finish_done_banner(&mut self) {
        if matches!(self.outcome, Some(InstallOutcome::Done { .. })) {
            self.outcome = None;
            self.resumed = false;
            self.refresh();
        }
    }

    fn cancel_install(&mut self) {
        if let Some(inst) = &self.install {
            inst.cancel.cancel();
        }
    }

    // ─── Компоненты: экран, баннер, строка статуса ───────────────────────────

    /// Клик по строке статуса: открывает экран компонентов, повторный клик
    /// возвращает на прежний экран. Проверка обновлений — только по кнопке
    /// на самом экране.
    fn open_components(&mut self) {
        if self.ui.global::<AppState>().get_screen() == Screen::Components {
            self.close_components();
        } else {
            self.open_components_screen(false);
        }
    }

    fn open_components_screen(&mut self, auto_close: bool) {
        let s = self.ui.global::<AppState>();
        if s.get_screen() != Screen::Components {
            self.screen_before_components = s.get_screen();
            self.components_auto_close = auto_close;
        }
        s.set_screen(Screen::Components);
        self.refresh();
    }

    fn close_components(&mut self) {
        let s = self.ui.global::<AppState>();
        if s.get_screen() == Screen::Components {
            s.set_screen(self.screen_before_components);
        }
        self.components_auto_close = false;
        self.refresh();
    }

    fn open_components_folder(&mut self) {
        if let Some(dir) = components::bin_dir() {
            let _ = std::fs::create_dir_all(&dir);
            open_in_file_manager(&dir, None);
        }
    }

    fn banner_action(&mut self) {
        if self.install.is_some() {
            self.cancel_install();
            return;
        }
        if matches!(self.outcome, Some(InstallOutcome::Failed { .. })) {
            self.outcome = None;
        }
        if !self.missing().is_empty() {
            self.install_missing();
        } else if self.update_available() {
            self.update_ytdlp();
        }
        self.refresh();
    }

    fn banner_dismiss(&mut self) {
        if let Some(banner) = self.banner() {
            self.dismissed_banner = Some(banner.key);
        }
        if matches!(self.outcome, Some(InstallOutcome::Done { .. })) {
            self.finish_done_banner();
        }
        self.refresh();
    }

    /// Переносит состояние компонентов в интерфейс: строку статуса в шапке,
    /// баннер и строки экрана компонентов.
    fn refresh(&mut self) {
        let s = self.ui.global::<AppState>();
        let (pill, tone) = self.pill();
        s.set_pill_text(pill.into());
        s.set_pill_tone(tone);

        // На экране компонентов баннер повторял бы то же самое.
        let banner = self
            .banner()
            .filter(|b| self.dismissed_banner.as_deref() != Some(b.key.as_str()))
            .filter(|_| s.get_screen() != Screen::Components);
        s.set_banner_visible(banner.is_some());
        if let Some(b) = banner {
            s.set_banner_tone(b.tone);
            s.set_banner_title(b.title.into());
            s.set_banner_text(b.text.into());
            s.set_banner_button(b.button.into());
            s.set_banner_dismissable(b.dismissable);
            s.set_banner_progress_visible(b.progress.is_some());
            let fraction = b.progress.flatten();
            s.set_banner_progress_known(fraction.is_some());
            s.set_banner_progress(fraction.unwrap_or(0.0));
        }

        for (i, c) in Component::ALL.into_iter().enumerate() {
            if i < self.component_rows.row_count() {
                self.component_rows.set_row_data(i, self.row(c));
            }
        }
        let missing = self.missing();
        s.set_components_setup(self.detected && !missing.is_empty());
        s.set_installing(self.install.is_some());
        s.set_retry_pending(self.pending.is_some());
        let mb: u32 = missing.iter().map(|c| c.approx_size_mb()).sum();
        s.set_missing_size(if missing.is_empty() { String::new() } else { format!("около {mb} МБ") }.into());
    }

    /// Строка статуса в шапке. Подробности (версии, пути) — на экране
    /// компонентов, который открывается кликом по ней.
    fn pill(&self) -> (String, Tone) {
        if self.install.is_some() {
            return ("Установка компонентов…".into(), Tone::Info);
        }
        if !self.detected || self.checking {
            return ("Проверяется актуальность компонентов…".into(), Tone::Neutral);
        }
        let missing = self.missing();
        if !missing.is_empty() {
            let tone = if missing.contains(&Component::YtDlp) { Tone::Danger } else { Tone::Warning };
            return (format!("Не хватает компонентов: {}", names(&missing)), tone);
        }
        if self.update_available() {
            return ("Доступно обновление yt-dlp".into(), Tone::Warning);
        }
        if self.latest_ytdlp.is_none() {
            return ("Не удалось проверить обновления".into(), Tone::Neutral);
        }
        ("Компоненты актуальны".into(), Tone::Success)
    }

    fn banner(&self) -> Option<Banner> {
        if let Some(inst) = &self.install {
            let c = inst.order[inst.index.min(inst.order.len() - 1)];
            let title = if inst.update {
                match &self.latest_ytdlp {
                    Some(v) => format!("Обновление yt-dlp до {v}…"),
                    None => "Обновление yt-dlp…".into(),
                }
            } else if inst.order.len() > 1 {
                format!("Установка компонентов: {} ({} из {})…", c.title(), inst.index + 1, inst.order.len())
            } else {
                format!("Установка {}…", c.title())
            };
            let mut text = stage_text(inst.stage);
            if self.pending.is_some() {
                text.push_str(" · затем всё продолжится автоматически");
            }
            return Some(Banner {
                key: format!("install:{}", self.install_seq),
                tone: Tone::Info,
                title,
                text,
                button: "Отмена".into(),
                progress: Some(stage_fraction(inst.stage)),
                dismissable: false,
            });
        }
        match &self.outcome {
            Some(InstallOutcome::Done { update }) => {
                let title = match (update, self.found.get(&Component::YtDlp)) {
                    (true, Some(i)) => format!("yt-dlp обновлён до {}", i.version),
                    _ => "Компоненты установлены".into(),
                };
                return Some(Banner {
                    key: format!("done:{}", self.install_seq),
                    tone: Tone::Success,
                    title,
                    text: if self.resumed { "Продолжаем…" } else { "Можно скачивать." }.into(),
                    button: String::new(),
                    progress: None,
                    dismissable: true,
                });
            }
            Some(InstallOutcome::Failed { failed, cancelled }) => {
                let (title, text) = if *cancelled {
                    ("Установка отменена".to_string(), "Недокачанное удалено — можно начать заново.".to_string())
                } else {
                    let list: Vec<Component> = failed.keys().copied().collect();
                    let first = failed.values().next().map(|e| first_line(e)).unwrap_or_default();
                    (format!("Не удалось установить: {}", names(&list)), first)
                };
                return Some(Banner {
                    key: format!("failed:{}", self.install_seq),
                    tone: Tone::Warning,
                    title,
                    text,
                    button: "Повторить".into(),
                    progress: None,
                    dismissable: true,
                });
            }
            None => {}
        }
        if !self.detected {
            return None;
        }
        let missing = self.missing();
        if !missing.is_empty() {
            let mb: u32 = missing.iter().map(|c| c.approx_size_mb()).sum();
            return Some(Banner {
                key: format!("missing:{}", names(&missing)),
                tone: Tone::Warning,
                title: format!("Не хватает компонентов: {}", names(&missing)),
                text: format!(
                    "Без них приложение не сможет скачивать видео. Они скачаются с официальных страниц \
                     (около {mb} МБ) и подключатся сами."
                ),
                button: "Установить".into(),
                progress: None,
                dismissable: true,
            });
        }
        if self.update_available() {
            let latest = self.latest_ytdlp.clone().unwrap_or_default();
            let current = self.found.get(&Component::YtDlp).map(|i| i.version.clone()).unwrap_or_default();
            return Some(Banner {
                key: format!("update:{latest}"),
                tone: Tone::Info,
                title: format!("Доступна новая версия yt-dlp: {latest}"),
                text: format!(
                    "У вас {current}. Старые версии часто перестают работать с YouTube. Новая версия \
                     скачается (около 18 МБ), проверится и подключится сама."
                ),
                button: "Обновить".into(),
                progress: None,
                dismissable: true,
            });
        }
        None
    }

    /// Строка экрана компонентов.
    fn row(&self, c: Component) -> ComponentRow {
        let mut row = ComponentRow {
            title: c.title().into(),
            purpose: c.purpose().into(),
            status: SharedString::new(),
            tone: Tone::Neutral,
            action: SharedString::new(),
            busy: false,
            progress: 0.0,
            progress_known: false,
        };
        if let Some(inst) = &self.install {
            if inst.order.get(inst.index) == Some(&c) {
                row.status = stage_text(inst.stage).into();
                row.tone = Tone::Info;
                row.busy = true;
                if let Some(f) = stage_fraction(inst.stage) {
                    row.progress = f;
                    row.progress_known = true;
                }
                return row;
            }
            if inst.order.iter().skip(inst.index + 1).any(|&q| q == c) {
                row.status = "В очереди".into();
                return row;
            }
            if let Some(e) = inst.failed.get(&c) {
                row.status = format!("Ошибка: {}", first_line(e)).into();
                row.tone = Tone::Danger;
                return row;
            }
        }
        if let Some(InstallOutcome::Failed { failed, .. }) = &self.outcome
            && let Some(e) = failed.get(&c)
            && !self.found.contains_key(&c)
        {
            row.status = format!("Ошибка: {}", first_line(e)).into();
            row.tone = Tone::Danger;
            row.action = "Повторить".into();
            return row;
        }
        let update = c == Component::YtDlp && self.update_available();
        let latest = self.latest_ytdlp.as_deref().unwrap_or("");
        match self.found.get(&c) {
            Some(i) => {
                let mut status = if i.managed { "Установлен" } else { "Найден в системе" }.to_string();
                status.push_str(&format!(" · {}", i.version));
                if update {
                    status.push_str(&format!(" · доступна {latest}"));
                }
                if !i.managed {
                    status.push_str(&format!("
{}", i.path.display()));
                }
                row.status = status.into();
                row.tone = if update { Tone::Warning } else { Tone::Success };
                row.action = match (update, i.managed) {
                    (true, _) => "Обновить",
                    (false, true) => "Переустановить",
                    (false, false) => "Скачать свою копию",
                }
                .into();
            }
            None if !self.detected => row.status = "Проверка…".into(),
            None => {
                row.status = format!("Не установлен · около {} МБ", c.approx_size_mb()).into();
                row.tone = Tone::Warning;
                row.action = "Установить".into();
            }
        }
        row
    }

    // ─── Получение информации ─────────────────────────────────────────────────

    fn busy(&self) -> bool {
        self.fetching || self.job.is_some() || self.install.is_some()
    }

    fn fetch(&mut self) {
        if self.busy() {
            return;
        }
        let url = self.ui.global::<AppState>().get_url().trim().to_string();
        if url.is_empty() {
            self.show_dialog(Dialog::message(
                DialogIcon::Info,
                "Введите ссылку",
                "Вставьте ссылку на видео, плейлист или канал YouTube либо на твит X с видео.",
            ));
            return;
        }
        // Лента профиля X не поддерживается yt-dlp — говорим об этом сразу,
        // а не через невнятную ошибку «Unsupported URL».
        if urls::is_twitter_profile_url(&url) {
            self.show_dialog(Dialog::message(
                DialogIcon::Error,
                "Это ссылка на профиль X",
                "Выгрузка всей ленты не поддерживается — откройте нужный твит и вставьте ссылку вида \
                 x.com/username/status/…",
            ));
            return;
        }
        // Новое действие пользователя отменяет отложенный повтор.
        self.pending = None;
        self.refresh();
        self.run_fetch(url);
    }

    fn run_fetch(&mut self, url: String) {
        if !self.detected {
            // Поиск компонентов при запуске ещё идёт — продолжим сразу после него.
            self.begin_fetch("Проверяем компоненты…");
            self.waiting_fetch = Some(url);
            return;
        }
        let Some(tools) = self.ready_tools(Retry::Fetch(url.clone())) else { return };
        let is_channel = urls::is_channel_url(&url);
        self.begin_fetch(if is_channel { "Загружаем список видео…" } else { "Получаем информацию о видео…" });
        let cookies = Cookies::from_browser(&self.settings.cookies_browser);
        thread::spawn(move || {
            if is_channel {
                let list_url = urls::normalize_channel_url(&url);
                let result = ytdlp::get_channel_videos(&tools, &list_url, 0, PAGE_SIZE, &cookies);
                let fresh = latest_if_outdated(&result);
                post(move |c| c.on_channel_loaded(url, list_url, result, fresh));
            } else {
                let result = ytdlp::get_video_info(&tools, &url, &cookies);
                let fresh = latest_if_outdated(&result);
                post(move |c| c.on_video_loaded(url, result, fresh));
            }
        });
    }

    fn begin_fetch(&mut self, text: &str) {
        let s = self.ui.global::<AppState>();
        if !self.fetching {
            self.screen_before_fetch = s.get_screen();
        }
        self.fetching = true;
        s.set_fetching(true);
        s.set_loading_text(text.into());
        s.set_screen(Screen::Loading);
    }

    /// Завершает получение информации; при ошибке возвращает прежний экран.
    fn end_fetch(&mut self, ok: bool) {
        self.fetching = false;
        let s = self.ui.global::<AppState>();
        s.set_fetching(false);
        if !ok {
            s.set_screen(self.screen_before_fetch);
        }
    }

    fn apply_fresh_latest(&mut self, fresh: Option<String>) {
        if let Some(version) = fresh {
            self.set_latest(version);
            self.refresh();
        }
    }

    fn on_video_loaded(&mut self, url: String, result: Result<VideoInfo, String>, fresh: Option<String>) {
        self.apply_fresh_latest(fresh);
        let info = match result {
            Ok(info) => info,
            Err(e) => {
                self.end_fetch(false);
                self.failure_dialog("Не удалось получить информацию о видео", &e, &url, Retry::Fetch(url.clone()));
                return;
            }
        };
        self.end_fetch(true);

        let s = self.ui.global::<AppState>();
        s.set_video_title(info.title.as_str().into());
        s.set_video_uploader(info.uploader.as_str().into());
        s.set_video_duration(format::duration(info.duration).into());
        s.set_video_thumb(Image::default());

        let mut labels: Vec<SharedString> = info.qualities.iter().map(|q| q.label.as_str().into()).collect();
        labels.push("Только аудио".into());
        self.single_audio = labels.len() == 1;
        s.set_quality_options(ModelRc::new(VecModel::from(labels)));
        s.set_quality_index(0);
        s.set_format_index(0);
        s.set_single_dir(self.settings.out_dir.as_str().into());

        let generation = self.thumbs.next_video();
        if !info.thumbnail.is_empty() {
            self.thumbs.load(ThumbTarget::Video, generation, info.thumbnail.clone());
        }
        self.video = Some(LoadedVideo { url, info });
        s.set_screen(Screen::Video);
    }

    fn on_channel_loaded(
        &mut self,
        url: String,
        list_url: String,
        result: Result<ChannelInfo, String>,
        fresh: Option<String>,
    ) {
        self.apply_fresh_latest(fresh);
        let info = match result {
            Ok(info) => info,
            Err(e) => {
                self.end_fetch(false);
                self.failure_dialog("Не удалось загрузить список видео", &e, &url, Retry::Fetch(url.clone()));
                return;
            }
        };
        self.end_fetch(true);

        let title = if info.title.is_empty() { info.uploader.clone() } else { info.title.clone() };
        let s = self.ui.global::<AppState>();
        s.set_channel_title(title.into());
        s.set_batch_dir(self.settings.out_dir.as_str().into());

        self.thumbs.next_channel();
        self.rows.set_vec(Vec::new());
        self.channel = Some(ChannelState { url: list_url, entries: Vec::new(), loading_more: false });
        self.append_entries(info.entries.clone());
        self.ui.global::<AppState>().set_screen(Screen::Channel);

        if info.entries.is_empty() {
            self.show_dialog(Dialog::message(DialogIcon::Info, "Видео не найдены", "В этом списке нет доступных видео."));
            return;
        }
        let folder = if info.uploader.is_empty() { &info.title } else { &info.uploader };
        self.ask_channel_folder(folder);
    }

    fn append_entries(&mut self, entries: Vec<ChannelEntry>) {
        let Some(channel) = self.channel.as_mut() else { return };
        let start = channel.entries.len();
        let generation = self.thumbs.channel_generation();
        for (offset, entry) in entries.iter().enumerate() {
            let index = start + offset;
            self.rows.push(ChannelRow {
                checked: true,
                number: (index + 1) as i32,
                title: entry.title.as_str().into(),
                duration: format::duration(entry.duration).into(),
                thumb: Image::default(),
            });
            if !entry.thumbnail.is_empty() {
                self.thumbs.load(ThumbTarget::Row(index), generation, entry.thumbnail.clone());
            }
        }
        let more = entries.len() >= PAGE_SIZE;
        channel.entries.extend(entries);
        let s = self.ui.global::<AppState>();
        s.set_can_load_more(more);
        s.set_loading_more(false);
        self.update_selected_count();
    }

    /// Спрашивает, создать ли подпапку с названием канала. База — всегда
    /// корневая папка из шапки, иначе при загрузке двух каналов подряд
    /// папка второго создавалась бы внутри папки первого.
    fn ask_channel_folder(&mut self, channel_name: &str) {
        let name = format::sanitize_folder_name(channel_name);
        let base = self.settings.out_dir.trim();
        if name == "channel" || base.is_empty() {
            return;
        }
        let base = PathBuf::from(settings::normalize_dir(base));
        // Корневая папка уже названа как канал — подпапка не нужна.
        if base.file_name().is_some_and(|n| n.to_string_lossy() == name) {
            self.ui.global::<AppState>().set_batch_dir(base.to_string_lossy().as_ref().into());
            return;
        }
        let target = base.join(&name);
        self.show_dialog(Dialog {
            icon: DialogIcon::Question,
            title: "Папка для канала".into(),
            text: format!("Создать папку «{name}» в\n{}\nи скачать видео канала туда?", base.display()),
            primary: "Создать папку".into(),
            cancel: "Нет, в корневую".into(),
            on_primary: Action::CreateChannelFolder(target),
            on_cancel: Action::UseBatchDir(base),
        });
    }

    fn load_more(&mut self) {
        let Some(tools) = self.tools() else { return };
        let Some(channel) = self.channel.as_mut() else { return };
        if channel.loading_more {
            return;
        }
        channel.loading_more = true;
        self.ui.global::<AppState>().set_loading_more(true);
        let (url, offset) = (channel.url.clone(), channel.entries.len());
        let cookies = Cookies::from_browser(&self.settings.cookies_browser);
        thread::spawn(move || {
            let result = ytdlp::get_channel_videos(&tools, &url, offset, PAGE_SIZE, &cookies);
            post(move |c| c.on_more_loaded(url, result));
        });
    }

    fn on_more_loaded(&mut self, url: String, result: Result<ChannelInfo, String>) {
        let Some(channel) = self.channel.as_mut() else { return };
        if channel.url != url {
            return; // за это время открыли другой канал
        }
        channel.loading_more = false;
        self.ui.global::<AppState>().set_loading_more(false);
        match result {
            Ok(info) => self.append_entries(info.entries),
            Err(e) => self.show_dialog(Dialog::message(
                DialogIcon::Error,
                "Не удалось загрузить ещё видео",
                ytdlp::clean_error(&e),
            )),
        }
    }

    fn toggle_row(&mut self, index: i32) {
        let index = index as usize;
        if let Some(mut row) = self.rows.row_data(index) {
            row.checked = !row.checked;
            self.rows.set_row_data(index, row);
            self.update_selected_count();
        }
    }

    fn select_all(&mut self, checked: bool) {
        for index in 0..self.rows.row_count() {
            if let Some(mut row) = self.rows.row_data(index)
                && row.checked != checked
            {
                row.checked = checked;
                self.rows.set_row_data(index, row);
            }
        }
        self.update_selected_count();
    }

    fn update_selected_count(&self) {
        let count = self.rows.iter().filter(|r| r.checked).count();
        self.ui.global::<AppState>().set_selected_count(count as i32);
    }

    fn quality_selected(&mut self, index: i32) {
        // Переключение между видео и аудио меняет список форматов — сбрасываем
        // формат на первый (MP4 или MP3), иначе индекс указал бы в никуда.
        let audio = self.video.as_ref().is_some_and(|v| index as usize >= v.info.qualities.len());
        if audio != self.single_audio {
            self.single_audio = audio;
            self.ui.global::<AppState>().set_format_index(0);
        }
    }

    fn batch_quality_selected(&mut self, index: i32) {
        let audio = index as usize >= BATCH_HEIGHTS.len();
        if audio != self.batch_audio {
            self.batch_audio = audio;
            self.ui.global::<AppState>().set_batch_format_index(0);
        }
    }

    // ─── Загрузка ─────────────────────────────────────────────────────────────

    fn download(&mut self) {
        if self.busy() {
            return;
        }
        let Some(video) = &self.video else { return };
        let s = self.ui.global::<AppState>();
        let quality = s.get_quality_index().max(0) as usize;
        let fmt = s.get_format_index().max(0) as usize;
        let target = match video.info.qualities.get(quality) {
            Some(q) => Target::Video { max_height: q.height, container: VIDEO_CONTAINERS[fmt.min(2)] },
            None => Target::Audio(AUDIO_FORMATS[fmt.min(1)]),
        };
        let opts = DownloadOptions {
            url: video.url.clone(),
            target,
            out_dir: self.dir_or_root(&s.get_single_dir()),
            cookies: Cookies::from_browser(&self.settings.cookies_browser),
            connections: self.settings.connections,
        };
        let title = video.info.title.clone();
        self.pending = None;
        self.refresh();
        self.start_single(opts, title);
    }

    fn download_batch(&mut self) {
        if self.busy() {
            return;
        }
        let Some(channel) = &self.channel else { return };
        let entries: Vec<ChannelEntry> = self
            .rows
            .iter()
            .zip(&channel.entries)
            .filter(|(row, _)| row.checked)
            .map(|(_, entry)| entry.clone())
            .collect();
        if entries.is_empty() {
            self.show_dialog(Dialog::message(
                DialogIcon::Info,
                "Ничего не выбрано",
                "Отметьте хотя бы одно видео для скачивания.",
            ));
            return;
        }
        let s = self.ui.global::<AppState>();
        let quality = s.get_batch_quality_index().max(0) as usize;
        let fmt = s.get_batch_format_index().max(0) as usize;
        let target = match BATCH_HEIGHTS.get(quality) {
            Some(&max_height) => Target::Video { max_height, container: VIDEO_CONTAINERS[fmt.min(2)] },
            None => Target::Audio(AUDIO_FORMATS[fmt.min(1)]),
        };
        let opts = DownloadOptions {
            url: String::new(),
            target,
            out_dir: self.dir_or_root(&s.get_batch_dir()),
            cookies: Cookies::from_browser(&self.settings.cookies_browser),
            connections: self.settings.connections,
        };
        self.pending = None;
        self.refresh();
        self.start_batch(entries, opts);
    }

    fn dir_or_root(&self, text: &str) -> PathBuf {
        let dir = if text.trim().is_empty() { self.settings.out_dir.as_str() } else { text };
        PathBuf::from(settings::normalize_dir(dir))
    }

    /// Переключает экран на прогресс и сбрасывает показатели.
    fn begin_job(&mut self, back: Screen, heading: String, subtitle: &str, out_dir: &Path) {
        self.back_screen = back;
        self.last_file = None;
        self.active_dir = out_dir.to_path_buf();
        let s = self.ui.global::<AppState>();
        s.set_job_running(true);
        s.set_job_state(JobState::Running);
        s.set_job_heading(heading.into());
        s.set_job_subtitle(subtitle.into());
        s.set_job_status("Запуск yt-dlp…".into());
        s.set_can_open_folder(false);
        self.reset_progress();
        s.set_screen(Screen::Progress);
    }

    fn reset_progress(&self) {
        let s = self.ui.global::<AppState>();
        s.set_job_progress(0.0);
        s.set_job_progress_known(true);
        s.set_job_percent("0%".into());
        s.set_job_size("".into());
        s.set_job_speed("".into());
        s.set_job_eta("".into());
    }

    fn start_single(&mut self, opts: DownloadOptions, title: String) {
        let Some(tools) = self.ready_tools(Retry::Single { opts: opts.clone(), title: title.clone() }) else { return };
        self.begin_job(Screen::Video, "Загрузка…".into(), &title, &opts.out_dir);
        let cancel = CancelToken::new();
        self.job = Some(Job { cancel: cancel.clone(), kind: JobKind::Single { opts: opts.clone(), title } });
        thread::spawn(move || {
            let outcome = ytdlp::download(&tools, &opts, &cancel, &mut |p| post(move |c| c.on_progress(p)));
            post(move |c| c.on_single_finished(outcome));
        });
    }

    fn start_batch(&mut self, entries: Vec<ChannelEntry>, opts: DownloadOptions) {
        let Some(tools) = self.ready_tools(Retry::Batch { entries: entries.clone(), opts: opts.clone() }) else {
            return;
        };
        let first = entries.first().map(|e| e.title.clone()).unwrap_or_default();
        self.begin_job(Screen::Channel, format!("Видео 1 из {}", entries.len()), &first, &opts.out_dir);
        let cancel = CancelToken::new();
        self.job = Some(Job {
            cancel: cancel.clone(),
            kind: JobKind::Batch { entries: entries.clone(), opts: opts.clone(), failed: 0, finished: 0 },
        });
        thread::spawn(move || {
            let summary =
                ytdlp::download_batch(&tools, &entries, &opts, &cancel, &mut |e| post(move |c| c.on_batch_event(e)));
            post(move |c| c.on_batch_finished(summary));
        });
    }

    fn is_batch(&self) -> bool {
        matches!(self.job, Some(Job { kind: JobKind::Batch { .. }, .. }))
    }

    fn on_progress(&mut self, progress: Progress) {
        if self.job.is_none() {
            return;
        }
        let s = self.ui.global::<AppState>();
        match progress {
            Progress::Status(text) => s.set_job_status(text.into()),
            Progress::Download(p) => {
                match p.percent {
                    Some(pct) => {
                        s.set_job_progress_known(true);
                        s.set_job_progress((pct / 100.0) as f32);
                        s.set_job_percent(format!("{pct:.1}%").into());
                    }
                    None => {
                        s.set_job_progress_known(false);
                        s.set_job_percent("".into());
                    }
                }
                if let Some(size) = p.size {
                    s.set_job_size(size.into());
                }
                if let Some(speed) = p.speed {
                    s.set_job_speed(speed.into());
                }
                if let Some(eta) = p.eta {
                    s.set_job_eta(eta.into());
                }
                s.set_job_status(
                    match p.stream {
                        Some(Stream::Video) => "Загрузка видео",
                        Some(Stream::Audio) => "Загрузка звука",
                        None => "Загрузка…",
                    }
                    .into(),
                );
            }
            Progress::Merging => {
                if !self.is_batch() {
                    s.set_job_heading("Объединение…".into());
                }
                s.set_job_status("Склейка видео и звука…".into());
                s.set_job_progress_known(true);
                s.set_job_progress(0.99);
                s.set_job_percent("99%".into());
                s.set_job_speed("".into());
                s.set_job_eta("".into());
            }
        }
    }

    fn on_single_finished(&mut self, outcome: Outcome) {
        let Some(Job { kind: JobKind::Single { opts, title }, .. }) = self.job.take() else { return };
        let s = self.ui.global::<AppState>();
        s.set_job_running(false);
        s.set_job_speed("".into());
        s.set_job_eta("".into());
        match outcome {
            Outcome::Done { file, already_downloaded } => {
                s.set_job_state(JobState::Done);
                s.set_job_heading("Готово".into());
                s.set_job_progress_known(true);
                s.set_job_progress(1.0);
                s.set_job_percent("100%".into());
                let status = match (&file, already_downloaded) {
                    (_, true) => "Уже скачано — пропущено".to_string(),
                    (Some(f), false) => format!("Сохранено: {}", f.file_name().unwrap_or_default().to_string_lossy()),
                    (None, false) => "Загрузка завершена".to_string(),
                };
                s.set_job_status(status.into());
                s.set_can_open_folder(true);
                self.last_file = file;
            }
            Outcome::Failed(error) => {
                s.set_job_state(JobState::Failed);
                s.set_job_heading("Ошибка".into());
                s.set_job_progress(0.0);
                s.set_job_percent("".into());
                s.set_job_status(first_line(&ytdlp::clean_error(&error)).into());
                let url = opts.url.clone();
                self.failure_dialog("Не удалось скачать видео", &error, &url, Retry::Single { opts, title });
            }
            Outcome::Cancelled => self.show_cancelled("Недокачанные файлы удалены".into()),
        }
        self.after_job();
    }

    fn on_batch_event(&mut self, event: BatchEvent) {
        let Some(Job { kind: JobKind::Batch { failed, finished, .. }, .. }) = self.job.as_mut() else { return };
        match event {
            BatchEvent::Started { index, total, title } => {
                let mut heading = format!("Видео {} из {total}", index + 1);
                if *failed > 0 {
                    heading.push_str(&format!(" · ошибок: {failed}"));
                }
                let s = self.ui.global::<AppState>();
                s.set_job_heading(heading.into());
                s.set_job_subtitle(title.into());
                s.set_job_status("Подготовка…".into());
                self.reset_progress();
            }
            BatchEvent::Progress(p) => self.on_progress(p),
            BatchEvent::VideoDone { already_downloaded, .. } => {
                *finished += 1;
                if already_downloaded {
                    self.ui.global::<AppState>().set_job_status("Уже скачано — пропущено".into());
                }
            }
            BatchEvent::VideoFailed { .. } => *failed += 1,
        }
    }

    fn on_batch_finished(&mut self, summary: BatchSummary) {
        let Some(Job { kind: JobKind::Batch { entries, opts, finished, .. }, .. }) = self.job.take() else { return };
        let s = self.ui.global::<AppState>();
        s.set_job_running(false);
        s.set_job_speed("".into());
        s.set_job_eta("".into());

        if summary.cancelled {
            self.show_cancelled(format!("Скачано {finished} из {}", summary.total));
            self.ui.global::<AppState>().set_can_open_folder(finished > 0);
            self.after_job();
            return;
        }

        let failed = summary.failed.len();
        let ok = summary.total - failed;
        let mut heading = format!("Готово: {ok} из {}", summary.total);
        if failed > 0 {
            heading.push_str(&format!(" (ошибок: {failed})"));
        }
        s.set_job_state(if ok == 0 { JobState::Failed } else { JobState::Done });
        s.set_job_heading(heading.into());
        s.set_job_subtitle("".into());
        s.set_job_progress_known(true);
        s.set_job_progress(1.0);
        s.set_job_percent("100%".into());
        s.set_job_status(
            match (failed, ok) {
                (0, _) => "Все видео скачаны!".to_string(),
                (_, 0) => "Ни одно видео не скачалось.".to_string(),
                _ => format!("Скачано {ok}, пропущено {failed}."),
            }
            .into(),
        );
        s.set_can_open_folder(true);

        if failed > 0 {
            let details = summary
                .failed
                .iter()
                .map(|(i, e)| format!("• {}\n  {}", entries[*i].title, ytdlp::clean_error(e)))
                .collect::<Vec<_>>()
                .join("\n\n");
            let retry_entries = summary.failed.iter().map(|(i, _)| entries[*i].clone()).collect();
            let retry = Retry::Batch { entries: retry_entries, opts };
            let mut dialog = Dialog {
                icon: DialogIcon::Error,
                title: if ok == 0 { "Ни одно видео не скачалось".into() } else { "Не все видео скачались".into() },
                text: details,
                primary: format!("Повторить неудавшиеся ({failed})"),
                cancel: "Закрыть".into(),
                on_primary: Action::Retry(retry.clone()),
                on_cancel: Action::Nothing,
            };
            if self.update_available() {
                dialog.text.push_str(&self.update_hint());
                dialog.primary = "Обновить yt-dlp и повторить".into();
                dialog.on_primary = Action::UpdateThen(retry);
            }
            self.show_dialog(dialog);
        }
        self.after_job();
    }

    fn show_cancelled(&self, status: String) {
        let s = self.ui.global::<AppState>();
        s.set_job_state(JobState::Cancelled);
        s.set_job_heading("Отменено".into());
        s.set_job_percent("".into());
        s.set_job_status(status.into());
    }

    fn after_job(&mut self) {
        if self.exit_after_job {
            let _ = slint::quit_event_loop();
        }
    }

    fn cancel(&mut self) {
        let Some(job) = &self.job else { return };
        // taskkill занимает заметное время — не держим UI-поток.
        let token = job.cancel.clone();
        thread::spawn(move || token.cancel());
        let s = self.ui.global::<AppState>();
        s.set_job_state(JobState::Cancelling);
        s.set_job_heading("Отмена…".into());
    }

    fn new_download(&mut self) {
        self.ui.global::<AppState>().set_screen(self.back_screen);
    }

    fn open_folder(&mut self) {
        let dir = std::path::absolute(&self.active_dir).unwrap_or_else(|_| self.active_dir.clone());
        let file = self.last_file.as_ref().filter(|f| f.is_file()).cloned();
        open_in_file_manager(&dir, file.as_deref());
    }

    // ─── Ошибки ───────────────────────────────────────────────────────────────

    /// Диалог ошибки yt-dlp с подсказками: про cookies, если похоже на
    /// требование авторизации, и про обновление yt-dlp, если оно доступно —
    /// тогда основная кнопка обновляет yt-dlp и повторяет действие сама.
    fn failure_dialog(&mut self, title: &str, raw_error: &str, url: &str, retry: Retry) {
        let mut text = ytdlp::clean_error(raw_error);
        if self.settings.cookies_browser.is_empty() && urls::needs_auth_error(raw_error) {
            let site = if urls::is_twitter_url(url) { "X" } else { "Сайт" };
            text.push_str(&format!(
                "\n\n{site} требует авторизации для этого видео. Выберите браузер в поле «Cookies из браузера» — \
                 тот, где вы залогинены, — и повторите."
            ));
        }
        let mut dialog = Dialog::message(DialogIcon::Error, title, text);
        if self.update_available() {
            dialog.text.push_str(&self.update_hint());
            dialog.primary = "Обновить yt-dlp и повторить".into();
            dialog.cancel = "Закрыть".into();
            dialog.on_primary = Action::UpdateThen(retry);
        }
        self.show_dialog(dialog);
    }

    fn update_hint(&self) -> String {
        format!(
            "\n\nДоступна новая версия yt-dlp — {} (у вас {}). Устаревший yt-dlp — самая частая причина \
             таких ошибок. После обновления всё повторится автоматически.",
            self.latest_ytdlp.as_deref().unwrap_or("?"),
            self.found.get(&Component::YtDlp).map_or("?", |i| i.version.as_str()),
        )
    }

    // ─── Диалоги ──────────────────────────────────────────────────────────────

    fn show_dialog(&mut self, dialog: Dialog) {
        if self.dialog.is_some() {
            self.dialog_queue.push_back(dialog);
        } else {
            self.present(dialog);
        }
    }

    fn present(&mut self, dialog: Dialog) {
        let s = self.ui.global::<AppState>();
        s.set_dialog_icon(dialog.icon);
        s.set_dialog_title(dialog.title.as_str().into());
        s.set_dialog_text(dialog.text.as_str().into());
        s.set_dialog_primary(dialog.primary.as_str().into());
        s.set_dialog_cancel(dialog.cancel.as_str().into());
        s.set_dialog_open(true);
        self.dialog = Some(dialog);
    }

    fn dialog_action(&mut self, primary: bool) {
        let Some(dialog) = self.dialog.take() else { return };
        self.ui.global::<AppState>().set_dialog_open(false);
        match if primary { dialog.on_primary } else { dialog.on_cancel } {
            Action::Nothing => {}
            Action::UpdateThen(retry) => {
                self.pending = Some(retry);
                self.update_ytdlp();
            }
            Action::Retry(retry) => self.run_retry(retry),
            Action::CreateChannelFolder(target) => match std::fs::create_dir_all(&target) {
                Ok(()) => self.ui.global::<AppState>().set_batch_dir(target.to_string_lossy().as_ref().into()),
                Err(e) => self.show_dialog(Dialog::message(
                    DialogIcon::Error,
                    "Не удалось создать папку",
                    format!("{}: {e}", target.display()),
                )),
            },
            Action::UseBatchDir(dir) => self.ui.global::<AppState>().set_batch_dir(dir.to_string_lossy().as_ref().into()),
            Action::Exit => {
                self.exit_after_job = true;
                if self.job.is_some() {
                    self.cancel();
                } else {
                    let _ = slint::quit_event_loop();
                }
            }
        }
        if self.dialog.is_none()
            && let Some(next) = self.dialog_queue.pop_front()
        {
            self.present(next);
        }
    }

    fn close_requested(&mut self) -> CloseRequestResponse {
        if self.job.is_some() && !self.exit_after_job {
            self.show_dialog(Dialog {
                icon: DialogIcon::Question,
                title: "Идёт загрузка".into(),
                text: "Если закрыть приложение, загрузка прервётся, а недокачанные файлы будут удалены.".into(),
                primary: "Прервать и выйти".into(),
                cancel: "Продолжить загрузку".into(),
                on_primary: Action::Exit,
                on_cancel: Action::Nothing,
            });
            return CloseRequestResponse::KeepWindowShown;
        }
        self.save_settings();
        CloseRequestResponse::HideWindow
    }

    // ─── Настройки ────────────────────────────────────────────────────────────

    fn set_connections(&mut self, index: i32) {
        if let Some(&n) = CONNECTION_CHOICES.get(index.max(0) as usize) {
            self.settings.connections = n;
            self.save_settings();
        }
    }

    fn set_cookies(&mut self, index: i32) {
        let value = COOKIE_BROWSERS.get(index.max(0) as usize).map_or("", |(_, v)| v);
        self.settings.cookies_browser = value.to_string();
        self.save_settings();
    }

    /// Ручной ввод корневой папки: запоминаем и синхронизируем папки
    /// одиночной и пакетной загрузки.
    fn root_dir_edited(&mut self, text: String) {
        let s = self.ui.global::<AppState>();
        s.set_single_dir(text.as_str().into());
        s.set_batch_dir(text.as_str().into());
        if text.trim().is_empty() {
            return;
        }
        self.settings.out_dir = text.trim().to_string();
        // Сохраняем не на каждую букву, а после паузы в наборе.
        self.save_timer.start(TimerMode::SingleShot, Duration::from_millis(700), || with(|c| c.save_settings()));
    }

    fn set_root_dir(&mut self, dir: &Path) {
        let dir = settings::normalize_dir(&dir.to_string_lossy());
        if dir.is_empty() {
            return;
        }
        self.settings.out_dir = dir.clone();
        self.save_settings();
        let s = self.ui.global::<AppState>();
        let dir: SharedString = dir.into();
        s.set_root_dir(dir.clone());
        s.set_single_dir(dir.clone());
        s.set_batch_dir(dir);
    }

    fn save_settings(&mut self) {
        self.save_timer.stop();
        if let Err(e) = self.settings.save() {
            eprintln!("не удалось сохранить настройки: {e}");
        }
    }

    /// Выбор папки. Нативный диалог открывается в отдельном потоке (так
    /// устроен асинхронный rfd на Windows), окно приложения остаётся живым.
    fn browse(&mut self) {
        if self.browsing {
            return;
        }
        self.browsing = true;
        let start = self.settings.out_dir.clone();
        let handle = self.ui.window().window_handle();
        let spawned = slint::spawn_local(async move {
            let mut dialog = rfd::AsyncFileDialog::new().set_title("Папка для загрузок").set_parent(&handle);
            if Path::new(&start).is_dir() {
                dialog = dialog.set_directory(&start);
            }
            let picked = dialog.pick_folder().await;
            with(move |c| {
                c.browsing = false;
                if let Some(folder) = picked {
                    c.set_root_dir(folder.path());
                }
            });
        });
        if spawned.is_err() {
            self.browsing = false;
        }
    }
}

/// Если ошибка похожа на последствия устаревшего yt-dlp, сразу сверяемся
/// с GitHub — чтобы диалог ошибки мог предложить обновление.
fn latest_if_outdated<T>(result: &Result<T, String>) -> Option<String> {
    match result {
        Err(e) if urls::looks_outdated(e) => components::latest_ytdlp_version().ok(),
        _ => None,
    }
}

fn first_line(s: &str) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} МБ", bytes as f64 / (1024.0 * 1024.0))
}

/// "Deno, ffmpeg".
fn names(list: &[Component]) -> String {
    list.iter().map(|c| c.title()).collect::<Vec<_>>().join(", ")
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

/// Доля выполнения этапа, если её можно посчитать.
fn stage_fraction(stage: Option<Stage>) -> Option<f32> {
    match stage {
        Some(Stage::Downloading { done, total: Some(total), .. }) if total > 0 => Some(done as f32 / total as f32),
        _ => None,
    }
}

// ─── Превью ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum ThumbTarget {
    Video,
    Row(usize),
}

struct ThumbJob {
    target: ThumbTarget,
    generation: u64,
    url: String,
}

/// Фоновая загрузка превью несколькими потоками. Поколения отсекают
/// устаревшие результаты: пока грузились превью одного канала, могли
/// открыть другой.
struct ThumbLoader {
    tx: mpsc::Sender<ThumbJob>,
    video_gen: Arc<AtomicU64>,
    channel_gen: Arc<AtomicU64>,
}

impl ThumbLoader {
    const WORKERS: usize = 6;

    fn new() -> Self {
        let (tx, rx) = mpsc::channel::<ThumbJob>();
        let rx = Arc::new(Mutex::new(rx));
        let video_gen = Arc::new(AtomicU64::new(0));
        let channel_gen = Arc::new(AtomicU64::new(0));
        for _ in 0..Self::WORKERS {
            let (rx, video_gen, channel_gen) = (rx.clone(), video_gen.clone(), channel_gen.clone());
            thread::spawn(move || {
                loop {
                    let job = match rx.lock() {
                        Ok(guard) => guard.recv(),
                        Err(_) => return,
                    };
                    let Ok(job) = job else { return };
                    let (current, (w, h)) = match job.target {
                        ThumbTarget::Video => (&video_gen, (384, 216)),
                        ThumbTarget::Row(_) => (&channel_gen, (184, 104)),
                    };
                    let stale = || current.load(Ordering::SeqCst) != job.generation;
                    if stale() {
                        continue;
                    }
                    // CDN превью на нестабильной сети иногда рвёт соединение —
                    // пара повторов убирает «дырявые» списки.
                    let mut result = http::fetch_thumbnail(&job.url, w, h);
                    for delay in [500, 1500] {
                        if result.is_ok() || stale() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(delay));
                        result = http::fetch_thumbnail(&job.url, w, h);
                    }
                    if let Ok(img) = result {
                        post(move |c| c.on_thumb(job.target, job.generation, img));
                    }
                }
            });
        }
        Self { tx, video_gen, channel_gen }
    }

    fn next_video(&self) -> u64 {
        self.video_gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn next_channel(&self) -> u64 {
        self.channel_gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn channel_generation(&self) -> u64 {
        self.channel_gen.load(Ordering::SeqCst)
    }

    fn is_current(&self, target: ThumbTarget, generation: u64) -> bool {
        let current = match target {
            ThumbTarget::Video => &self.video_gen,
            ThumbTarget::Row(_) => &self.channel_gen,
        };
        current.load(Ordering::SeqCst) == generation
    }

    fn load(&self, target: ThumbTarget, generation: u64, url: String) {
        let _ = self.tx.send(ThumbJob { target, generation, url });
    }
}

impl Controller {
    fn on_thumb(&mut self, target: ThumbTarget, generation: u64, img: image::RgbaImage) {
        if !self.thumbs.is_current(target, generation) {
            return;
        }
        let (w, h) = img.dimensions();
        let image = Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(img.as_raw(), w, h));
        match target {
            ThumbTarget::Video => self.ui.global::<AppState>().set_video_thumb(image),
            ThumbTarget::Row(index) => {
                if let Some(mut row) = self.rows.row_data(index) {
                    row.thumb = image;
                    self.rows.set_row_data(index, row);
                }
            }
        }
    }
}
