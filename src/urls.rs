//! Классификация ссылок и сообщений об ошибках yt-dlp.

use std::sync::LazyLock;

use regex::Regex;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

/// Одиночные видео: YouTube (watch/youtu.be/shorts/live) и твит X с видео.
static SINGLE_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)(watch\?v=|youtu\.be/|/shorts/|/live/|/status(?:es)?/\d+)"));

/// Списки видео: плейлисты, каналы и вкладки YouTube.
static CHANNEL: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)(list=|/playlist|/channel/|/c/|/user/|/@[\w.-]+|/videos|/streams)"));

static YOUTUBE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)^https?://(www\.|m\.|music\.)?(youtube\.com|youtu\.be)/"));

static TWITTER: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^https?://(www\.|mobile\.)?(twitter|x)\.com/"));

/// Профиль X: x.com/username[/media|/with_replies|/likes]. Такие ссылки
/// yt-dlp не поддерживает (экстрактора ленты нет — только отдельные твиты),
/// поэтому мы их распознаём, чтобы дать внятную ошибку.
static TWITTER_PROFILE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)^https?://(www\.|mobile\.)?(twitter|x)\.com/[A-Za-z0-9_]{1,15}(/(media|with_replies|likes))?/?(\?.*)?$")
});

/// Корень канала YouTube без вкладки: youtube.com/@name, /channel/ID, /c/name, /user/name.
static BARE_CHANNEL: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)^(https?://(?:www\.|m\.)?youtube\.com/(?:@[^/?#]+|channel/[^/?#]+|c/[^/?#]+|user/[^/?#]+))/?(\?[^#]*)?$")
});

/// Ссылка ведёт на YouTube.
pub fn is_youtube_url(url: &str) -> bool {
    YOUTUBE.is_match(url.trim())
}

/// Ссылка ведёт на X (бывший Twitter).
pub fn is_twitter_url(url: &str) -> bool {
    TWITTER.is_match(url.trim())
}

/// Ссылка ведёт на профиль X, а не на твит. Среди twitter-экстракторов
/// yt-dlp нет ленты пользователя — только твит, amplify, broadcast, card и spaces.
pub fn is_twitter_profile_url(url: &str) -> bool {
    let url = url.trim();
    is_twitter_url(url) && !SINGLE_VIDEO.is_match(url) && TWITTER_PROFILE.is_match(url)
}

/// Ссылка ведёт на список видео — канал или плейлист YouTube.
///
/// Порядок проверок от частного к общему: сначала явные одиночные видео
/// (watch?v=, youtu.be, /shorts/, твит /status/), затем явные списки.
/// Неизвестная ссылка считается одиночным видео.
pub fn is_channel_url(url: &str) -> bool {
    let url = url.trim();
    if SINGLE_VIDEO.is_match(url) {
        return false;
    }
    // У X списков нет: лента профиля не поддерживается yt-dlp, а твит —
    // это всегда одиночное видео.
    if is_twitter_url(url) {
        return false;
    }
    CHANNEL.is_match(url)
}

/// Для корня канала YouTube yt-dlp возвращает не видео, а вкладки
/// («Канал - Videos», «Канал - Shorts») — список из двух «плейлистов».
/// Пользователь, вставивший ссылку на канал, ждёт его видео, поэтому
/// направляем такие ссылки сразу во вкладку /videos.
pub fn normalize_channel_url(url: &str) -> String {
    let url = url.trim();
    match BARE_CHANNEL.captures(url) {
        Some(c) => format!("{}/videos{}", &c[1], c.get(2).map_or("", |m| m.as_str())),
        None => url.to_string(),
    }
}

/// Ошибка, которая лечится подстановкой cookies из браузера: приватный или
/// NSFW твит, требование логина, возрастное ограничение, rate limit.
pub fn needs_auth_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    [
        "nsfw",
        "log in",
        "login",
        "sign in",
        "authenticat",
        "private",
        "not authorized",
        "requires cookies",
        "rate limit",
        "no video could be found",
        "age-restricted",
        "confirm your age",
    ]
    .iter()
    .any(|s| m.contains(s))
}

