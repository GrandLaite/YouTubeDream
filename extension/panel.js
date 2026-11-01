// Панель Lader: те же экраны, что у окна приложения (app.rs + app.slint),
// только загрузки идут очередью. Всю работу делает помощник lader-host,
// с ним панель говорит через фоновую часть (background.js).

import { isChannelUrl, isDownloadable, isTwitterUrl, playlistOf } from './lib/urls.js';
import { DEFAULT_SETTINGS, loadSettings, saveSettings } from './lib/settings.js';

const VIDEO_CONTAINERS = ['mp4', 'webm', 'mkv'];
const AUDIO_FORMATS = ['mp3', 'm4a'];
const VIDEO_FORMAT_LABELS = ['MP4', 'WebM', 'MKV'];
const AUDIO_FORMAT_LABELS = ['MP3', 'M4A'];
const BATCH_QUALITIES = ['Лучшее', '1080p', '720p', '480p', '360p', 'Только аудио'];
/** Высоты для пунктов «Лучшее», «1080p», …, «360p» пакетной загрузки. */
const BATCH_HEIGHTS = [9999, 1080, 720, 480, 360];
const CONNECTION_CHOICES = [1, 2, 4, 8, 16];
const ACTIVE = new Set(['queued', 'running', 'cancelling']);
const RELEASES = 'https://github.com/GrandLaite/YouTubeDream/releases';

const FORMAT_HINTS = {
  MP4: 'H.264 + AAC без перекодирования — открывается везде.',
  WebM: 'VP9/AV1 + Opus — родные кодеки YouTube, склейка без потерь.',
  MKV: 'Лучшее качество: любые кодеки без перекодирования.',
  MP3: 'Аудио конвертируется в MP3 (небольшая потеря качества).',
  M4A: 'Родной AAC YouTube и X — без перекодирования, лучшее качество аудио.',
};

const $ = (sel) => document.querySelector(sel);
const params = new URLSearchParams(location.search);
document.body.dataset.mode = params.get('mode') || 'side';
const isEdge = (navigator.userAgentData?.brands || []).some((b) => /edge/i.test(b.brand));

// ─── Состояние ──────────────────────────────────────────────────────────────

const app = {
  host: { status: 'off', error: '', info: null },
  components: null,
  /** Загрузки по id, в порядке добавления. */
  jobs: new Map(),
  settings: { ...DEFAULT_SETTINGS },
  tab: 'get',
  /** Экран поверх вкладок: settings или components. */
  overlay: null,
  /** Экран компонентов открылся сам из-за нехватки. Закрыть, когда всё поставится. */
  componentsAutoClose: false,
  get: { view: 'empty', loadingText: '', video: null, channel: null },
  fetching: false,
  /** Ссылка, пришедшая, пока шло получение предыдущей. */
  queuedUrl: null,
  /** Действие, которое продолжится после установки компонентов. */
  pending: null,
  resumed: false,
  dismissedBanner: null,
  installSeq: 0,
  doneUntil: 0,
  /** Загрузки, о провале которых уже показан диалог. */
  reported: new Set(),
  dialogs: [],
  dialog: null,
};

// ─── Связь с фоновой частью ─────────────────────────────────────────────────

let bg = null;
let nextRid = 1;
const calls = new Map();
let hostReadyWaiters = [];

function connectBackground() {
  bg = chrome.runtime.connect({ name: 'panel' });
  bg.onMessage.addListener(onBackgroundMessage);
  bg.onDisconnect.addListener(() => {
    for (const c of calls.values()) c.reject({ error: 'Расширение перезапустилось' });
    calls.clear();
    // Фоновая часть перезапустилась (обновили расширение), подключаемся заново.
    setTimeout(connectBackground, 500);
  });
  chrome.windows?.getCurrent().then((w) => bg.postMessage({ type: 'hello', windowId: w.id, mode: document.body.dataset.mode })).catch(() => {});
}

function request(type, payload = {}) {
  return new Promise((resolve, reject) => {
    const rid = nextRid++;
    calls.set(rid, { resolve, reject });
    bg.postMessage({ type, rid, ...payload });
  });
}

function hostCall(cmd, params = {}) {
  return request('call', { cmd, params });
}

function onBackgroundMessage(msg) {
  switch (msg.type) {
    case 'reply': {
      const c = calls.get(msg.rid);
      if (!c) return;
      calls.delete(msg.rid);
      if (msg.ok) c.resolve(msg.data);
      else c.reject(msg.error || {});
      return;
    }
    case 'state':
      app.host = msg.host;
      setComponents(msg.components);
      app.jobs = new Map(msg.jobs.map((j) => [j.id, j]));
      for (const j of msg.jobs) if (!ACTIVE.has(j.view?.state)) app.reported.add(j.id);
      onHostChanged();
      renderAll();
      return;
    case 'host':
      app.host = msg.host;
      onHostChanged();
      renderAll();
      return;
    case 'components':
      setComponents(msg.components);
      renderAll();
      return;
    case 'job':
      onJob(msg.job);
      return;
    case 'removed':
      for (const id of msg.ids) app.jobs.delete(id);
      renderJobs();
      return;
    case 'load':
      loadUrl(msg.url);
      return;
  }
}

function onHostChanged() {
  if (app.host.status === 'ready') {
    const waiters = hostReadyWaiters;
    hostReadyWaiters = [];
    for (const f of waiters) f();
  }
}

function whenHostReady(f) {
  if (app.host.status === 'ready') f();
  else hostReadyWaiters.push(f);
}

function hostReady() {
  return app.host.status === 'ready';
}

// ─── Компоненты ─────────────────────────────────────────────────────────────

function setComponents(next) {
  const prev = app.components;
  app.components = next;
  if (prev?.install && next && !next.install) onInstallFinished(next);
}

function onInstallFinished(c) {
  app.installSeq += 1;
  if (c.outcome?.type !== 'done') return;
  // Баннер «готово» убирается сам через несколько секунд, как в приложении.
  app.doneUntil = Date.now() + 6000;
  setTimeout(renderAll, 6100);
  const retry = c.missing.length === 0 ? app.pending : null;
  app.pending = null;
  app.resumed = Boolean(retry);
  if (c.missing.length === 0 && app.componentsAutoClose && app.overlay === 'components') {
    app.overlay = null;
    app.componentsAutoClose = false;
  }
  if (retry) setTimeout(() => runRetry(retry), 0);
}

function item(key) {
  return app.components?.items.find((i) => i.key === key);
}

/** "Deno, ffmpeg". */
function names(keys) {
  return keys.map((k) => item(k)?.title || k).join(', ');
}

