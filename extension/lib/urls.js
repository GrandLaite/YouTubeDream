// Классификация ссылок по тем же правилам, что в src/urls.rs. Здесь они
// нужны до обращения к помощнику: подписи «Загружаем список видео…»,
// подстановка ссылки из открытой вкладки и выбор домена для cookies.

const SINGLE_VIDEO = /(watch\?v=|youtu\.be\/|\/shorts\/|\/live\/|\/status(?:es)?\/\d+)/i;
const CHANNEL = /(list=|\/playlist|\/channel\/|\/c\/|\/user\/|\/@[\w.-]+|\/videos|\/streams)/i;
const YOUTUBE = /^https?:\/\/(www\.|m\.|music\.)?(youtube\.com|youtu\.be)\//i;
const TWITTER = /^https?:\/\/(www\.|mobile\.)?(twitter|x)\.com\//i;

export function isYoutubeUrl(url) {
  return YOUTUBE.test(url.trim());
}

export function isTwitterUrl(url) {
  return TWITTER.test(url.trim());
}

/** Список видео (канал или плейлист YouTube), а не одно видео. */
export function isChannelUrl(url) {
  url = url.trim();
  if (SINGLE_VIDEO.test(url) || isTwitterUrl(url)) return false;
  return CHANNEL.test(url);
}

/** Ссылка, по которой есть что скачать: видео, Shorts, плейлист, канал, твит. */
export function isDownloadable(url) {
  url = url.trim();
  if (isYoutubeUrl(url)) return SINGLE_VIDEO.test(url) || CHANNEL.test(url);
  if (isTwitterUrl(url)) return /\/status(?:es)?\/\d+/i.test(url);
  return false;
}

/**
 * Видео открыто из плейлиста (watch?v=…&list=…), возвращает ссылку на весь плейлист.
 * Само видео yt-dlp качает без плейлиста (--no-playlist), а здесь даём
 * возможность одним щелчком открыть список целиком.
 */
export function playlistOf(url) {
  try {
    const u = new URL(url.trim());
    if (!isYoutubeUrl(u.href) || !u.searchParams.get('v')) return null;
    const list = u.searchParams.get('list');
    return list ? `https://www.youtube.com/playlist?list=${encodeURIComponent(list)}` : null;
  } catch {
    return null;
  }
}

/** Домены, cookies которых нужны yt-dlp для этой ссылки. */
export function cookieDomains(url) {
  if (isYoutubeUrl(url)) return ['youtube.com'];
  if (isTwitterUrl(url)) return ['x.com', 'twitter.com'];
  return [];
}
