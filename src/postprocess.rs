//! Постобработка: обложки внутри файлов и уборка после отмены.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::process;

/// Подпапка, куда yt-dlp складывает превью (`-o thumbnail:…`).
pub const THUMBS_DIR: &str = "_thumbs";

const THUMB_EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];
const EMBEDDABLE: [&str; 3] = ["mp4", "m4a", "mp3"];

fn ext_lower(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Можно ли встроить обложку в файл с таким расширением.
pub fn is_embeddable(path: &Path) -> bool {
    EMBEDDABLE.contains(&ext_lower(path).as_str())
}

/// Ищет превью для медиафайла: сначала в `_thumbs/`, затем рядом с файлом.
pub fn find_thumbnail(media: &Path) -> Option<PathBuf> {
    let dir = media.parent()?;
    let base = stem(media);
    for d in [dir.join(THUMBS_DIR), dir.to_path_buf()] {
        for ext in THUMB_EXTS {
            let candidate = d.join(format!("{base}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Встраивает обложку в mp4/m4a/mp3 через ffmpeg без перекодирования.
/// Запись идёт во временный файл, который затем подменяет исходный —
/// при ошибке исходник остаётся целым.
pub fn embed_thumbnail(ffmpeg: &Path, media: &Path, thumb: &Path) -> Result<(), String> {
    let ext = ext_lower(media);
    let tmp = PathBuf::from(format!("{}.embed_tmp.{ext}", media.display()));
    let codec_args: &[&str] = match ext.as_str() {
        "mp4" | "m4a" | "mov" => &["-map", "0", "-map", "1", "-c", "copy", "-disposition:v:1", "attached_pic"],
        "mp3" => &[
            "-map",
            "0:0",
            "-map",
            "1:0",
            "-c",
            "copy",
            "-id3v2_version",
            "3",
            "-metadata:s:v",
            "title=Album cover",
            "-metadata:s:v",
            "comment=Cover (front)",
        ],
        other => return Err(format!("встраивание обложки не поддерживается для .{other}")),
    };

    let output = process::command(ffmpeg)
        .args(["-loglevel", "error", "-i"])
        .arg(media)
        .arg("-i")
        .arg(thumb)
        .args(codec_args)
        .arg("-y")
        .arg(&tmp)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("не удалось запустить ffmpeg: {e}"))?;
    if !output.status.success() {
        let _ = fs::remove_file(&tmp);
        let msg = process::decode(&output.stderr).trim().to_string();
        return Err(format!("ffmpeg: {}", if msg.is_empty() { output.status.to_string() } else { msg }));
    }
    fs::rename(&tmp, media).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("не удалось заменить файл: {e}")
    })
}

/// Проходит по папке и встраивает обложки во все mp4/m4a/mp3, у которых
/// есть одноимённое превью. Уже обработанные файлы пропускаются сами —
/// их превью удалено после встраивания. В конце удаляет пустую `_thumbs/`.
///
/// Имя итогового файла из вывода yt-dlp не угадываем (промежуточные fNNN,
/// merge, разные версии) — просто обходим папку.
pub fn embed_thumbnails_in_dir(ffmpeg: &Path, dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || !is_embeddable(&path) {
            continue;
        }
        if let Some(thumb) = find_thumbnail(&path)
            && embed_thumbnail(ffmpeg, &path, &thumb).is_ok()
        {
            let _ = fs::remove_file(thumb);
        }
    }
    remove_if_empty(&dir.join(THUMBS_DIR));
}

/// Удаляет следы прерванной загрузки: `.part`/`.ytdl`/`.lader-part`, промежуточные
/// fNNN-файлы (Video.f137.mp4, Video.f140.m4a), временные файлы встраивания
/// и превью. `last_file` — последний известный файл текущей загрузки: по нему
/// находим её промежуточные файлы. Загрузка у нас всегда одна, поэтому
/// любые `.part` в папке принадлежат ей.
pub fn cleanup_partials(dir: &Path, last_file: Option<&Path>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if entry.path().is_file()
                && (name.ends_with(".part")
                    || name.ends_with(".ytdl")
                    || name.ends_with(".lader-part")
                    || name.contains(".embed_tmp."))
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    if let Some(file) = last_file {
        let base = strip_format_suffix(&stem(file));
        let prefix = format!("{base}.f");
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(rest) = name.strip_prefix(&prefix)
                    && rest.contains('.')
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        // Превью этой же загрузки — тоже мусор: целевого файла нет.
        for d in [dir.to_path_buf(), dir.join(THUMBS_DIR)] {
            for ext in THUMB_EXTS {
                let _ = fs::remove_file(d.join(format!("{base}.{ext}")));
            }
        }
    }
    remove_if_empty(&dir.join(THUMBS_DIR));
}

/// "Video.f137" → "Video": промежуточные файлы yt-dlp имеют суффикс .fNNN
/// (иногда с уточнением: .f140-drc) перед расширением.
pub fn strip_format_suffix(stem: &str) -> String {
    if let Some(pos) = stem.rfind(".f") {
        let tail = &stem[pos + 2..];
        if tail.chars().next().is_some_and(|c| c.is_ascii_digit())
            && tail.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return stem[..pos].to_string();
        }
    }
    stem.to_string()
}

fn remove_if_empty(dir: &Path) {
    if fs::read_dir(dir).is_ok_and(|mut it| it.next().is_none()) {
        let _ = fs::remove_dir(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_format_suffix() {
        assert_eq!(strip_format_suffix("Video.f137"), "Video");
        assert_eq!(strip_format_suffix("Video.f140-drc"), "Video");
        assert_eq!(strip_format_suffix("My.fancy video"), "My.fancy video");
        assert_eq!(strip_format_suffix("Video"), "Video");
    }

    #[test]
    fn cleanup_removes_partials_of_current_download() {
        let dir = std::env::temp_dir().join(format!("ytdl-cleanup-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(THUMBS_DIR)).unwrap();
        for name in ["Clip.f137.mp4.part", "Clip.f140.m4a", "Clip.f137.mp4", "Other.mp4", "Clip.jpg"] {
            fs::write(dir.join(name), b"x").unwrap();
        }
        fs::write(dir.join(THUMBS_DIR).join("Clip.jpg"), b"x").unwrap();

        cleanup_partials(&dir, Some(&dir.join("Clip.f137.mp4")));

        let mut left: Vec<String> =
            fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["Other.mp4"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