function missingMb(c) {
  return c.missing.reduce((sum, k) => sum + (item(k)?.approx_mb || 0), 0);
}

function busyJobs() {
  return [...app.jobs.values()].some((j) => j.view?.state === 'running' || j.view?.state === 'cancelling');
}

async function install(which) {
  try {
    await hostCall('install', { which, connections: app.settings.connections });
  } catch (e) {
    showDialog({ icon: 'error', title: 'Не удалось начать установку', text: e.error || String(e) });
  }
}

function installMissing() {
  install('missing');
}

function updateYtdlp() {
  install('yt-dlp');
}

function cancelInstall() {
  hostCall('cancel_install').catch(() => {});
}

function openComponents(auto) {
  if (app.overlay !== 'components') app.componentsAutoClose = auto;
  app.overlay = 'components';
  renderAll();
}

/** Строка статуса внизу, как в шапке приложения. */
function pill() {
  const h = app.host.status;
  if (h === 'off' || h === 'connecting') return ['Подключение к помощнику…', 'neutral'];
  if (h === 'missing') return ['Помощник не установлен', 'danger'];
  if (h === 'forbidden') return ['Помощник не разрешает это расширение', 'danger'];
  if (h !== 'ready') return ['Помощник не отвечает', 'danger'];
  const c = app.components;
  if (c?.install) return ['Установка компонентов…', 'info'];
  if (!c || !c.detected || c.checking) return ['Проверяется актуальность компонентов…', 'neutral'];
  if (c.missing.length) {
    return [`Не хватает компонентов: ${names(c.missing)}`, c.missing.includes('yt-dlp') ? 'danger' : 'warning'];
  }
  if (c.update_available) return ['Доступно обновление yt-dlp', 'warning'];
  if (!c.latest) return ['Не удалось проверить обновления', 'neutral'];
  return ['Компоненты актуальны', 'success'];
}

/** Баннер над экраном: установка, её итог, нехватка компонентов, обновление. */
function banner() {
  const c = app.components;
  if (!hostReady() || !c) return null;
  if (c.install) {
    const i = c.install;
    const current = item(i.current)?.title || i.current;
    let title;
    if (i.update) title = c.latest ? `Обновление yt-dlp до ${c.latest}…` : 'Обновление yt-dlp…';
    else if (i.order.length > 1) title = `Установка компонентов: ${current} (${i.index + 1} из ${i.order.length})…`;
    else title = `Установка ${current}…`;
    let text = i.stage;
    if (app.pending) text += ' · затем всё продолжится автоматически';
    return {
      key: `install:${app.installSeq}`,
      tone: 'info',
      title,
      text,
      button: 'Отмена',
      action: cancelInstall,
      progress: i.fraction ?? -1,
      dismissable: false,
    };
  }
  if (c.outcome?.type === 'done' && Date.now() < app.doneUntil) {
    const yt = item('yt-dlp')?.installed;
    return {
      key: `done:${app.installSeq}`,
      tone: 'success',
      title: c.outcome.update && yt ? `yt-dlp обновлён до ${yt.version}` : 'Компоненты установлены',
      text: app.resumed ? 'Продолжаем…' : 'Можно скачивать.',
      dismissable: true,
    };
  }
  if (c.outcome?.type === 'failed') {
    const failed = Object.keys(c.outcome.failed);
    return {
      key: `failed:${app.installSeq}`,
      tone: 'warning',
      title: c.outcome.cancelled ? 'Установка отменена' : `Не удалось установить: ${names(failed)}`,
      text: c.outcome.cancelled ? 'Недокачанное удалено — можно начать заново.' : Object.values(c.outcome.failed)[0] || '',
      button: 'Повторить',
      action: bannerAction,
      dismissable: true,
    };
  }
  if (!c.detected) return null;
  if (c.missing.length) {
    return {
      key: `missing:${names(c.missing)}`,
      tone: 'warning',
      title: `Не хватает компонентов: ${names(c.missing)}`,
      text: `Без них не получится скачивать видео. Они скачаются с официальных страниц (около ${missingMb(c)} МБ) и подключатся сами.`,
      button: 'Установить',
      action: bannerAction,
      dismissable: true,
    };
  }
  if (c.update_available) {
    return {
      key: `update:${c.latest}`,
      tone: 'info',
      title: `Доступна новая версия yt-dlp: ${c.latest}`,
      text: `У вас ${item('yt-dlp')?.installed?.version}. Старые версии часто перестают работать с YouTube. Новая версия скачается (около 18 МБ), проверится и подключится сама.`,
      button: 'Обновить',
      action: bannerAction,
      dismissable: true,
    };
  }
  return null;
}

function bannerAction() {
  const c = app.components;
  if (!c) return;
  if (c.missing.length) installMissing();
  else if (c.update_available) updateYtdlp();
  else if (c.outcome?.type === 'failed') install('missing');
}

/** Строка экрана компонентов, как `Controller::row` в приложении. */
function componentRow(it) {
  const c = app.components;
  const row = { status: '', path: '', tone: 'neutral', action: '', busy: false, progress: null };
  const inst = c.install;
  if (inst) {
    if (inst.current === it.key) {
      return { ...row, status: inst.stage, tone: 'info', busy: true, progress: inst.fraction ?? -1 };
    }
    if (inst.order.slice(inst.index + 1).includes(it.key)) return { ...row, status: 'В очереди' };
    if (inst.failed[it.key]) return { ...row, status: `Ошибка: ${inst.failed[it.key]}`, tone: 'danger' };
  }
  if (c.outcome?.type === 'failed' && c.outcome.failed[it.key] && !it.installed) {
    return { ...row, status: `Ошибка: ${c.outcome.failed[it.key]}`, tone: 'danger', action: 'Повторить' };
  }
  const update = it.key === 'yt-dlp' && c.update_available;
  if (it.installed) {
    const i = it.installed;
    let status = `${i.managed ? 'Установлен' : 'Найден в системе'} · ${i.version}`;
    if (update) status += ` · доступна ${c.latest}`;
    return {
      ...row,
      status,
      path: i.managed ? '' : i.path,
      tone: update ? 'warning' : 'success',
      action: update ? 'Обновить' : i.managed ? 'Переустановить' : 'Скачать свою копию',
    };
  }
  if (!c.detected) return { ...row, status: 'Проверка…' };
  return { ...row, status: `Не установлен · около ${it.approx_mb} МБ`, tone: 'warning', action: 'Установить' };
}

// ─── Проверка готовности ────────────────────────────────────────────────────

/**
 * Можно ли начинать работу. Если не хватает компонентов, открывает экран
 * установки и запоминает действие: после установки оно продолжится само.
 */
