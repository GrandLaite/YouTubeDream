// Настройки расширения (chrome.storage.local). Их же читают кнопки на
// страницах YouTube и X, поэтому ключ общий (`settings`).

export const DEFAULT_SETTINGS = {
  /** Корневая папка загрузок. Пустая берётся у помощника (папка приложения). */
  out_dir: '',
  /** Во сколько соединений качать файл. */
  connections: 8,
  /** '' без cookies, 'self' cookies этого браузера, иначе имя браузера для yt-dlp. */
  cookies: '',
  /** Кнопка «Скачать» на страницах YouTube и X. */
  page_buttons: true,
};

export async function loadSettings() {
  const { settings } = await chrome.storage.local.get('settings');
  return { ...DEFAULT_SETTINGS, ...settings };
}

/** Меняет часть настроек. Возвращает настройки целиком. */
export async function saveSettings(patch) {
  const { settings } = await chrome.storage.local.get('settings');
  const next = { ...settings, ...patch };
  await chrome.storage.local.set({ settings: next });
  return { ...DEFAULT_SETTINGS, ...next };
}

/** Задана ли настройка явно (а не взята по умолчанию). */
export async function hasSetting(key) {
  const { settings } = await chrome.storage.local.get('settings');
  return settings != null && Object.hasOwn(settings, key) && settings[key] !== '';
}
