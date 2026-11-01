// Общее для кнопок «Скачать» на страницах YouTube и X: включены ли они
// в настройках и как открыть панель Lader с нужной ссылкой.

globalThis.LaderPage = (() => {
  let enabled = true;
  const listeners = [];

  const apply = (settings) => {
    const next = settings?.page_buttons !== false;
    if (next === enabled) return;
    enabled = next;
    for (const f of listeners) f(enabled);
  };
  chrome.storage.local.get('settings').then(({ settings }) => apply(settings), () => {});
  chrome.storage.onChanged.addListener((changes, area) => {
    if (area === 'local' && changes.settings) apply(changes.settings.newValue);
  });

  function open(url) {
    if (!url) return;
    try {
      chrome.runtime.sendMessage({ type: 'open', url }).catch(() => {});
    } catch {
      // Расширение обновили, а на странице ещё старый скрипт: нужна перезагрузка.
      alert('Lader обновился. Обновите страницу, чтобы кнопка снова заработала.');
    }
  }

  function icon() {
    const ns = 'http://www.w3.org/2000/svg';
    const svg = document.createElementNS(ns, 'svg');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('aria-hidden', 'true');
    for (const d of ['M12 4v11', 'M7 10.5l5 5 5-5', 'M5 20h14']) {
      const path = document.createElementNS(ns, 'path');
      path.setAttribute('d', d);
      svg.append(path);
    }
    return svg;
  }

  /** Кнопка. `url` это строка или функция (адрес считается в момент щелчка). */
  function button({ label = '', title = 'Скачать через Lader', className, url }) {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = `lader-btn ${className}`;
    b.title = title;
    b.setAttribute('aria-label', title);
    b.append(icon());
    if (label) {
      const span = document.createElement('span');
      span.textContent = label;
      b.append(span);
    }
    b.addEventListener('click', (e) => {
      e.preventDefault();
      e.stopPropagation();
      open(typeof url === 'function' ? url() : url);
    });
    return b;
  }

  return {
    isEnabled: () => enabled,
    onToggle: (f) => listeners.push(f),
    button,
  };
})();
