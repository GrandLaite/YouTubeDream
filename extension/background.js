// Фоновая часть расширения Lader.
//
// Держит единственное соединение с помощником lader-host (Native
// Messaging). Помощник один на весь браузер, поэтому загрузки идут одной
// очередью, сколько бы панелей ни было открыто. Здесь же список загрузок,
// число активных на значке, контекстное меню и открытие панели по кнопке
// со страницы YouTube или X.

import { cookieDomains, isDownloadable } from './lib/urls.js';
import { hasSetting, loadSettings, saveSettings } from './lib/settings.js';

const HOST = 'io.github.grandlaite.lader';
/** Помощник без дела отключается через столько: процесс не висит зря. */
const IDLE_MS = 3 * 60 * 1000;
/** Сколько завершённых загрузок помнить в списке. */
const HISTORY = 50;
const ACTIVE = new Set(['queued', 'running', 'cancelling']);
const SITES = ['*://*.youtube.com/*', '*://youtu.be/*', '*://*.x.com/*', '*://*.twitter.com/*'];

// ─── Соединение с помощником ────────────────────────────────────────────────

let port = null;
let ready = null;
let nextId = 1;
const waiting = new Map();
/** status: off, connecting, ready, missing, forbidden, error. */
const host = { status: 'off', error: '', info: null };
let components = null;
let idleTimer = 0;

function setHost(status, error = '') {
  host.status = status;
  host.error = error;
  broadcast({ type: 'host', host });
}

/** Почему не удалось подключиться (по тексту ошибки Chromium). */
function classify(message) {
  if (/not found/i.test(message)) return 'missing';
  if (/forbidden/i.test(message)) return 'forbidden';
  return 'error';
}

function connect() {
  if (port) return ready;
  setHost('connecting');
  try {
    port = chrome.runtime.connectNative(HOST);
  } catch (e) {
    port = null;
    setHost(classify(e.message), e.message);
    ready = Promise.reject({ error: e.message, code: 'host' });
    ready.catch(() => {});
    return ready;
  }
  port.onMessage.addListener(onHostMessage);
  port.onDisconnect.addListener(() => onDisconnect(chrome.runtime.lastError?.message || 'Native host has exited.'));
  ready = send('hello').then((info) => {
    host.info = info;
    setHost('ready');
    applyDefaults(info);
    return info;
  });
  ready.catch(() => {});
  return ready;
}

function onDisconnect(message) {
  port = null;
  ready = null;
  components = null;
  for (const w of waiting.values()) w.reject({ error: message, code: 'host' });
  waiting.clear();
  setHost(classify(message), message);
  broadcast({ type: 'components', components });
  // Процесс помощника завершился, вместе с ним прервались и его загрузки.
  let changed = false;
  for (const job of jobs) {
    if (isActive(job)) {
      interrupt(job);
      changed = true;
    }
  }
  if (changed) jobsChanged();
}

function disconnect() {
  if (!port) return;
  port.disconnect();
  port = null;
  ready = null;
  components = null;
  setHost('off');
}

function send(cmd, params = {}) {
  return new Promise((resolve, reject) => {
    if (!port) {
      reject({ error: 'Помощник не подключён', code: 'host' });
      return;
    }
    const id = nextId++;
    waiting.set(id, { resolve, reject });
    port.postMessage({ id, cmd, ...params });
  });
}

async function call(cmd, params) {
  await connect();
  return send(cmd, params);
}

function onHostMessage(msg) {
  if (msg.reply !== undefined) {
    const w = waiting.get(msg.reply);
    if (w) {
      waiting.delete(msg.reply);
      if (msg.ok) w.resolve(msg.data);
      else w.reject(msg);
    }
  } else if (msg.event === 'components') {
    components = msg;
    broadcast({ type: 'components', components });
  } else if (msg.event === 'job') {
    updateJob(msg);
  }
  scheduleIdle();
}

