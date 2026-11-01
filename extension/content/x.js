// Кнопка «Скачать» у твитов с видео в X (Twitter): в строке действий
// рядом с «Ответить», «Репост», «Нравится». Лента подгружается по мере
// прокрутки, поэтому новые твиты обрабатываются по мере появления.

(() => {
  const P = globalThis.LaderPage;
  const STATUS = /^\/([^/]+)\/status\/(\d+)/;
  let timer = 0;

  /** Ссылка на сам твит: из ссылки на время публикации в его шапке. */
  function statusUrl(article) {
    for (const time of article.querySelectorAll('a[href*="/status/"] time')) {
      const m = new URL(time.closest('a').href).pathname.match(STATUS);
      if (m) return `https://x.com/${m[1]}/status/${m[2]}`;
    }
    // Открытый твит: время публикации у него бывает без ссылки.
    const m = location.pathname.match(STATUS);
    return m ? `https://x.com/${m[1]}/status/${m[2]}` : null;
  }

  function hasVideo(article) {
    return article.querySelector('[data-testid="videoPlayer"], [data-testid="videoComponent"], video') !== null;
  }

  function update() {
    timer = 0;
    if (!P.isEnabled()) {
      for (const n of document.querySelectorAll('[data-lader]')) n.remove();
      return;
    }
    for (const article of document.querySelectorAll('article[data-testid="tweet"]')) {
      if (article.querySelector('[data-lader]') || !hasVideo(article)) continue;
      const groups = article.querySelectorAll('[role="group"]');
      const bar = groups[groups.length - 1];
      const url = statusUrl(article);
      if (!bar || !url) continue;
      const b = P.button({ className: 'lader-x', url });
      b.dataset.lader = 'x';
      bar.append(b);
    }
  }

  function schedule() {
    if (!timer) timer = setTimeout(update, 400);
  }

  new MutationObserver(schedule).observe(document.documentElement, { childList: true, subtree: true });
  P.onToggle(update);
  update();
})();
