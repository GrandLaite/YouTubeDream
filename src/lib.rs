//! Ядро Lader: всё, что не зависит от GUI. Работа с yt-dlp и ffmpeg,
//! установка и обновление компонентов, настройки, разбор ссылок.

pub mod archive;
pub mod cancel;
pub mod components;
pub mod fetch;
pub mod format;
pub mod http;
pub mod postprocess;
pub mod process;
pub mod settings;
pub mod urls;
pub mod ytdlp;