/// Ошибка, характерная для устаревшего yt-dlp: сайты регулярно меняют
/// внутренности, и старые экстракторы начинают падать. yt-dlp в таких
/// случаях сам дописывает «Confirm you are on the latest version using yt-dlp -U».
pub fn looks_outdated(msg: &str) -> bool {
    let m = msg.to_lowercase();
    [
        "yt-dlp -u",
        "latest version",
        "please update",
        "outdated",
        "older than",
        "nsig extraction failed",
        "signature extraction failed",
        "unable to extract",
    ]
    .iter()
    .any(|s| m.contains(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_urls() {
        let cases = [
            // YouTube — одиночные видео
            ("https://www.youtube.com/watch?v=dQw4w9WgXcQ", false),
            ("https://youtu.be/dQw4w9WgXcQ", false),
            ("https://www.youtube.com/shorts/abc123", false),
            ("https://www.youtube.com/live/abc123", false),
            // видео, открытое из плейлиста, — всё ещё одно видео
            ("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL123", false),
            // YouTube — списки
            ("https://www.youtube.com/playlist?list=PL123", true),
            ("https://www.youtube.com/@SomeChannel", true),
            ("https://www.youtube.com/@SomeChannel/videos", true),
            ("https://www.youtube.com/channel/UC123456", true),
            ("https://www.youtube.com/c/SomeChannel", true),
            ("https://www.youtube.com/user/SomeChannel", true),
            // X — одиночные твиты
            ("https://x.com/someuser/status/1234567890", false),
            ("https://twitter.com/someuser/status/1234567890", false),
            ("https://mobile.twitter.com/someuser/status/1234567890", false),
            ("https://x.com/someuser/status/1234567890?s=20&t=abc", false),
            ("https://x.com/i/status/1234567890", false),
            // X — профили: режим канала для x.com не включается никогда
            ("https://x.com/someuser", false),
            ("https://x.com/someuser/media", false),
            ("https://twitter.com/someuser/with_replies", false),
        ];
        for (url, want) in cases {
            assert_eq!(is_channel_url(url), want, "is_channel_url({url})");
        }
    }

    #[test]
    fn twitter_profile_urls() {
        for url in [
            "https://x.com/someuser",
            "https://x.com/someuser/",
            "https://x.com/someuser/media",
            "https://twitter.com/someuser/with_replies",
            "https://x.com/someuser?lang=en",
        ] {
            assert!(is_twitter_profile_url(url), "{url}");
        }
        for url in [
            // конкретные твиты — это видео, а не профиль
            "https://x.com/someuser/status/1234567890",
            "https://twitter.com/someuser/status/1234567890?s=20",
            // не X вообще
            "https://www.youtube.com/@SomeChannel",
            "https://www.youtube.com/watch?v=abc",
        ] {
            assert!(!is_twitter_profile_url(url), "{url}");
        }
    }

    #[test]
    fn twitter_urls() {
        for url in ["https://x.com/u/status/1", "https://twitter.com/u/status/1", "https://www.x.com/u", "https://mobile.twitter.com/u"] {
            assert!(is_twitter_url(url), "{url}");
        }
        for url in ["https://www.youtube.com/watch?v=abc", "https://youtu.be/abc", "https://notx.com/u/status/1"] {
            assert!(!is_twitter_url(url), "{url}");
        }
    }

    #[test]
    fn bare_channels_go_to_videos_tab() {
        let cases = [
            ("https://www.youtube.com/@MrBeast", "https://www.youtube.com/@MrBeast/videos"),
            ("https://www.youtube.com/@MrBeast/", "https://www.youtube.com/@MrBeast/videos"),
            ("https://youtube.com/channel/UC123", "https://youtube.com/channel/UC123/videos"),
            ("https://www.youtube.com/c/Name?app=desktop", "https://www.youtube.com/c/Name/videos?app=desktop"),
            ("https://www.youtube.com/user/Name", "https://www.youtube.com/user/Name/videos"),
            // уже вкладка или не канал — без изменений
            ("https://www.youtube.com/@MrBeast/videos", "https://www.youtube.com/@MrBeast/videos"),
            ("https://www.youtube.com/@MrBeast/streams", "https://www.youtube.com/@MrBeast/streams"),
            ("https://www.youtube.com/playlist?list=PL123", "https://www.youtube.com/playlist?list=PL123"),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_channel_url(input), want, "{input}");
        }
    }

    #[test]
    fn auth_errors() {
        for msg in [
            "ERROR: [twitter] 123: NSFW tweet requires authentication",
            "Sign in to confirm your age",
            "This account is private",
            "HTTP Error 429: rate limit exceeded",
        ] {
            assert!(needs_auth_error(msg), "{msg}");
        }
        for msg in ["ERROR: unable to download video data: HTTP Error 404: Not Found", "ffmpeg not found"] {
            assert!(!needs_auth_error(msg), "{msg}");
        }
    }

    #[test]
    fn outdated_errors() {
        assert!(looks_outdated(
            "ERROR: [youtube] abc: Unable to extract uploader id; please report this issue on https://github.com/yt-dlp/yt-dlp/issues?q= . Confirm you are on the latest version using yt-dlp -U"
        ));
        assert!(!looks_outdated("ERROR: [youtube] abc: Video unavailable"));
    }
}
