//! Распаковка скачанных архивов компонентов.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Достаёт из zip файлы, для имени которых `wanted` вернул true, и кладёт
/// их плоско в `dest` (вложенные папки архива отбрасываются). Берётся только
/// имя файла, поэтому «../» в путях архива не может вывести за пределы `dest`.
pub fn extract_zip(archive: &Path, dest: &Path, wanted: &dyn Fn(&str) -> bool) -> Result<Vec<PathBuf>, String> {
    let file = File::open(archive).map_err(|e| format!("не удалось открыть архив: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("повреждённый архив: {e}"))?;
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let mut extracted = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| format!("повреждённый архив: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let Some(name) = entry.name().ok().and_then(|n| base_name(&n)) else { continue };
        if !wanted(&name) {
            continue;
        }
        let target = dest.join(&name);
        let mut out = File::create(&target).map_err(|e| format!("не удалось записать {name}: {e}"))?;
        io::copy(&mut entry, &mut out).map_err(|e| format!("ошибка распаковки {name}: {e}"))?;
        extracted.push(target);
    }
    Ok(extracted)
}

/// Распаковывает .tar.xz системным `tar` (есть в любом дистрибутиве Linux
/// и в macOS) и переносит нужные файлы плоско в `dest`.
pub fn extract_tar_xz(archive: &Path, dest: &Path, wanted: &dyn Fn(&str) -> bool) -> Result<Vec<PathBuf>, String> {
    let unpacked = dest.join(".unpacked");
    let _ = fs::remove_dir_all(&unpacked);
    fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    let output = Command::new("tar")
        .arg("-xJf")
        .arg(archive)
        .arg("-C")
        .arg(&unpacked)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("не удалось запустить tar: {e}"))?;
    if !output.status.success() {
        return Err(format!("tar: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    let mut found = Vec::new();
    collect_files(&unpacked, &mut found);
    let mut extracted = Vec::new();
    for path in found {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
        if wanted(&name) {
            let target = dest.join(&name);
            fs::rename(&path, &target).map_err(|e| format!("не удалось перенести {name}: {e}"))?;
            extracted.push(target);
        }
    }
    let _ = fs::remove_dir_all(&unpacked);
    Ok(extracted)
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// "ffmpeg-master/bin/ffmpeg.exe" → "ffmpeg.exe". В zip разделитель — "/",
/// но встречаются архивы, собранные на Windows, с "\".
fn base_name(entry: &str) -> Option<String> {
    entry.rsplit(['/', '\\']).next().filter(|n| !n.is_empty() && *n != "." && *n != "..").map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn extracts_wanted_files_flat() {
        let dir = std::env::temp_dir().join(format!("ytdl-zip-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("a.zip");
        {
            let mut zip = zip::ZipWriter::new(File::create(&archive).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                ("ffmpeg-master/bin/ffmpeg.exe", "ff"),
                ("ffmpeg-master/bin/avcodec-62.dll", "dll"),
                ("ffmpeg-master/bin/ffplay.exe", "play"),
                ("ffmpeg-master/doc/readme.txt", "doc"),
                ("../evil/ffprobe.exe", "probe"),
            ] {
                zip.start_file(name, opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        let out = dir.join("out");
        let wanted = |n: &str| n == "ffmpeg.exe" || n == "ffprobe.exe" || n.ends_with(".dll");
        let mut got: Vec<String> = extract_zip(&archive, &out, &wanted)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, ["avcodec-62.dll", "ffmpeg.exe", "ffprobe.exe"]);
        // «../evil» не вывел файл за пределы папки назначения.
        assert!(out.join("ffprobe.exe").is_file());
        assert!(!dir.join("evil").exists());
        assert_eq!(fs::read_to_string(out.join("ffmpeg.exe")).unwrap(), "ff");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn base_names() {
        assert_eq!(base_name("a/b/deno.exe").as_deref(), Some("deno.exe"));
        assert_eq!(base_name("a\\b\\ffmpeg").as_deref(), Some("ffmpeg"));
        assert_eq!(base_name("dir/"), None);
        assert_eq!(base_name(".."), None);
    }
}