function readyFor(retry) {
  if (!hostReady()) {
    app.overlay = null;
    renderAll();
    if (app.host.status === 'off' || app.host.status === 'connecting') {
      whenHostReady(() => runRetry(retry));
    }
    return false;
  }
  const c = app.components;
  if (c?.install) {
    showDialog({ icon: 'info', title: 'Идёт установка компонентов', text: 'Дождитесь её окончания, затем можно будет продолжить.' });
    return false;
  }
  if (c?.detected && c.missing.length) {
    app.pending = retry;
    openComponents(true);
    return false;
  }
  return true;
}

function runRetry(retry) {
  switch (retry.type) {
    case 'fetch':
      fetchInfo(retry.url);
      break;
    case 'job':
      startJob(retry.spec, retry.thumb);
      break;
    case 'retry':
      retryJob(retry.id, retry.failedOnly);
      break;
  }
}

// ─── Получение информации ───────────────────────────────────────────────────

function setGetView(view, loadingText = '') {
  app.get.view = view;
  app.get.loadingText = loadingText;
  renderGet();
}

function loadUrl(url) {
  if (!url) return;
  app.tab = 'get';
  app.overlay = null;
  $('#url').value = url;
  renderAll();
  if (app.fetching) app.queuedUrl = url;
  else whenHostReady(() => fetchInfo(url));
}

async function fetchInfo(raw) {
  const url = (raw ?? $('#url').value).trim();
  if (app.fetching) return;
  if (!url) {
    showDialog({ icon: 'info', title: 'Введите ссылку', text: 'Вставьте ссылку на видео, плейлист или канал YouTube либо на твит X с видео.' });
    return;
  }
  $('#url').value = url;
  app.tab = 'get';
  if (!readyFor({ type: 'fetch', url })) return;
  app.pending = null;
  const before = app.get.view === 'loading' ? 'empty' : app.get.view;
  app.fetching = true;
  setGetView('loading', isChannelUrl(url) ? 'Загружаем список видео…' : 'Получаем информацию о видео…');
  renderAll();
  try {
    const data = await hostCall('info', { url });
    if (data.kind === 'channel') await showChannel(data);
    else showVideo(data);
  } catch (e) {
    setGetView(before);
    if (e.code === 'components') {
      app.pending = { type: 'fetch', url };
      openComponents(true);
    } else if (e.code === 'host') {
      renderAll();
    } else if (e.code === 'empty' || e.code === 'twitter_profile' || e.code === 'installing') {
      showDialog({ icon: e.code === 'empty' ? 'info' : 'error', title: e.title, text: e.error });
    } else {
      failureDialog(e.title || 'Не удалось получить информацию о видео', e, url, { type: 'fetch', url });
    }
  } finally {
    app.fetching = false;
    renderAll();
    const next = app.queuedUrl;
    app.queuedUrl = null;
    if (next && next !== url) loadUrl(next);
  }
}

function showVideo(data) {
  app.get.video = data;
  $('#video-thumb').src = data.thumbnail || '';
  $('#video-duration').textContent = data.duration || '';
  $('#video-title').textContent = data.title || data.url;
  $('#video-uploader').textContent = data.uploader || '';
  $('#video-playlist').hidden = !playlistOf(data.url);
  fillSelect($('#quality'), [...data.qualities.map((q) => q.label), 'Только аудио'], 0);
  updateSingleFormats(true);
  $('#single-dir').value = app.settings.out_dir;
  setGetView('video');
}

function singleIsAudio() {
  const v = app.get.video;
  return !v || $('#quality').selectedIndex >= v.qualities.length;
}

/** Переключение видео и звука меняет список форматов, сбрасываем на первый. */
function updateSingleFormats(reset) {
  const audio = singleIsAudio();
  const labels = audio ? AUDIO_FORMAT_LABELS : VIDEO_FORMAT_LABELS;
  const select = $('#format');
  if (reset || select.dataset.audio !== String(audio)) fillSelect(select, labels, 0);
  select.dataset.audio = String(audio);
  $('#format-hint').textContent = FORMAT_HINTS[labels[select.selectedIndex]] || '';
}

function updateBatchFormats(reset) {
  const audio = $('#batch-quality').selectedIndex >= BATCH_HEIGHTS.length;
  const labels = audio ? AUDIO_FORMAT_LABELS : VIDEO_FORMAT_LABELS;
  const select = $('#batch-format');
  if (reset || select.dataset.audio !== String(audio)) fillSelect(select, labels, 0);
  select.dataset.audio = String(audio);
  $('#batch-format-hint').textContent = FORMAT_HINTS[labels[select.selectedIndex]] || '';
}

async function showChannel(data) {
  app.get.channel = {
    url: data.url,
    listUrl: data.list_url,
    title: data.title,
    entries: [],
    checked: [],
    more: false,
    loadingMore: false,
  };
  $('#channel-rows').replaceChildren();
  $('#channel-title').textContent = data.title || data.url;
  $('#batch-dir').value = app.settings.out_dir;
  appendEntries(data.entries, data.more);
  setGetView('channel');
  if (!data.entries.length) {
    showDialog({ icon: 'info', title: 'Видео не найдены', text: 'В этом списке нет доступных видео.' });
    return;
  }
  await askChannelFolder(data.folder);
}

/**
 * Предлагает подпапку с названием канала. База всегда корневая папка
 * из настроек, иначе при загрузке двух каналов подряд папка второго
 * создавалась бы внутри папки первого.
 */
async function askChannelFolder(name) {
  let res;
  try {
    res = await hostCall('channel_folder', { base: app.settings.out_dir, name });
  } catch {
    return;
  }
  if (!res.ask) {
    if (res.dir) $('#batch-dir').value = res.dir;
    return;
  }
  showDialog({
    icon: 'question',
    title: 'Папка для канала',
    text: `Создать папку «${res.name}» в\n${res.base}\nи скачать видео канала туда?`,
    buttons: [
      {
        label: 'Создать папку',
        primary: true,
        action: async () => {
          try {
            const made = await hostCall('mkdir', { path: res.target });
            $('#batch-dir').value = made.path;
          } catch (e) {
            showDialog({ icon: 'error', title: e.title || 'Не удалось создать папку', text: e.error });
          }
        },
      },
      { label: 'Нет, в корневую', action: () => ($('#batch-dir').value = res.base) },
    ],
  });
}

function appendEntries(entries, more) {
  const ch = app.get.channel;
  const rows = $('#channel-rows');
  for (const entry of entries) {
    const index = ch.entries.length;
    ch.entries.push(entry);
    ch.checked.push(true);
    rows.append(channelRow(entry, index));
  }
  ch.more = more;
  renderChannelTools();
}

