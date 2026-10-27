//! HTTP: превью видео и релизы компонентов (yt-dlp, Deno, ffmpeg).
//!
//! TLS: на Windows и macOS — системный стек (schannel, Security.framework),
//! так приложение уважает корпоративные корневые сертификаты. На Linux —
//! rustls с вшитыми корневыми сертификатами Mozilla: AppImage не должен
//! зависеть от того, какая версия OpenSSL стоит в дистрибутиве.

use std::sync::LazyLock;
use std::time::Duration;

use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

const USER_AGENT: &str = concat!("Lader/", env!("CARGO_PKG_VERSION"));

/// Ответ сервера с телом.
pub type Response = ureq::http::Response<ureq::Body>;

#[cfg(not(target_os = "linux"))]
fn tls_config() -> TlsConfig {
    TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build()
}

#[cfg(target_os = "linux")]
fn tls_config() -> TlsConfig {
    TlsConfig::builder().provider(TlsProvider::Rustls).root_certs(RootCerts::WebPki).build()
}

static AGENT: LazyLock<Agent> = LazyLock::new(|| {
    Agent::config_builder()
        .tls_config(tls_config())
        .user_agent(USER_AGENT)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .build()
        .new_agent()
});

/// Общий HTTP-клиент приложения (пул соединений, TLS, таймауты).
pub(crate) fn agent() -> &'static Agent {
    &AGENT
}

pub(crate) fn describe(err: ureq::Error) -> String {
    match err {
        ureq::Error::StatusCode(code) => format!("сервер ответил HTTP {code}"),
        other => other.to_string(),
    }
}

/// Скачивает небольшой ответ целиком (не больше `limit` байт).
pub fn get_bytes(url: &str, limit: u64, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut resp = AGENT
        .get(url)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .map_err(describe)?;
    resp.body_mut().with_config().limit(limit).read_to_vec().map_err(describe)
}

/// Короткий текстовый ответ: файлы контрольных сумм и т.п.
pub fn get_text(url: &str) -> Result<String, String> {
    let bytes = get_bytes(url, 1 << 20, Duration::from_secs(30))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// GET с JSON-ответом от GitHub API.
pub fn get_github_json(url: &str) -> Result<serde_json::Value, String> {
    let mut resp = AGENT
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .config()
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .call()
        .map_err(describe)?;
    let body = resp.body_mut().with_config().limit(4 << 20).read_to_vec().map_err(describe)?;
    serde_json::from_slice(&body).map_err(|e| format!("некорректный ответ GitHub: {e}"))
}

/// Возвращает адрес, на который перенаправляет `url` (без перехода по нему).
pub fn redirect_target(url: &str) -> Result<String, String> {
    let resp = AGENT
        .get(url)
        .config()
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .call()
        .map_err(describe)?;
    resp.headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| format!("нет перенаправления (HTTP {})", resp.status().as_u16()))
}

/// Скачивает превью и ужимает до `max_w`×`max_h` с сохранением пропорций:
/// в UI оно занимает пару сотен пикселей, а оригинал бывает 1280×720.
pub fn fetch_thumbnail(url: &str, max_w: u32, max_h: u32) -> Result<image::RgbaImage, String> {
    let bytes = get_bytes(url, 8 << 20, Duration::from_secs(20))?;
    let img = image::load_from_memory(&bytes).map_err(|e| format!("не удалось декодировать превью: {e}"))?;
    Ok(img.thumbnail(max_w, max_h).to_rgba8())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ходит в сеть: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn resolves_latest_release_redirect() {
        let target = redirect_target("https://github.com/yt-dlp/yt-dlp/releases/latest").unwrap();
        assert!(target.contains("/releases/tag/"), "{target}");
    }

    /// Ходит в сеть: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn fetches_and_decodes_thumbnails() {
        for url in [
            "https://i.ytimg.com/vi/jNQXAC9IVRw/hqdefault.jpg",
            "https://i.ytimg.com/vi_webp/dQw4w9WgXcQ/maxresdefault.webp",
        ] {
            let img = fetch_thumbnail(url, 184, 104).unwrap_or_else(|e| panic!("{url}: {e}"));
            assert!(img.width() <= 184 && img.height() <= 104 && img.width() > 0, "{url}");
        }
    }
}
