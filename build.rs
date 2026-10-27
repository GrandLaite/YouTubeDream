fn main() {
    // Тёмная тема Fluent — как у Windows 11 и как было в Fyne-версии.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI must compile");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources();
    }
}

/// Иконка и сведения о версии внутри .exe — их показывают Проводник и
/// «Программы и компоненты». Нужен компилятор ресурсов (rc.exe из Windows
/// SDK или windres из MinGW); без него сборка не падает, просто .exe
/// останется со стандартной иконкой.
#[cfg(windows)]
fn embed_windows_resources() {
    println!("cargo:rerun-if-changed=packaging/icons/icon.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("packaging/icons/icon.ico")
        .set("ProductName", "Lader")
        .set("FileDescription", "Lader")
        .set("LegalCopyright", "GrandLaite");
    if let Err(e) = res.compile() {
        println!("cargo:warning=иконка не встроена в .exe: {e}");
    }
}

#[cfg(not(windows))]
fn embed_windows_resources() {}
