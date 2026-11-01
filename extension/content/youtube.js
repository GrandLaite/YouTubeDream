// Кнопка «Скачать» на YouTube: на странице видео, Shorts, плейлиста и
// канала. YouTube это одностраничное приложение, оно перерисовывает шапки
// при переходах, поэтому кнопка проверяется раз в секунду и после каждой
// навигации. Если привычного места для неё нет (YouTube поменял вёрстку,
// мобильная версия, Shorts), она плавает в углу страницы.

(() => {
  const P = globalThis.LaderPage;
  const CHANNEL = /^\/(@[^/]+|channel\/[^/]+|c\/[^/]+|user\/[^/]+)(?:\/([^/]+))?\/?$/;
  /** Вкладки канала, которые yt-dlp отдаёт списком видео. */
  const VIDEO_TABS = new Set(['videos', 'streams', 'shorts']);
  /** Сколько ждать родное место для кнопки после перехода, прежде чем показать плавающую. */
  const GRACE_MS = 2500;

  /** Где ставить кнопку: селектор и как вставить. Первый видимый выигрывает. */
  const ANCHORS = {
    watch: [
      ['ytd-watch-metadata #owner', 'append'],
      ['#above-the-fold #owner', 'append'],
      ['ytd-watch-metadata #top-level-buttons-computed', 'prepend'],
    ],
    playlist: [
      ['yt-page-header-view-model yt-flexible-actions-view-model', 'append'],
      ['ytd-playlist-header-renderer yt-flexible-actions-view-model', 'append'],
      ['ytd-playlist-header-renderer .metadata-buttons-wrapper', 'append'],
    ],
    channel: [
      ['#page-header yt-flexible-actions-view-model', 'append'],
      ['ytd-c4-tabbed-header-renderer #buttons', 'append'],
      ['#channel-header #buttons', 'append'],
    ],
    shorts: [],
  };

  let navigatedAt = Date.now();

  /** Что скачивать с этой страницы: [вид страницы, ссылка] или null. */
  function target() {
    const u = new URL(location.href);
    const path = u.pathname;
    if (path === '/watch' && u.searchParams.get('v')) return ['watch', u.href];
    if (path.startsWith('/shorts/') || path.startsWith('/live/')) return ['shorts', u.origin + path];
    if (path === '/playlist' && u.searchParams.get('list')) {
      return ['playlist', `${u.origin}/playlist?list=${encodeURIComponent(u.searchParams.get('list'))}`];
    }
    const m = path.match(CHANNEL);
    if (m) {
      // Главная, «Плейлисты», «Сообщество» не являются списком видео, берём корень
      // канала, его помощник сам направит во вкладку «Видео».
      const tab = m[2] && VIDEO_TABS.has(m[2]) ? `/${m[2]}` : '';
      return ['channel', `${u.origin}/${m[1]}${tab}`];
    }
    return null;
  }

  const visible = (node) => node.isConnected && node.getClientRects().length > 0;

  function remove(kind) {
    for (const n of document.querySelectorAll(kind ? `[data-lader="${kind}"]` : '[data-lader]')) n.remove();
  }

  function make(kind, className) {
    const b = P.button({ label: 'Скачать', className, url: () => target()?.[1] });
    b.dataset.lader = kind;
    return b;
  }

  function update() {
    const t = P.isEnabled() ? target() : null;
    if (!t) {
      remove();
      return;
    }
    const inline = document.querySelector('[data-lader="inline"]');
    if (inline && visible(inline)) {
      remove('float');
      return;
    }
    inline?.remove();
    for (const [selector, how] of ANCHORS[t[0]]) {
      const anchor = [...document.querySelectorAll(selector)].find(visible);
      if (anchor) {
        anchor[how](make('inline', 'lader-yt'));
        remove('float');
        return;
      }
    }
    if (!document.querySelector('[data-lader="float"]') && Date.now() - navigatedAt > GRACE_MS) {
      document.body.append(make('float', 'lader-float'));
    }
  }

  document.addEventListener('yt-navigate-finish', () => {
    navigatedAt = Date.now();
    update();
  });
  P.onToggle(update);
  setInterval(update, 1000);
  update();
})();
