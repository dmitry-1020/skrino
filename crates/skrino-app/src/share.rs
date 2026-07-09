//! Background upload orchestration. The blocking `skrino_upload::upload` call
//! runs on a worker thread; the result comes back over an mpsc channel that the
//! UI polls each frame. On failure the PNG is auto-saved locally so the shot is
//! never lost.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

use skrino_upload::{UploadConfig, UploadError};

pub enum ShareResult {
    /// Uploaded; here is the public URL.
    Success(String),
    /// Upload failed. `saved_to` is the local fallback path if the save worked.
    Failure {
        error: String,
        /// The failure was an authentication error (bad password/key) — the UI
        /// offers an «Открыть настройки» rescue on top of «Повторить».
        auth: bool,
        saved_to: Option<PathBuf>,
    },
}

/// A single in-flight upload. Dropping it detaches the worker (result ignored).
pub struct ShareHandle {
    rx: Receiver<ShareResult>,
    done: bool,
}

impl ShareHandle {
    pub fn spawn(
        config: UploadConfig,
        filename: String,
        png_bytes: Vec<u8>,
        fallback_dir: PathBuf,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("skrino-upload".into())
            .spawn(move || {
                let result = match skrino_upload::upload(&config, &filename, &png_bytes) {
                    Ok(url) => ShareResult::Success(url),
                    Err(e) => {
                        let auth = matches!(e, UploadError::Auth(_));
                        let saved_to = save_fallback(&fallback_dir, &filename, &png_bytes);
                        ShareResult::Failure {
                            error: e.to_string(),
                            auth,
                            saved_to,
                        }
                    }
                };
                // Receiver may be gone if the editor closed; ignore send errors.
                let _ = tx.send(result);
            })
            .expect("failed to spawn upload thread");

        Self { rx, done: false }
    }

    /// True while the upload is still running.
    pub fn in_flight(&self) -> bool {
        !self.done
    }

    /// Non-blocking poll. Returns `Some` exactly once, when the worker finishes.
    pub fn poll(&mut self) -> Option<ShareResult> {
        if self.done {
            return None;
        }
        match self.rx.try_recv() {
            Ok(res) => {
                self.done = true;
                Some(res)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.done = true;
                Some(ShareResult::Failure {
                    error: "рабочий поток завершился без ответа".into(),
                    auth: false,
                    saved_to: None,
                })
            }
        }
    }
}

/// Write the PNG to the local fallback directory. Returns the path on success.
fn save_fallback(dir: &PathBuf, filename: &str, bytes: &[u8]) -> Option<PathBuf> {
    if std::fs::create_dir_all(dir).is_err() {
        return None;
    }
    let path = dir.join(filename);
    match std::fs::write(&path, bytes) {
        Ok(()) => Some(path),
        Err(_) => None,
    }
}