/** Папка и число потоков по умолчанию берутся у приложения на этом компьютере. */
async function applyDefaults(info) {
  const patch = {};
  if (!(await hasSetting('out_dir')) && info.defaults?.out_dir) patch.out_dir = info.defaults.out_dir;
  if (!(await hasSetting('connections')) && info.defaults?.connections) patch.connections = info.defaults.connections;
  if (Object.keys(patch).length) await saveSettings(patch);
}

function scheduleIdle() {
  clearTimeout(idleTimer);
  if (!port || busy()) return;
  idleTimer = setTimeout(() => {
    if (!busy()) disconnect();
  }, IDLE_MS);
}

function busy() {
  return panels.size > 0 || jobs.some(isActive) || Boolean(components?.install);
}

// ─── Cookies ────────────────────────────────────────────────────────────────

/**
 * Cookies для yt-dlp по настройке. В режиме «Этот браузер» cookies отдаёт сам
 * браузер: yt-dlp не может прочитать базу Edge и Chrome (App-Bound
 * Encryption), а расширению они доступны штатно.
 */
async function cookieSpec(url) {
  const { cookies: mode } = await loadSettings();
  if (!mode) return { mode: 'none' };
  if (mode !== 'self') return { mode: 'browser', browser: mode };
  const seen = new Set();
  const list = [];
  for (const domain of cookieDomains(url)) {
    for (const c of await chrome.cookies.getAll({ domain })) {
      const key = `${c.domain}|${c.path}|${c.name}`;
      if (seen.has(key)) continue;
      seen.add(key);
      list.push({
        name: c.name,
        value: c.value,
        domain: c.domain,
        path: c.path,
        secure: c.secure,
        httpOnly: c.httpOnly,
        hostOnly: c.hostOnly,
        expirationDate: c.expirationDate,
      });
    }
  }
  return { mode: 'list', list };
}

function specUrl(spec) {
  return spec.kind === 'batch' ? spec.entries[0]?.url || '' : spec.url;
}

// ─── Загрузки ───────────────────────────────────────────────────────────────

/** [{ id, spec, view, created, thumb }] в порядке добавления. */
let jobs = [];
let saveTimer = 0;
const restored = restoreJobs();

function isActive(job) {
  return ACTIVE.has(job.view?.state);
}

/** Загрузка, которую помощник уже не продолжит. */
function interrupt(job) {
  job.view = {
    ...job.view,
    state: 'failed',
    heading: 'Прервано',
    status: 'Загрузка прервалась: браузер или помощник закрылся. Недокачанное удалено.',
    speed: '',
    eta: '',
    interrupted: true,
  };
}

async function restoreJobs() {
  const { jobs: saved } = await chrome.storage.local.get('jobs');
  jobs = Array.isArray(saved) ? saved : [];
  // Service worker перезапустился, соединение с помощником было закрыто,
  // и всё, что шло, прервано.
  for (const job of jobs) if (isActive(job)) interrupt(job);
  updateBadge();
}

function jobsChanged() {
  updateBadge();
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    const finished = jobs.filter((j) => !isActive(j));
    const extra = finished.length - HISTORY;
    if (extra > 0) {
      const drop = new Set(finished.slice(0, extra).map((j) => j.id));
      jobs = jobs.filter((j) => !drop.has(j.id));
    }
    chrome.storage.local.set({ jobs });
  }, 500);
}

function updateBadge() {
  const n = jobs.filter(isActive).length;
  chrome.action.setBadgeText({ text: n ? String(n) : '' });
  chrome.action.setBadgeBackgroundColor({ color: '#0f6cbd' });
}

function placeholderView(job, state, heading, status) {
  const title = job.spec.kind === 'batch' ? job.spec.entries[0]?.title || '' : job.spec.title || job.spec.url;
  return {
    id: job.id,
    kind: job.spec.kind,
    state,
    heading,
    subtitle: title,
    status,
    progress: 0,
    known: true,
    percent: '',
    size: '',
    speed: '',
    eta: '',
    connections: 0,
    total: job.spec.kind === 'batch' ? job.spec.entries.length : 1,
    index: 0,
    finished: 0,
    failed: [],
    dir: job.spec.out_dir,
    file: null,
    can_open: false,
    error: null,
    auth: false,
    outdated: false,
  };
}

