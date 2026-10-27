//! Кооперативная отмена долгих операций с yt-dlp.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::process::kill_tree;

/// Токен отмены. Клонируется между UI и рабочим потоком: UI вызывает
/// `cancel()`, рабочий поток регистрирует PID текущего процесса через
/// `attach()`, чтобы отмена мгновенно убивала yt-dlp вместе с ffmpeg.
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    // Флаг и PID меняются под одним мьютексом — иначе отмена, пришедшая
    // между spawn и attach, потерялась бы и процесс доработал бы до конца.
    pid: Mutex<Option<u32>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        let pid = {
            let mut guard = self.inner.pid.lock().unwrap_or_else(|e| e.into_inner());
            self.inner.cancelled.store(true, Ordering::SeqCst);
            guard.take()
        };
        if let Some(pid) = pid {
            kill_tree(pid);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn attach(&self, pid: u32) {
        let kill_now = {
            let mut guard = self.inner.pid.lock().unwrap_or_else(|e| e.into_inner());
            if self.inner.cancelled.load(Ordering::SeqCst) {
                true
            } else {
                *guard = Some(pid);
                false
            }
        };
        if kill_now {
            kill_tree(pid);
        }
    }

    pub(crate) fn detach(&self) {
        *self.inner.pid.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}
