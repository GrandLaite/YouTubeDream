//! Помощник браузерного расширения Lader для Microsoft Edge и Chrome.
//!
//! Расширение не может само запускать программы, поэтому всё, что делает
//! окно приложения (yt-dlp, ffmpeg, Deno, многопоточная загрузка, обложки,
//! архив скачанного, установка и обновление компонентов), здесь делает
//! этот помощник. Браузер запускает его сам, когда он нужен расширению,
//! и общается с ним через stdin/stdout по протоколу Native Messaging.
//!
//! Запуск без аргументов (двойной щелчок) регистрирует помощник в Edge
//! и Chrome текущего пользователя, `--uninstall` снимает регистрацию.

mod cookies;
mod host;
mod jobs;
mod protocol;
mod register;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Браузер передаёт первым аргументом адрес вызвавшего расширения,
    // на Windows ещё и своё окно: --parent-window=<HWND>.
    if args.iter().any(|a| a.starts_with("chrome-extension://")) {
        lader_core::settings::migrate_legacy_dirs();
        host::run(parent_window(&args));
        return;
    }
    std::process::exit(register::cli(&args));
}

fn parent_window(args: &[String]) -> Option<isize> {
    args.iter().find_map(|a| a.strip_prefix("--parent-window=")).and_then(|v| v.parse().ok()).filter(|&h| h != 0)
}