function channelRow(entry, index) {
  const row = el('div', { class: 'row', role: 'checkbox', tabindex: '0', 'aria-checked': 'true', 'data-index': index });
  const box = el('span', { class: 'box' });
  box.append(svg('i-tick'));
  const thumb = el('div', { class: 'row-thumb' });
  if (entry.thumbnail) {
    thumb.append(el('img', { src: entry.thumbnail, alt: '', loading: 'lazy', referrerpolicy: 'no-referrer' }));
  }
  if (entry.duration_text) thumb.append(el('span', { class: 'duration' }, entry.duration_text));
  row.append(box, el('span', { class: 'row-num' }, String(index + 1)), thumb, el('div', { class: 'row-title' }, entry.title || entry.url));
  return row;
}

function toggleRow(index, value) {
  const ch = app.get.channel;
  ch.checked[index] = value ?? !ch.checked[index];
  const row = $(`#channel-rows .row[data-index="${index}"]`);
  if (row) row.setAttribute('aria-checked', String(ch.checked[index]));
}

function selectAll(value) {
  const ch = app.get.channel;
  if (!ch) return;
  ch.checked.forEach((_, i) => toggleRow(i, value));
  renderChannelTools();
}

function selectedCount() {
  return app.get.channel?.checked.filter(Boolean).length || 0;
}

async function loadMore() {
  const ch = app.get.channel;
  if (!ch || ch.loadingMore) return;
  ch.loadingMore = true;
  renderChannelTools();
  try {
    const res = await hostCall('more', { list_url: ch.listUrl, offset: ch.entries.length });
    if (app.get.channel === ch) appendEntries(res.entries, res.more);
  } catch (e) {
    showDialog({ icon: 'error', title: e.title || 'Не удалось загрузить ещё видео', text: e.error });
  } finally {
    ch.loadingMore = false;
    renderChannelTools();
  }
}

// ─── Загрузка ───────────────────────────────────────────────────────────────

function dirOrRoot(text) {
  return text.trim() || app.settings.out_dir;
}

function downloadSingle() {
  const v = app.get.video;
  if (!v) return;
  const q = v.qualities[$('#quality').selectedIndex];
  const fmt = $('#format').selectedIndex;
  const target = q
    ? { type: 'video', height: q.height, container: VIDEO_CONTAINERS[Math.min(fmt, 2)] }
    : { type: 'audio', format: AUDIO_FORMATS[Math.min(fmt, 1)] };
  const spec = {
    kind: 'single',
    url: v.url,
    title: v.title,
    target,
    out_dir: dirOrRoot($('#single-dir').value),
    connections: app.settings.connections,
  };
  startJob(spec, v.thumbnail);
}

function downloadBatch() {
  const ch = app.get.channel;
  if (!ch) return;
  const entries = ch.entries.filter((_, i) => ch.checked[i]);
  if (!entries.length) {
    showDialog({ icon: 'info', title: 'Ничего не выбрано', text: 'Отметьте хотя бы одно видео для скачивания.' });
    return;
  }
  const q = $('#batch-quality').selectedIndex;
  const fmt = $('#batch-format').selectedIndex;
  const target =
    q < BATCH_HEIGHTS.length
      ? { type: 'video', height: BATCH_HEIGHTS[q], container: VIDEO_CONTAINERS[Math.min(fmt, 2)] }
      : { type: 'audio', format: AUDIO_FORMATS[Math.min(fmt, 1)] };
  const spec = {
    kind: 'batch',
    title: ch.title,
    entries: entries.map((e) => ({ id: e.id, url: e.url, title: e.title, duration: e.duration })),
    target,
    out_dir: dirOrRoot($('#batch-dir').value),
    connections: app.settings.connections,
  };
  startJob(spec, entries[0].thumbnail);
}

async function startJob(spec, thumb) {
  if (!readyFor({ type: 'job', spec, thumb })) return;
  app.pending = null;
  try {
    await request('enqueue', { spec, thumb });
    app.tab = 'jobs';
    app.overlay = null;
    renderAll();
  } catch (e) {
    showDialog({ icon: 'error', title: 'Не удалось начать загрузку', text: e.error || String(e) });
  }
}

async function retryJob(id, failedOnly) {
  const job = app.jobs.get(id);
  if (!job) return;
  if (!readyFor({ type: 'retry', id, failedOnly })) return;
  app.pending = null;
  try {
    await request('retry', { id, failedOnly });
    app.tab = 'jobs';
    renderAll();
  } catch (e) {
    showDialog({ icon: 'error', title: 'Не удалось повторить загрузку', text: e.error || String(e) });
  }
}

function onJob(job) {
  const prev = app.jobs.get(job.id);
  app.jobs.set(job.id, job);
  renderJob(job);
  renderJobsMeta();
  const was = prev?.view?.state;
  const now = job.view?.state;
  if (ACTIVE.has(was) && !ACTIVE.has(now) && !app.reported.has(job.id)) {
    app.reported.add(job.id);
    // Диалог показывает только видимая панель: открытых панелей может быть несколько.
    if (document.visibilityState === 'visible') reportFinished(job);
  }
}

/** Диалог об ошибке загрузки, как в приложении. */
function reportFinished(job) {
  const v = job.view;
  if (v.interrupted) return;
  if (job.spec.kind === 'single' && v.state === 'failed') {
    failureDialog('Не удалось скачать видео', v, job.spec.url, { type: 'retry', id: job.id, failedOnly: false });
    return;
  }
  if (job.spec.kind === 'batch' && v.failed?.length && (v.state === 'done' || v.state === 'failed')) {
    const ok = v.total - v.failed.length;
    const details = v.failed.map((f) => `• ${f.title}\n  ${f.error}`).join('\n\n');
    const retry = { type: 'retry', id: job.id, failedOnly: true };
    const buttons = [
      { label: `Повторить неудавшиеся (${v.failed.length})`, primary: true, action: () => runRetry(retry) },
      { label: 'Закрыть' },
    ];
    let text = details;
    if (app.components?.update_available) {
      text += updateHint();
      buttons[0] = { label: 'Обновить yt-dlp и повторить', primary: true, action: () => updateThen(retry) };
    } else if (!app.settings.cookies && v.auth) {
      text += authHint(job.spec.entries[0]?.url || '');
      buttons.splice(1, 0, { label: 'Включить cookies и повторить', action: () => cookiesThen(retry) });
    }
    showDialog({ icon: 'error', title: ok === 0 ? 'Ни одно видео не скачалось' : 'Не все видео скачались', text, buttons });
  }
}

function updateHint() {
  const c = app.components;
  return `\n\nДоступна новая версия yt-dlp — ${c?.latest || '?'} (у вас ${item('yt-dlp')?.installed?.version || '?'}). Устаревший yt-dlp — самая частая причина таких ошибок. После обновления всё повторится автоматически.`;
}