async function enqueue(spec, thumb) {
  const job = { id: crypto.randomUUID(), spec, view: null, created: Date.now(), thumb: thumb || '' };
  job.view = placeholderView(job, 'queued', 'В очереди', 'Передаём помощнику…');
  jobs.push(job);
  broadcast({ type: 'job', job });
  jobsChanged();
  try {
    const cookies = await cookieSpec(specUrl(spec));
    await call('enqueue', { job: { ...spec, id: job.id, cookies } });
  } catch (e) {
    job.view = { ...placeholderView(job, 'failed', 'Ошибка', e.error || String(e)), error: e.error || String(e) };
    broadcast({ type: 'job', job });
    jobsChanged();
    throw e;
  }
  return job.id;
}

function updateJob(view) {
  const job = jobs.find((j) => j.id === view.id);
  if (!job) return;
  job.view = view;
  broadcast({ type: 'job', job });
  jobsChanged();
}

/** Повтор той же загрузки. У пакета можно повторить только неудавшиеся. */
async function retry(id, failedOnly) {
  const job = jobs.find((j) => j.id === id);
  if (!job || isActive(job)) return null;
  const spec = structuredClone(job.spec);
  if (failedOnly && spec.kind === 'batch' && job.view?.failed?.length) {
    spec.entries = job.view.failed.map((f) => job.spec.entries[f.index]).filter(Boolean);
  }
  remove(id);
  return enqueue(spec, job.thumb);
}

function remove(id) {
  const before = jobs.length;
  jobs = jobs.filter((j) => j.id !== id || isActive(j));
  if (jobs.length !== before) {
    broadcast({ type: 'removed', ids: [id] });
    jobsChanged();
  }
}

function clearFinished() {
  const ids = jobs.filter((j) => !isActive(j)).map((j) => j.id);
  jobs = jobs.filter(isActive);
  broadcast({ type: 'removed', ids });
  jobsChanged();
}

async function cancel(id) {
  const job = jobs.find((j) => j.id === id);
  const result = await call('cancel', { job_id: id });
  if (job && result.state === 'cancelling' && job.view.state === 'running') {
    job.view = { ...job.view, state: 'cancelling', heading: 'Отмена…' };
    broadcast({ type: 'job', job });
  }
  return result;
}

// ─── Панели ─────────────────────────────────────────────────────────────────

/** Открытые панели: порт → { windowId }. */
const panels = new Map();
/** Ссылки, которые ждут открытия панели в окне: windowId → url. */
const pendingOpen = new Map();

function broadcast(msg) {
  for (const p of panels.keys()) {
    try {
      p.postMessage(msg);
    } catch {
      panels.delete(p);
    }
  }
}

chrome.runtime.onConnect.addListener((p) => {
  if (p.name !== 'panel') return;
  panels.set(p, { windowId: null, mode: '' });
  clearTimeout(idleTimer);
  p.onMessage.addListener((msg) => onPanelMessage(p, msg));
  p.onDisconnect.addListener(() => {
    panels.delete(p);
    scheduleIdle();
  });
  restored.then(() => p.postMessage({ type: 'state', host, components, jobs }));
  connect().catch(() => {});
});

