//! Native OS toast notifications — distinct from the in-app [`crate::toast`]
//! cards. These surface background events (share/save results) even when the
//! app's own window is hidden or about to exit.
//!
//! Always safe to call: no-ops when disabled in config, never panics, and
//! never blocks the calling (typically UI) thread — the actual WinRT call
//! happens on a short-lived detached thread.

use std::sync::Mutex;
use std::thread::JoinHandle;

/// Detached notification threads that haven't been waited on yet. `flush`
/// joins them before process exit so a just-fired toast isn't lost when the
/// process quits immediately after an action (copy/share auto-close).
static PENDING: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

/// Fire a system notification with `title`/`body` when `enabled`. Any failure
/// (no notifier registered, WinRT error, …) is logged and swallowed.
pub fn notify(title: impl Into<String>, body: impl Into<String>, enabled: bool) {
    if !enabled {
        return;
    }
    let title = title.into();
    let body = body.into();
    let spawned = std::thread::Builder::new()
        .name("skrino-notify".into())
        .spawn(move || {
            if let Err(e) = show(&title, &body) {
                log::warn!("system notification failed: {e}");
            }
        });
    match spawned {
        Ok(handle) => {
            if let Ok(mut pending) = PENDING.lock() {
                // Drop already-finished handles so the list never grows.
                pending.retain(|h| !h.is_finished());
                pending.push(handle);
            }
        }
        Err(e) => log::warn!("could not spawn notification thread: {e}"),
    }
}

/// Wait for in-flight notifications to be handed to the OS. Call right before
/// `process::exit` — the WinRT `show` call is fast, so this returns almost
/// immediately in practice; it only prevents killing the thread mid-call.
pub fn flush() {
    let handles: Vec<JoinHandle<()>> = match PENDING.lock() {
        Ok(mut pending) => pending.drain(..).collect(),
        Err(_) => return,
    };
    for h in handles {
        let _ = h.join();
    }
}

#[cfg(windows)]
fn show(title: &str, body: &str) -> Result<(), String> {
    use tauri_winrt_notification::Toast;
    Toast::new(Toast::POWERSHELL_APP_ID)
        .title(title)
        .text1(body)
        .show()
        .map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn show(_title: &str, _body: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not run by default (`cargo test` stays quiet/CI-safe) — pops a real
    /// Windows toast for manual verification: `cargo test -p skrino-app
    /// notify::tests::fires_a_real_toast -- --ignored`.
    #[test]
    #[ignore]
    fn fires_a_real_toast() {
        show("Skrino: тест уведомлений", "Если видно, значит работает").expect("toast failed");
    }
}
