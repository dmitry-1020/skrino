//! "Launch on system start" via the Windows per-user Run key. No-op elsewhere.

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const VALUE_NAME: &str = "Skrino";

/// Enable or disable auto-start. Returns an error string on failure.
#[cfg(windows)]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run, _) = hkcu
        .create_subkey(RUN_KEY)
        .map_err(|e| e.to_string())?;

    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let cmd = format!("\"{}\" --tray", exe.display());
        run.set_value(VALUE_NAME, &cmd).map_err(|e| e.to_string())
    } else {
        match run.delete_value(VALUE_NAME) {
            Ok(()) => Ok(()),
            // Missing value is fine — already disabled.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(not(windows))]
pub fn set_autostart(_enabled: bool) -> Result<(), String> {
    Ok(())
}