async function onPanelMessage(p, msg) {
  if (msg.type === 'hello') {
    panels.set(p, { windowId: msg.windowId, mode: msg.mode });
    const url = msg.mode === 'side' ? pendingOpen.get(msg.windowId) : null;
    if (url) {
      pendingOpen.delete(msg.windowId);
      p.postMessage({ type: 'load', url });
    }
    return;
  }
  if (msg.type === 'reconnect') {
    disconnect();
    connect().catch(() => {});
    return;
  }
  const respond = (ok, payload) => {
    try {
      p.postMessage({ type: 'reply', rid: msg.rid, ok, ...(ok ? { data: payload } : { error: payload }) });
    } catch {
      // панель закрылась, пока шёл запрос
    }
  };
  try {
    let data = null;
    switch (msg.type) {
      case 'call': {
        const params = { ...msg.params };
        if (msg.cmd === 'info' || msg.cmd === 'more') {
          params.cookies = await cookieSpec(params.url || params.list_url || '');
        }
        data = await call(msg.cmd, params);
        break;
      }
      case 'enqueue':
        data = await enqueue(msg.spec, msg.thumb);
        break;
      case 'retry':
        data = await retry(msg.id, msg.failedOnly);
        break;
      case 'cancel':
        data = await cancel(msg.id);
        break;
      case 'remove':
        remove(msg.id);
        break;
      case 'clear':
        clearFinished();
        break;
      default:
        throw { error: `неизвестный запрос: ${msg.type}` };
    }
    respond(true, data);
  } catch (e) {
    respond(false, e && typeof e === 'object' ? e : { error: String(e) });
  }
}

// ─── Открытие панели ────────────────────────────────────────────────────────

/**
 * Открывает панель в окне вкладки и передаёт ей ссылку. sidePanel.open
 * работает только в ответ на действие пользователя, поэтому вызывается
 * сразу, без await перед ним. Если боковая панель недоступна, Lader
 * открывается отдельным окном.
 */
function openPanel(tab, url) {
  const windowId = tab?.windowId;
  if (chrome.sidePanel?.open && windowId != null) {
    if (url) pendingOpen.set(windowId, url);
    chrome.sidePanel.open({ windowId }).then(
      () => deliverOpen(windowId),
      () => {
        pendingOpen.delete(windowId);
        openWindow(url);
      },
    );
  } else {
    openWindow(url);
  }
}

/**
 * Боковая панель в этом окне уже была открыта и не переподключится,
 * передаём ссылку ей. Только что открытая заберёт ссылку сама при
 * подключении ('hello'). Панели во вкладках и отдельных окнах ссылку
 * от кнопки не получают.
 */
function deliverOpen(windowId) {
  const url = pendingOpen.get(windowId);
  if (!url) return;
  for (const [p, meta] of panels) {
    if (meta.windowId === windowId && meta.mode === 'side') {
      pendingOpen.delete(windowId);
      p.postMessage({ type: 'load', url });
      return;
    }
  }
}

function openWindow(url) {
  const query = url ? `&url=${encodeURIComponent(url)}` : '';
  chrome.windows.create({ url: `panel.html?mode=window${query}`, type: 'popup', width: 460, height: 780 });
}

chrome.runtime.onMessage.addListener((msg, sender) => {
  if (msg?.type === 'open' && sender.tab && isDownloadable(msg.url || '')) openPanel(sender.tab, msg.url);
});

// ─── Значок и контекстное меню ──────────────────────────────────────────────

function setupAction() {
  const popup = () => chrome.action.setPopup({ popup: 'panel.html?mode=popup' });
  if (chrome.sidePanel?.setPanelBehavior) {
    chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: true }).catch(popup);
  } else {
    popup();
  }
}
setupAction();

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: 'lader-link',
      title: 'Скачать через Lader',
      contexts: ['link'],
      targetUrlPatterns: SITES,
    });
    chrome.contextMenus.create({
      id: 'lader-page',
      title: 'Скачать через Lader',
      contexts: ['page', 'video', 'frame'],
      documentUrlPatterns: SITES,
    });
  });
});

chrome.contextMenus.onClicked.addListener((info, tab) => {
  const url = info.menuItemId === 'lader-link' ? info.linkUrl : info.frameUrl || info.pageUrl;
  openPanel(tab, isDownloadable(url || '') ? url : '');
});
