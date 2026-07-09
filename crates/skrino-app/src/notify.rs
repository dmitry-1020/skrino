//! Native OS toast notifications — distinct from the in-app [`crate::toast`]
//! cards. These surface background events (share/save results) even when the
//! app's own window is hidden or about to exit.
//!
//! Always safe to call: no-ops when disabled in config, never panics, and
//! never blocks the calling (typically UI) thread — the actual WinRT call
//! happens on a short-lived detached thread.

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
    if let Err(e) = spawned {
        log::warn!("could not spawn notification thread: {e}");
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
        show("Skrino: тест уведомлений", "Если видно — работает").expect("toast failed");
    }
}