function authHint(url) {
  const site = isTwitterUrl(url) ? 'X' : 'Сайт';
  return `\n\n${site} требует авторизации для этого видео. Включите в настройках cookies «Этот браузер» (если вы в нём залогинены) или нажмите кнопку ниже.`;
}

function updateThen(retry) {
  app.pending = retry;
  updateYtdlp();
}

async function cookiesThen(retry) {
  app.settings = await saveSettings({ cookies: 'self' });
  renderSettings();
  runRetry(retry);
}

/**
 * Диалог ошибки yt-dlp с подсказками: про cookies, если похоже на
 * требование авторизации, и про обновление yt-dlp, если оно доступно.
 * Тогда основная кнопка обновляет yt-dlp и повторяет действие сама.
 */
function failureDialog(title, err, url, retry) {
  let text = err.error || 'Неизвестная ошибка';
  const buttons = [];
  if (!app.settings.cookies && err.auth) {
    text += authHint(url);
    buttons.push({ label: 'Включить cookies и повторить', primary: true, action: () => cookiesThen(retry) });
  }
  if (app.components?.update_available) {
    text += updateHint();
    for (const b of buttons) b.primary = false;
    buttons.unshift({ label: 'Обновить yt-dlp и повторить', primary: true, action: () => updateThen(retry) });
  }
  buttons.push(buttons.length ? { label: 'Закрыть' } : { label: 'OK', primary: true });
  showDialog({ icon: 'error', title, text, buttons });
}

// ─── Диалоги ────────────────────────────────────────────────────────────────

function showDialog(d) {
  if (app.dialog) app.dialogs.push(d);
  else presentDialog(d);
}

function presentDialog(d) {
  app.dialog = d;
  const box = $('#dialog');
  box.dataset.tone = d.icon === 'error' ? 'danger' : 'info';
  $('#dialog .dialog-icon use').setAttribute('href', d.icon === 'question' ? '#i-folder' : '#i-alert');
  $('#dialog-title').textContent = d.title || '';
  $('#dialog-text').textContent = d.text || '';
  const buttons = d.buttons?.length ? d.buttons : [{ label: 'OK', primary: true }];
  const row = $('#dialog-buttons');
  row.replaceChildren(
    ...buttons.map((b) => {
      const btn = el('button', { type: 'button', class: b.primary ? 'btn primary' : 'btn' }, b.label);
      btn.addEventListener('click', () => closeDialog(b.action));
      return btn;
    }),
  );
  box.hidden = false;
  (row.querySelector('.primary') || row.lastElementChild)?.focus();
}

function closeDialog(action) {
  $('#dialog').hidden = true;
  app.dialog = null;
  if (action) action();
  const next = app.dialogs.shift();
  if (next && !app.dialog) presentDialog(next);
}

// ─── Отрисовка ──────────────────────────────────────────────────────────────

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v !== undefined && v !== null && v !== false) node.setAttribute(k, v === true ? '' : String(v));
  }
  for (const c of children) node.append(c);
  return node;
}

function svg(id, cls = 'icon') {
  const ns = 'http://www.w3.org/2000/svg';
  const s = document.createElementNS(ns, 'svg');
  s.setAttribute('class', cls);
  const use = document.createElementNS(ns, 'use');
  use.setAttribute('href', `#${id}`);
  s.append(use);
  return s;
}

function fillSelect(select, labels, index) {
  select.replaceChildren(...labels.map((l, i) => el('option', { value: i }, l)));
  select.selectedIndex = Math.min(index, labels.length - 1);
}

function setProgress(bar, fraction) {
  const fill = bar.querySelector('.progress-fill');
  const known = fraction !== null && fraction !== undefined && fraction >= 0;
  bar.classList.toggle('indeterminate', !known);
  fill.style.width = known ? `${Math.round(Math.min(1, fraction) * 1000) / 10}%` : '';
}

function currentScreen() {
  const broken = ['missing', 'forbidden', 'error'].includes(app.host.status);
  if (app.overlay === 'settings') return 'settings';
  if (broken) return 'host';
  if (app.overlay === 'components') return 'components';
  return app.tab;
}

function renderAll() {
  const screen = currentScreen();
  for (const s of ['get', 'jobs', 'settings', 'components', 'host']) $(`#screen-${s}`).hidden = s !== screen;
  for (const b of document.querySelectorAll('.tabs button')) {
    b.setAttribute('aria-selected', String(b.dataset.tab === screen));
  }
  const [text, tone] = pill();
  $('#pill-text').textContent = text;
  $('#pill').dataset.tone = tone;
  renderBanner(screen);
  renderGet();
  renderJobs();
  renderComponents();
  renderHost();
  renderSettings();
}

function renderBanner(screen) {
  const b = banner();
  const box = $('#banner');
  const show = b && b.key !== app.dismissedBanner && screen !== 'components' && screen !== 'host';
  box.hidden = !show;
  if (!show) return;
  box.dataset.tone = b.tone;
  box.querySelector('.banner-icon use').setAttribute(
    'href',
    b.tone === 'success' ? '#i-check' : b.tone === 'info' ? '#i-refresh' : '#i-alert',
  );
  box.querySelector('.banner-title').textContent = b.title;
  box.querySelector('.banner-text').textContent = b.text;
  const progress = box.querySelector('.banner-progress');
  progress.hidden = b.progress === undefined;
  if (b.progress !== undefined) setProgress(progress, b.progress);
  const action = box.querySelector('.banner-action');
  action.hidden = !b.button;
  action.textContent = b.button || '';
  action.className = b.key.startsWith('install') ? 'btn banner-action' : 'btn primary banner-action';
  action.disabled = !b.key.startsWith('install') && (busyJobs() || app.fetching);
  action.onclick = b.action;
  const close = box.querySelector('.banner-close');
  close.hidden = !b.dismissable;
  close.onclick = () => {
    app.dismissedBanner = b.key;
    if (b.key.startsWith('done')) app.doneUntil = 0;
    renderAll();
  };
}

function renderGet() {
  const view = app.get.view;
  $('#get-empty').hidden = view !== 'empty';
  $('#get-loading').hidden = view !== 'loading';
  $('#get-video').hidden = view !== 'video';
  $('#get-channel').hidden = view !== 'channel';
  $('#loading-text').textContent = app.get.loadingText;
  const installing = Boolean(app.components?.install);
  $('#fetch').disabled = app.fetching || installing || !hostReady();
  $('#download').disabled = installing;
  $('#download-batch').disabled = installing || selectedCount() === 0;
  if (view === 'channel') renderChannelTools();
}

