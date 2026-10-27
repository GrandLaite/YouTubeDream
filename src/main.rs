// В релизной сборке — без окна консоли (аналог -H windowsgui у Go).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

mod app;

fn main() -> Result<(), slint::PlatformError> {
    install_crash_log();
    lader_core::settings::migrate_legacy_dirs();
    let ui = AppWindow::new()?;
    app::init(&ui);
    ui.run()
}

/// У GUI-приложения без консоли паника исчезает бесследно — пишем её в
/// %LOCALAPPDATA%\Lader\crash.log.
fn install_crash_log() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = lader_core::settings::data_dir() {
            let _ = std::fs::create_dir_all(&dir);
            let backtrace = std::backtrace::Backtrace::force_capture();
            let _ = std::fs::write(dir.join("crash.log"), format!("{info}\n\n{backtrace}"));
        }
        default_hook(info);
    }));
}
