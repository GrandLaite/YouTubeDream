//! Запуск внешних программ (yt-dlp, ffmpeg) без мигающих окон консоли,
//! с таймаутом и убийством всего дерева процессов.

use std::ffi::OsStr;
use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::cancel::CancelToken;

/// Создаёт `Command` для внешней программы. На Windows она не открывает
/// окно консоли (GUI-приложение иначе мигало бы чёрным окном на каждый
/// вызов), на macOS и Linux запускается в собственной группе процессов —
/// чтобы отмена могла убить её вместе с потомками.
pub fn command<S: AsRef<OsStr>>(program: S) -> Command {
    let mut cmd = Command::new(program);
    detach(&mut cmd);
    cmd
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

/// Убивает процесс вместе со всеми потомками. На Windows `Child::kill`
/// убивает только сам yt-dlp, а запущенный им ffmpeg продолжает работать —
/// поэтому `taskkill /T` (дерево) `/F` (принудительно).
#[cfg(windows)]
pub fn kill_tree(pid: u32) {
    let _ = command("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Процесс запущен лидером своей группы (см. `command`), поэтому сигнал
/// группе убивает и его, и запущенные им ffmpeg.
#[cfg(unix)]
pub fn kill_tree(pid: u32) {
    // SAFETY: kill() только отправляет сигнал; неверный pid даёт ESRCH.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

/// Результат завершившейся команды.
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Почему команда не дошла до штатного завершения.
#[derive(Debug)]
pub enum RunError {
    Spawn(io::Error),
    TimedOut,
    Cancelled,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Spawn(e) => write!(f, "не удалось запустить: {e}"),
            RunError::TimedOut => f.write_str("превышено время ожидания"),
            RunError::Cancelled => f.write_str("отменено"),
        }
    }
}

/// Запускает команду, собирая stdout и stderr целиком. По таймауту или
/// отмене убивает всё дерево процессов.
pub fn run(mut cmd: Command, timeout: Duration, cancel: Option<&CancelToken>) -> Result<Output, RunError> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
    if let Some(c) = cancel {
        c.attach(child.id());
    }

    // Читаем оба потока в отдельных потоках: если читать последовательно,
    // процесс может зависнуть, заполнив буфер второго пайпа.
    let out_reader = spawn_reader(child.stdout.take());
    let err_reader = spawn_reader(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let mut outcome = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if cancel.is_some_and(CancelToken::is_cancelled) {
            outcome = Some(RunError::Cancelled);
        } else if Instant::now() >= deadline {
            outcome = Some(RunError::TimedOut);
        }
        if outcome.is_some() {
            kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(40));
    };
    if let Some(c) = cancel {
        c.detach();
    }

    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if let Some(err) = outcome {
        return Err(err);
    }
    let status = match status {
        Some(s) => s,
        None => child.wait().map_err(RunError::Spawn)?,
    };
    Ok(Output { status, stdout, stderr })
}

fn spawn_reader<R: Read + Send + 'static>(stream: Option<R>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stream {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    })
}

/// Декодирует вывод внешней программы. Мы просим yt-dlp писать в UTF-8
/// (`--encoding utf-8`), но на случай чужого вывода не падаем на мусоре.
pub fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Имя исполняемого файла на текущей ОС: "ffmpeg" → "ffmpeg.exe" на Windows.
pub fn exe_name(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// Ищет исполняемый файл в PATH. На macOS дополнительно смотрит папки
/// Homebrew: приложение, запущенное из Finder, получает урезанный PATH
/// без /opt/homebrew/bin.
pub fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    let file = exe_name(name);
    let mut dirs: Vec<std::path::PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if cfg!(target_os = "macos") {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(std::path::PathBuf::from));
    }
    dirs.into_iter().map(|dir| dir.join(&file)).find(|candidate| candidate.is_file())
}