function renderChannelTools() {
  const ch = app.get.channel;
  if (!ch) return;
  const n = selectedCount();
  $('#selected-count').textContent = `Выбрано ${n} из ${ch.entries.length}`;
  $('#download-batch-text').textContent = `Скачать выбранные (${n})`;
  $('#download-batch').disabled = Boolean(app.components?.install) || n === 0;
  const more = $('#load-more');
  more.hidden = !ch.more;
  more.disabled = ch.loadingMore;
  more.textContent = ch.loadingMore ? 'Загружаем…' : 'Загрузить ещё';
}

function sortedJobs() {
  const all = [...app.jobs.values()];
  const active = all.filter((j) => ACTIVE.has(j.view?.state));
  // Сначала идущая загрузка, затем очередь по порядку, затем завершённые (свежие выше).
  active.sort((a, b) => (a.view.state === 'queued') - (b.view.state === 'queued') || a.created - b.created);
  const done = all.filter((j) => !ACTIVE.has(j.view?.state)).sort((a, b) => b.created - a.created);
  return [...active, ...done];
}

function renderJobs() {
  const list = $('#jobs-list');
  const jobs = sortedJobs();
  const ids = new Set(jobs.map((j) => j.id));
  for (const node of [...list.children]) if (!ids.has(node.dataset.id)) node.remove();
  jobs.forEach((job, i) => {
    let node = list.querySelector(`.job[data-id="${CSS.escape(job.id)}"]`) || jobCard(job);
    if (list.children[i] !== node) list.insertBefore(node, list.children[i] || null);
    updateJobCard(node, job);
  });
  renderJobsMeta();
}

function renderJob(job) {
  const node = $(`#jobs-list .job[data-id="${CSS.escape(job.id)}"]`);
  if (node) {
    const wasActive = node.dataset.state && ACTIVE.has(node.dataset.state);
    updateJobCard(node, job);
    // Загрузка закончилась, её место в списке меняется.
    if (wasActive !== ACTIVE.has(job.view?.state)) renderJobs();
  } else {
    renderJobs();
  }
}

function renderJobsMeta() {
  const all = [...app.jobs.values()];
  const active = all.filter((j) => ACTIVE.has(j.view?.state)).length;
  const count = $('#jobs-count');
  count.hidden = active === 0;
  count.textContent = String(active);
  $('#jobs-empty').hidden = all.length > 0;
  $('#clear-jobs').hidden = all.length === active;
  $('#jobs-summary').textContent = all.length
    ? active
      ? `Активных: ${active}`
      : 'Все загрузки завершены'
    : '';
}

function jobCard(job) {
  const node = el('div', { class: 'job', 'data-id': job.id });
  const thumb = el('div', { class: 'job-thumb' });
  if (job.thumb) thumb.append(el('img', { src: job.thumb, alt: '', loading: 'lazy', referrerpolicy: 'no-referrer' }));
  node.append(
    thumb,
    el('div', { class: 'job-heading' }, svg('i-clock'), el('span', { class: 'job-heading-text' })),
    el('div', { class: 'job-subtitle' }),
    el(
      'div',
      { class: 'job-body' },
      el('div', { class: 'progress' }, el('div', { class: 'progress-fill' })),
      el('div', { class: 'job-line' }, el('span', { class: 'job-percent' }), el('span', { class: 'job-status' })),
      el('div', { class: 'job-stats' }),
      el('div', { class: 'job-actions' }),
      el('div', { class: 'job-details', hidden: true }),
    ),
  );
  if (!job.thumb) thumb.remove();
  if (!job.thumb) node.style.gridTemplateColumns = '1fr';
  return node;
}

function updateJobCard(node, job) {
  const v = job.view || {};
  const state = v.state || 'queued';
  const changed = node.dataset.state !== state;
  node.dataset.state = state;
  const icon = { queued: 'i-clock', running: 'i-download', cancelling: 'i-download', done: 'i-check', failed: 'i-alert', cancelled: 'i-close' }[state];
  node.querySelector('.job-heading use').setAttribute('href', `#${icon}`);
  node.querySelector('.job-heading-text').textContent = v.heading || '';
  node.querySelector('.job-subtitle').textContent = v.subtitle || job.spec.title || '';
  const bar = node.querySelector('.progress');
  bar.hidden = state === 'cancelled' || (state === 'failed' && !v.progress);
  setProgress(bar, v.known === false ? -1 : v.progress || 0);
  node.querySelector('.job-percent').textContent = v.percent || '';
  node.querySelector('.job-status').textContent = state === 'done' && job.spec.kind === 'single' ? '' : v.status || '';
  const stats = [v.size, v.speed, v.eta && `осталось ${v.eta}`, v.connections > 1 && `${v.connections} потоков`];
  node.querySelector('.job-stats').textContent = ACTIVE.has(state) ? stats.filter(Boolean).join(' · ') : '';
  if (changed || node.dataset.actions !== actionsKey(job)) {
    node.dataset.actions = actionsKey(job);
    renderJobActions(node, job);
  }
}

/** Набор кнопок зависит от состояния; перерисовываем их только при его смене. */
function actionsKey(job) {
  const v = job.view || {};
  return [v.state, v.can_open, v.failed?.length, v.auth, v.outdated, app.components?.update_available, app.settings.cookies].join('|');
}

function renderJobActions(node, job) {
  const v = job.view || {};
  const actions = node.querySelector('.job-actions');
  const details = node.querySelector('.job-details');
  const buttons = [];
  const button = (label, onClick, cls = 'btn small') => {
    const b = el('button', { type: 'button', class: cls }, label);
    b.addEventListener('click', onClick);
    buttons.push(b);
    return b;
  };
  const retry = (failedOnly) => ({ type: 'retry', id: job.id, failedOnly });
  switch (v.state) {
    case 'queued':
      button('Убрать из очереди', () => cancelJob(job.id));
      break;
    case 'running':
      button('Отмена', () => cancelJob(job.id));
      break;
    case 'cancelling':
      button('Отмена…', () => {}).disabled = true;
      break;
    default: {
      if (v.can_open) button('Открыть папку', () => openFolder(v), 'btn small primary');
      if (v.state === 'failed' || v.state === 'cancelled' || v.failed?.length) {
        const failedOnly = job.spec.kind === 'batch' && v.failed?.length > 0 && v.failed.length < v.total && v.state !== 'cancelled';
        if (v.state === 'failed' && app.components?.update_available && (v.outdated || !v.interrupted)) {
          button('Обновить yt-dlp и повторить', () => updateThen(retry(failedOnly)));
        }
        if (v.auth && !app.settings.cookies) button('Включить cookies и повторить', () => cookiesThen(retry(failedOnly)));
        button(failedOnly ? `Повторить неудавшиеся (${v.failed.length})` : 'Повторить', () => runRetry(retry(failedOnly)));
      }
      const text = jobDetails(v);
      if (text) {
        const b = button('Подробнее', () => {
          details.hidden = !details.hidden;
          b.textContent = details.hidden ? 'Подробнее' : 'Скрыть';
        });
        details.textContent = text;
      }
      button('Убрать', () => request('remove', { id: job.id }));
    }
  }
  if (ACTIVE.has(v.state)) details.hidden = true;
  actions.replaceChildren(...buttons);
}

