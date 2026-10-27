//! embed-thumbs — разовая утилита: рекурсивно обходит папку, для каждого
//! mp4/m4a/mp3 ищет превью (.jpg/.jpeg/.png/.webp) в `_thumbs/` или рядом
//! и встраивает его в файл как обложку через ffmpeg. После успешного
//! встраивания превью удаляется, пустые `_thumbs/` убираются в конце.
//!
//! Использование:
//!
//! ```text
//! embed-thumbs.exe                       # текущая папка
//! embed-thumbs.exe "D:\YT\МойКанал"      # конкретная папка
//! embed-thumbs.exe --dry-run "D:\YT"     # без изменения файлов
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lader_core::components::{self, Component};
use lader_core::postprocess;

fn main() -> ExitCode {
    let mut dry_run = false;
    let mut root = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--dry-run" | "-dry-run" => dry_run = true,
            "-h" | "--help" => {
                println!("Использование: embed-thumbs [--dry-run] [ПАПКА]");
                return ExitCode::SUCCESS;
            }
            _ => root = Some(PathBuf::from(arg)),
        }
    }
    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let root = std::path::absolute(&root).unwrap_or(root);

    let Some(ffmpeg) = components::find(Component::Ffmpeg).map(|i| i.path) else {
        eprintln!("ffmpeg не найден в PATH");
        return ExitCode::FAILURE;
    };

    println!("Папка: {}", root.display());
    if dry_run {
        println!("Режим: dry-run (файлы не изменяются)");
    }
    println!();

    let mut stats = Stats::default();
    walk(&root, &mut |path| process(&root, path, &ffmpeg, dry_run, &mut stats));
    if !dry_run {
        clean_empty_thumb_dirs(&root);
    }

    println!(
        "\nГотово. Найдено: {}, встроено: {}, без превью: {}, ошибок: {}",
        stats.found, stats.embedded, stats.skipped, stats.failed
    );
    if stats.failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

#[derive(Default)]
struct Stats {
    found: usize,
    embedded: usize,
    skipped: usize,
    failed: usize,
}

fn process(root: &Path, path: &Path, ffmpeg: &Path, dry_run: bool, stats: &mut Stats) {
    if !postprocess::is_embeddable(path) {
        return;
    }
    stats.found += 1;
    let Some(thumb) = postprocess::find_thumbnail(path) else {
        stats.skipped += 1;
        return;
    };
    let rel = path.strip_prefix(root).unwrap_or(path);
    println!(
        "[{}] {}\n        ← {}",
        stats.found,
        rel.display(),
        thumb.file_name().unwrap_or_default().to_string_lossy()
    );
    if dry_run {
        return;
    }
    match postprocess::embed_thumbnail(ffmpeg, path, &thumb) {
        Ok(()) => {
            stats.embedded += 1;
            if let Err(e) = fs::remove_file(&thumb) {
                eprintln!("        не удалось удалить превью: {e}");
            }
        }
        Err(e) => {
            stats.failed += 1;
            eprintln!("        ошибка: {e}");
        }
    }
}

/// Рекурсивный обход файлов; содержимое `_thumbs/` не обходится.
fn walk(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != postprocess::THUMBS_DIR) {
                walk(&path, visit);
            }
        } else {
            visit(&path);
        }
    }
}

fn clean_empty_thumb_dirs(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        if path.file_name().is_some_and(|n| n == postprocess::THUMBS_DIR) {
            if fs::read_dir(&path).is_ok_and(|mut it| it.next().is_none()) {
                let _ = fs::remove_dir(&path);
            }
        } else {
            clean_empty_thumb_dirs(&path);
        }
    }
}