function jobDetails(v) {
  if (v.failed?.length) return v.failed.map((f) => `• ${f.title}\n  ${f.error}`).join('\n\n');
  if (v.error) return v.error;
  if (v.file) return v.file;
  return '';
}

async function cancelJob(id) {
  try {
    await request('cancel', { id });
  } catch (e) {
    showDialog({ icon: 'error', title: 'Не удалось отменить', text: e.error || String(e) });
  }
}

function openFolder(v) {
  hostCall('open_folder', { dir: v.dir, file: v.file }).catch(() => {});
}

function renderComponents() {
  const c = app.components;
  const screen = $('#screen-components');
  if (screen.hidden) return;
  const rows = $('#component-rows');
  if (!c) {
    rows.replaceChildren(el('div', { class: 'muted' }, 'Подключение к помощнику…'));
    return;
  }
  const setup = c.detected && c.missing.length > 0;
  $('#components-title').textContent = setup ? 'Подготовка к работе' : 'Компоненты';
  $('#components-intro').textContent = setup
    ? 'Для скачивания нужны три программы. Помощник скачает их с официальных страниц проектов, сверит контрольные суммы и подключит сам. В системе ничего не меняется.'
    : 'Программы, которыми помощник скачивает видео. yt-dlp регулярно обновляется: сайты часто меняются, и старые версии перестают работать.';
  const busy = Boolean(c.install) || busyJobs();
  rows.replaceChildren(
    ...c.items.map((it) => {
      const r = componentRow(it);
      const status = el('div', { class: 'component-status' }, r.status);
      if (r.path) status.append('\n', el('span', { class: 'path-line' }, r.path));
      const card = el(
        'div',
        { class: 'component', 'data-tone': r.tone },
        el('div', { class: 'component-head' }, el('span', { class: 'component-title' }, it.title), el('span', { class: 'component-purpose' }, `— ${it.purpose}`)),
        status,
      );
      if (r.busy) {
        const bar = el('div', { class: 'progress' }, el('div', { class: 'progress-fill' }));
        setProgress(bar, r.progress);
        card.append(bar);
      }
      if (r.action) {
        const b = el('button', { type: 'button', class: 'btn small' }, r.action);
        b.disabled = busy;
        b.title = busyJobs() ? 'Недоступно, пока идёт загрузка' : '';
        b.addEventListener('click', () => install(it.key));
        card.append(b);
      }
      return card;
    }),
  );
  const missing = $('#install-missing');
  missing.hidden = !setup || Boolean(c.install);
  missing.textContent = `Установить всё (около ${missingMb(c)} МБ)`;
  missing.disabled = busy;
  $('#cancel-install').hidden = !c.install;
  $('#check-updates').disabled = Boolean(c.install) || c.checking;
  $('#check-updates').textContent = c.checking ? 'Проверяем…' : 'Проверить обновления';
  $('#components-folder').textContent = c.bin_dir ? `Папка: ${c.bin_dir}` : '';
}

function renderHost() {
  const screen = $('#screen-host');
  if (screen.hidden) return;
  const status = app.host.status;
  const id = chrome.runtime.id;
  const command = `lader-host --install --extension-id ${id}`;
  $('#host-command').hidden = status !== 'forbidden';
  $('#host-command').textContent = command;
  $('#host-copy').hidden = status !== 'forbidden';
  $('#host-copy').onclick = () => navigator.clipboard.writeText(command);
  $('#host-details').hidden = !app.host.error;
  $('#host-error').textContent = app.host.error;
  const steps = $('#host-steps');
  if (status === 'forbidden') {
    $('#host-title').textContent = 'Помощник не разрешает это расширение';
    $('#host-text').textContent = `Помощник зарегистрирован для другого расширения. Так бывает, если расширение установлено из магазина или ID у него другой (сейчас ${id}). Разрешите его командой в терминале:`;
    steps.replaceChildren();
  } else {
    $('#host-title').textContent = status === 'missing' ? 'Нужен помощник Lader' : 'Помощник не запустился';
    $('#host-text').textContent =
      status === 'missing'
        ? 'Расширение не может само запускать программы, поэтому видео скачивает небольшой помощник lader-host на том же ядре, что и приложение Lader. yt-dlp, Deno и ffmpeg он поставит и будет обновлять сам.'
        : 'Помощник зарегистрирован, но не отвечает. Скорее всего, его файл переместили или удалили. Запустите lader-host ещё раз, и он заново зарегистрируется.';
    const link = el('a', { href: RELEASES, target: '_blank', rel: 'noreferrer' }, 'релизов Lader');
    steps.replaceChildren(
      el('li', {}, 'Скачайте lader-host.exe со страницы ', link, ' (или соберите: ', el('code', {}, 'cargo build --release --bin lader-host'), ').'),
      el('li', {}, 'Запустите его двойным щелчком. Он скопирует себя в ', el('code', {}, '%LOCALAPPDATA%\\Lader\\host'), ' и зарегистрируется в Edge. Права администратора не нужны.'),
      el('li', {}, 'Нажмите «Проверить снова».'),
    );
  }
}

function renderSettings() {
  const s = app.settings;
  const root = $('#root-dir');
  if (document.activeElement !== root) root.value = s.out_dir;
  const browsers = app.host.info?.cookie_browsers || [
    { label: 'Firefox', value: 'firefox' },
    { label: 'Brave', value: 'brave' },
    { label: 'Opera', value: 'opera' },
    { label: 'Vivaldi', value: 'vivaldi' },
    { label: 'Chromium', value: 'chromium' },
  ];
  const options = [
    { label: 'Без cookies', value: '' },
    { label: isEdge ? 'Этот браузер (Edge)' : 'Этот браузер', value: 'self' },
    ...browsers.map((b) => ({ label: `Из браузера ${b.label}`, value: b.value })),
  ];
  const cookies = $('#cookies');
  if (cookies.options.length !== options.length) {
    cookies.replaceChildren(...options.map((o) => el('option', { value: o.value }, o.label)));
  }
  cookies.value = s.cookies;
  const hints = {
    '': 'Видео, закрытые авторизацией или возрастным ограничением, и приватные твиты не скачаются.',
    self: 'Помощник получит cookies YouTube и X из этого браузера только на время загрузки, во временном файле, который сразу удаляется.',
  };
  $('#cookies-hint').textContent = hints[s.cookies] ?? 'yt-dlp сам прочитает cookies из профиля этого браузера.';
  const conn = $('#connections');
  if (!conn.options.length) fillSelect(conn, CONNECTION_CHOICES.map(String), 0);
  conn.value = String(CONNECTION_CHOICES.indexOf(s.connections) >= 0 ? CONNECTION_CHOICES.indexOf(s.connections) : 3);
  $('#page-buttons').checked = s.page_buttons;
  const manifest = chrome.runtime.getManifest();
  $('#about').replaceChildren(
    el('span', {}, `Расширение ${manifest.version}${app.host.info ? ` · помощник ${app.host.info.version}` : ''}`),
    el('span', {}, `ID расширения: ${chrome.runtime.id}`),
  );
}

// ─── События интерфейса ─────────────────────────────────────────────────────

function wire() {
  $('#source').addEventListener('submit', (e) => {
    e.preventDefault();
    fetchInfo();
  });
  for (const b of document.querySelectorAll('.tabs button')) {
    b.addEventListener('click', () => {
      app.tab = b.dataset.tab;
      app.overlay = null;
      renderAll();
    });
  }
  $('#open-settings').addEventListener('click', () => {
    app.overlay = app.overlay === 'settings' ? null : 'settings';
    renderAll();
  });
  for (const b of document.querySelectorAll('[data-back]')) {
    b.addEventListener('click', () => {
      app.overlay = null;
      app.componentsAutoClose = false;
      renderAll();
    });
  }
  // Повторный щелчок по строке статуса возвращает на прежний экран.
  $('#pill').addEventListener('click', () => {
    if (app.overlay === 'components') {
      app.overlay = null;
    } else if (hostReady()) {
      openComponents(false);
      return;
    }
    renderAll();
  });
  $('#settings-components').addEventListener('click', () => openComponents(false));

  $('#quality').addEventListener('change', () => updateSingleFormats(false));
  $('#format').addEventListener('change', () => updateSingleFormats(false));
  $('#batch-quality').addEventListener('change', () => updateBatchFormats(false));
  $('#batch-format').addEventListener('change', () => updateBatchFormats(false));
  $('#download').addEventListener('click', downloadSingle);
  $('#download-batch').addEventListener('click', downloadBatch);
  $('#video-playlist').addEventListener('click', () => {
    const list = playlistOf(app.get.video?.url || '');
    if (list) loadUrl(list);
  });

  const rows = $('#channel-rows');
  rows.addEventListener('click', (e) => {
    const row = e.target.closest('.row');
    if (!row) return;
    toggleRow(Number(row.dataset.index));
    renderChannelTools();
  });
  rows.addEventListener('keydown', (e) => {
    const row = e.target.closest('.row');
    if (!row || (e.key !== ' ' && e.key !== 'Enter')) return;
    e.preventDefault();
    toggleRow(Number(row.dataset.index));
    renderChannelTools();
  });
  $('#select-all').addEventListener('click', () => selectAll(true));
  $('#select-none').addEventListener('click', () => selectAll(false));
  $('#load-more').addEventListener('click', loadMore);

  $('#clear-jobs').addEventListener('click', () => request('clear'));

  for (const b of document.querySelectorAll('[data-browse]')) {
    b.addEventListener('click', () => browse(b.dataset.browse));
  }

  $('#install-missing').addEventListener('click', installMissing);
  $('#cancel-install').addEventListener('click', cancelInstall);
  $('#check-updates').addEventListener('click', () => {
    app.dismissedBanner = null;
    hostCall('check', { user: true }).catch(() => {});
  });
  $('#open-components-folder').addEventListener('click', () => hostCall('open_components_folder').catch(() => {}));
  $('#host-retry').addEventListener('click', () => bg.postMessage({ type: 'reconnect' }));

  // Настройки сохраняются сразу, корневая папка после паузы в наборе.
  let rootTimer = 0;
  $('#root-dir').addEventListener('input', (e) => {
    const value = e.target.value;
    $('#single-dir').value = value;
    $('#batch-dir').value = value;
    clearTimeout(rootTimer);
    if (value.trim()) rootTimer = setTimeout(() => setSetting('out_dir', value.trim()), 700);
  });
  $('#cookies').addEventListener('change', (e) => setSetting('cookies', e.target.value));
  $('#connections').addEventListener('change', (e) => setSetting('connections', CONNECTION_CHOICES[e.target.selectedIndex]));
  $('#page-buttons').addEventListener('change', (e) => setSetting('page_buttons', e.target.checked));

  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && app.dialog) {
      const cancel = app.dialog.buttons?.find((b) => !b.primary);
      closeDialog(cancel?.action);
    }
  });

  fillSelect($('#batch-quality'), BATCH_QUALITIES, 0);
  updateBatchFormats(true);
}

async function setSetting(key, value) {
  app.settings = await saveSettings({ [key]: value });
  renderSettings();
  renderJobs();
}

/** Выбор папки через нативный диалог помощника. */
async function browse(fieldId) {
  const input = $(`#${fieldId}`);
  try {
    const res = await hostCall('browse', { start: input.value.trim() || app.settings.out_dir });
    if (!res.path) return;
    input.value = res.path;
    if (fieldId === 'root-dir') {
      $('#single-dir').value = res.path;
      $('#batch-dir').value = res.path;
      setSetting('out_dir', res.path);
    }
  } catch (e) {
    showDialog({ icon: 'error', title: 'Не удалось открыть выбор папки', text: e.error || String(e) });
  }
}

chrome.storage.onChanged.addListener((changes, area) => {
  if (area === 'local' && changes.settings) {
    app.settings = { ...DEFAULT_SETTINGS, ...changes.settings.newValue };
    renderSettings();
  }
});

// ─── Запуск ─────────────────────────────────────────────────────────────────

async function start() {
  app.settings = await loadSettings();
  wire();
  connectBackground();
  renderAll();
  // Ссылка для панели: из адреса окна, от кнопки на странице (придёт
  // сообщением 'load') или из открытой вкладки.
  const url = params.get('url');
  if (url) {
    loadUrl(url);
    return;
  }
  try {
    const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
    if (tab?.url && isDownloadable(tab.url) && app.get.view === 'empty' && !app.fetching && !$('#url').value) {
      loadUrl(tab.url);
    }
  } catch {
    // нет доступа к вкладке, оставляем поле пустым
  }
}

start();
