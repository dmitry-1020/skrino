//! Persistent application configuration (JSON at
//! `dirs::config_dir()/skrino/config.json`) plus OS-keychain helpers for
//! secrets. Secrets (upload password / key passphrase) are NEVER written to the
//! JSON — only `has_password` / `has_passphrase` booleans are.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use skrino_upload::Protocol;

use crate::theme::Theme;

/// Service name used for all keychain entries.
const KEYRING_SERVICE: &str = "skrino";
const KEY_PASSWORD: &str = "upload_password";
const KEY_PASSPHRASE: &str = "upload_passphrase";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
        }
    }
}

/// Where the editor's «Поделиться» button sends the rendered screenshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ShareDestination {
    /// Save into a local folder (no network); default `Pictures\Skrino`.
    LocalDir { path: PathBuf },
    /// Upload to the configured FTP/SFTP server.
    Server,
}

impl Default for ShareDestination {
    fn default() -> Self {
        ShareDestination::LocalDir {
            path: AppConfig::fallback_dir(),
        }
    }
}

/// Upload settings as edited in the UI. Converted to
/// [`skrino_upload::UploadConfig`] (with the secret pulled from the keychain)
/// only when an actual transfer runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadSettings {
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// True when a password is stored in the OS keychain.
    pub has_password: bool,
    /// SFTP: use a private key file instead of a password.
    pub use_key_file: bool,
    pub key_file: String,
    /// True when a key passphrase is stored in the OS keychain.
    pub has_passphrase: bool,
    pub remote_dir: String,
    pub url_template: String,
}

impl Default for UploadSettings {
    fn default() -> Self {
        Self {
            protocol: Protocol::Sftp,
            host: String::new(),
            port: skrino_upload::UploadConfig::default_port(Protocol::Sftp),
            username: String::new(),
            has_password: false,
            use_key_file: false,
            key_file: String::new(),
            has_passphrase: false,
            remote_dir: String::new(),
            url_template: String::new(),
        }
    }
}

impl UploadSettings {
    /// Configured enough to attempt a share (host + public URL template set).
    pub fn is_configured(&self) -> bool {
        !self.host.trim().is_empty() && self.url_template.contains("{filename}")
    }

    /// Build a `skrino_upload::UploadConfig`, pulling secrets from the keychain.
    /// Returns `None` if a required secret is missing.
    pub fn to_upload_config(&self) -> Option<skrino_upload::UploadConfig> {
        self.build_upload_config(None, None)
    }

    /// Build a `skrino_upload::UploadConfig` for a "test connection" against the
    /// *unsaved* settings form. A freshly-typed password/passphrase is passed in
    /// via `typed_*` and used directly (never round-tripped through the keychain);
    /// when the field was left blank we fall back to a previously-saved secret.
    /// Returns `None` if no usable secret is available.
    pub fn to_upload_config_staged(
        &self,
        typed_password: &str,
        typed_passphrase: &str,
    ) -> Option<skrino_upload::UploadConfig> {
        let pw = (!typed_password.is_empty()).then(|| typed_password.to_string());
        let pp = (!typed_passphrase.is_empty()).then(|| typed_passphrase.to_string());
        self.build_upload_config(pw, pp)
    }

    /// Shared builder: `override_*` secrets win over the keychain when present.
    fn build_upload_config(
        &self,
        override_password: Option<String>,
        override_passphrase: Option<String>,
    ) -> Option<skrino_upload::UploadConfig> {
        use skrino_upload::{Auth, UploadConfig};

        let auth = if self.use_key_file {
            let passphrase = override_passphrase.or_else(|| {
                if self.has_passphrase {
                    secret_get(KEY_PASSPHRASE).ok().flatten()
                } else {
                    None
                }
            });
            Auth::KeyFile {
                path: self.key_file.clone(),
                passphrase,
            }
        } else {
            let password = match override_password {
                Some(p) => p,
                None => secret_get(KEY_PASSWORD).ok().flatten()?,
            };
            Auth::Password(password)
        };

        Some(UploadConfig {
            protocol: self.protocol,
            host: self.host.trim().to_string(),
            port: self.port,
            username: self.username.clone(),
            auth,
            remote_dir: self.remote_dir.clone(),
            url_template: self.url_template.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub theme: Theme,
    /// Global hotkey for region capture, e.g. "Ctrl+Shift+3".
    pub hotkey: String,
    /// Global hotkey for full-screen capture, e.g. "Ctrl+Shift+4". Added later,
    /// so old configs (which lack the key) fall back via `default_hotkey_full`.
    #[serde(default = "default_hotkey_full")]
    pub hotkey_full: String,
    /// Global hotkey that starts (and, while a recording is active, stops)
    /// a region screen recording. Added later, so old configs fall back via
    /// `default_hotkey_record`.
    #[serde(default = "default_hotkey_record")]
    pub hotkey_record: String,
    /// Global hotkey for full-monitor screen recording (start/stop toggle).
    #[serde(default = "default_hotkey_record_full")]
    pub hotkey_record_full: String,
    /// Recording frame rate (frames per second): 15, 30 or 60.
    #[serde(default = "default_record_fps")]
    pub record_fps: u32,
    /// Whether the mouse cursor is drawn into the recording.
    #[serde(default = "default_true")]
    pub record_cursor: bool,
    pub format: ImageFormat,
    /// JPEG quality 1..=100 (ignored for PNG).
    pub jpeg_quality: u8,
    /// Launch in the background on system start (Windows Run key).
    pub autostart: bool,
    /// Remembered save folder: the default directory offered in the Save
    /// dialog, and (when `ask_where_to_save` is false) the silent-save target.
    /// Was named `last_save_dir` before the folder became reusable for silent
    /// saves; the alias keeps old configs loading unchanged.
    #[serde(alias = "last_save_dir")]
    pub save_dir: Option<PathBuf>,
    /// Whether «Сохранить» asks where to save every time (rfd dialog) or
    /// saves straight into `save_dir` with a generated filename.
    #[serde(default = "default_true")]
    pub ask_where_to_save: bool,
    pub upload: UploadSettings,
    /// Destination for the editor's «Поделиться» action.
    #[serde(default)]
    pub share_dest: ShareDestination,
    /// Set once the user has completed first-run setup.
    pub configured: bool,
    /// Show native OS toast notifications for background events (share/save
    /// results). In-app toasts are unaffected by this flag.
    #[serde(default = "default_true")]
    pub notifications: bool,
}

/// `#[serde(default = "...")]` needs a named function; used for fields that
/// default to `true` (old configs lacking the key get the new default).
fn default_true() -> bool {
    true
}

/// Default full-screen hotkey (also used by `#[serde(default)]` for old configs).
fn default_hotkey_full() -> String {
    "Ctrl+Shift+4".to_string()
}

/// Default region-recording hotkey (also `#[serde(default)]` for old configs).
fn default_hotkey_record() -> String {
    "Ctrl+Shift+5".to_string()
}

/// Default full-monitor recording hotkey.
fn default_hotkey_record_full() -> String {
    "Ctrl+Shift+6".to_string()
}

/// Default recording frame rate.
fn default_record_fps() -> u32 {
    30
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: Theme::Light,
            hotkey: "Ctrl+Shift+3".to_string(),
            hotkey_full: default_hotkey_full(),
            hotkey_record: default_hotkey_record(),
            hotkey_record_full: default_hotkey_record_full(),
            record_fps: default_record_fps(),
            record_cursor: true,
            format: ImageFormat::Png,
            jpeg_quality: 90,
            autostart: false,
            save_dir: None,
            ask_where_to_save: true,
            upload: UploadSettings::default(),
            share_dest: ShareDestination::default(),
            configured: false,
            notifications: true,
        }
    }
}

impl AppConfig {
    /// Path to the JSON config file.
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("skrino").join("config.json"))
    }

    /// Local fallback directory where shots are auto-saved when an upload fails.
    pub fn fallback_dir() -> PathBuf {
        dirs::picture_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Skrino")
    }

    /// Default directory offered in the Save dialog (and, when
    /// `ask_where_to_save` is false, the silent-save target).
    pub fn default_save_dir(&self) -> PathBuf {
        self.save_dir.clone().unwrap_or_else(Self::fallback_dir)
    }

    /// Load config, falling back to defaults on any error (missing file,
    /// corrupt JSON, …) so the app always starts.
    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("config parse failed ({e}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Persist to disk. Errors are logged, never fatal.
    pub fn save(&self) {
        let Some(path) = Self::config_path() else {
            log::warn!("no config dir; not saving");
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("cannot create config dir: {e}");
                return;
            }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    log::warn!("cannot write config: {e}");
                }
            }
            Err(e) => log::warn!("cannot serialise config: {e}"),
        }
    }

    // --- secret helpers (thin wrappers keyed by this config's semantics) ---

    pub fn set_password(&mut self, password: &str) {
        if password.is_empty() {
            let _ = secret_delete(KEY_PASSWORD);
            self.upload.has_password = false;
        } else if secret_set(KEY_PASSWORD, password).is_ok() {
            self.upload.has_password = true;
        }
    }

    pub fn set_passphrase(&mut self, passphrase: &str) {
        if passphrase.is_empty() {
            let _ = secret_delete(KEY_PASSPHRASE);
            self.upload.has_passphrase = false;
        } else if secret_set(KEY_PASSPHRASE, passphrase).is_ok() {
            self.upload.has_passphrase = true;
        }
    }
}

// --- keychain backend (OS-specific) ---

#[cfg(windows)]
fn secret_set(key: &str, value: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, key).map_err(|e| e.to_string())?;
    entry.set_password(value).map_err(|e| e.to_string())
}

#[cfg(windows)]
fn secret_get(key: &str) -> Result<Option<String>, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, key).map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(windows)]
fn secret_delete(key: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, key).map_err(|e| e.to_string())?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

// Non-Windows fallback: no secure store wired up (the app targets Windows).
#[cfg(not(windows))]
fn secret_set(_key: &str, _value: &str) -> Result<(), String> {
    Err("secret storage unavailable on this platform".into())
}
#[cfg(not(windows))]
fn secret_get(_key: &str) -> Result<Option<String>, String> {
    Ok(None)
}
#[cfg(not(windows))]
fn secret_delete(_key: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Reassigning fields one-by-one (rather than a giant struct literal) keeps
    // this test readable as a checklist of "every field round-trips".
    #[allow(clippy::field_reassign_with_default)]
    fn config_round_trips_through_json() {
        let mut cfg = AppConfig::default();
        cfg.theme = Theme::Light;
        cfg.hotkey = "Ctrl+Shift+S".into();
        cfg.format = ImageFormat::Jpeg;
        cfg.jpeg_quality = 75;
        cfg.autostart = true;
        cfg.save_dir = Some(PathBuf::from("C:/shots"));
        cfg.ask_where_to_save = false;
        cfg.notifications = false;
        cfg.hotkey_record = "Ctrl+Alt+5".into();
        cfg.hotkey_record_full = "Ctrl+Alt+6".into();
        cfg.record_fps = 60;
        cfg.record_cursor = false;
        cfg.upload.host = "example.com".into();
        cfg.upload.protocol = Protocol::Ftps;
        cfg.upload.url_template = "https://example.com/s/{filename}".into();
        cfg.upload.has_password = true;
        cfg.share_dest = ShareDestination::LocalDir {
            path: PathBuf::from("C:/shots/shared"),
        };

        let text = serde_json::to_string_pretty(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(cfg, back);

        // Server variant round-trips too.
        cfg.share_dest = ShareDestination::Server;
        let text = serde_json::to_string_pretty(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn unknown_and_missing_fields_use_defaults() {
        // Forward/backward compatible: an empty object yields all defaults.
        let back: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(back, AppConfig::default());
    }

    #[test]
    fn old_config_without_share_dest_defaults_to_local_dir() {
        // A config.json written by an earlier version has no `share_dest` key.
        let json = r#"{
            "theme": "Light",
            "hotkey": "PrintScreen",
            "format": "Png",
            "jpeg_quality": 90,
            "autostart": false,
            "last_save_dir": null,
            "configured": true
        }"#;
        let back: AppConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(back.share_dest, ShareDestination::LocalDir { .. }));
        assert!(back.configured);
    }

    #[test]
    fn old_config_without_hotkey_full_gets_default() {
        // Earlier configs have no `hotkey_full`; it must fall back to the default
        // while the user's saved region `hotkey` (e.g. old "PrintScreen") wins.
        let json = r#"{
            "theme": "Light",
            "hotkey": "PrintScreen",
            "format": "Png",
            "jpeg_quality": 90,
            "autostart": false,
            "last_save_dir": null,
            "configured": true
        }"#;
        let back: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(back.hotkey, "PrintScreen");
        assert_eq!(back.hotkey_full, "Ctrl+Shift+4");
    }

    #[test]
    fn old_config_with_legacy_last_save_dir_key_aliases_to_save_dir() {
        // A config.json written before `save_dir` was renamed from
        // `last_save_dir` must still populate the (renamed) field via alias.
        let json = r#"{
            "theme": "Light",
            "hotkey": "PrintScreen",
            "format": "Png",
            "jpeg_quality": 90,
            "autostart": false,
            "last_save_dir": "C:/old/shots",
            "configured": true
        }"#;
        let back: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(back.save_dir, Some(PathBuf::from("C:/old/shots")));
    }

    #[test]
    fn old_config_without_ask_where_to_save_defaults_to_true() {
        // Earlier configs have no `ask_where_to_save` key; must default to
        // asking every time (unchanged prior behaviour) rather than silently
        // saving somewhere the user never picked.
        let json = r#"{
            "theme": "Light",
            "hotkey": "PrintScreen",
            "format": "Png",
            "jpeg_quality": 90,
            "autostart": false,
            "configured": true
        }"#;
        let back: AppConfig = serde_json::from_str(json).unwrap();
        assert!(back.ask_where_to_save);
    }

    #[test]
    fn old_config_without_notifications_defaults_to_true() {
        // Earlier configs have no `notifications` key; must default to shown.
        let json = r#"{
            "theme": "Light",
            "hotkey": "PrintScreen",
            "format": "Png",
            "jpeg_quality": 90,
            "autostart": false,
            "configured": true
        }"#;
        let back: AppConfig = serde_json::from_str(json).unwrap();
        assert!(back.notifications);
    }

    #[test]
    fn default_hotkeys_are_ctrl_shift_digits() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.hotkey, "Ctrl+Shift+3");
        assert_eq!(cfg.hotkey_full, "Ctrl+Shift+4");
        // Both defaults must parse and be distinct.
        assert!(crate::hotkey::parse(&cfg.hotkey).is_ok());
        assert!(crate::hotkey::parse(&cfg.hotkey_full).is_ok());
        assert_ne!(cfg.hotkey, cfg.hotkey_full);
    }

    #[test]
    fn default_record_hotkeys_and_options() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.hotkey_record, "Ctrl+Shift+5");
        assert_eq!(cfg.hotkey_record_full, "Ctrl+Shift+6");
        assert_eq!(cfg.record_fps, 30);
        assert!(cfg.record_cursor);
        // Record hotkeys must parse and be distinct from each other and the
        // capture hotkeys.
        assert!(crate::hotkey::parse(&cfg.hotkey_record).is_ok());
        assert!(crate::hotkey::parse(&cfg.hotkey_record_full).is_ok());
        for a in [&cfg.hotkey, &cfg.hotkey_full, &cfg.hotkey_record] {
            assert_ne!(a, &cfg.hotkey_record_full);
        }
        assert_ne!(cfg.hotkey_record, cfg.hotkey_record_full);
    }

    #[test]
    fn old_config_without_record_fields_gets_defaults() {
        // Earlier configs predate recording entirely; every record field must
        // fall back to its default while the rest of the config loads.
        let json = r#"{
            "theme": "Light",
            "hotkey": "PrintScreen",
            "format": "Png",
            "jpeg_quality": 90,
            "autostart": false,
            "configured": true
        }"#;
        let back: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(back.hotkey_record, "Ctrl+Shift+5");
        assert_eq!(back.hotkey_record_full, "Ctrl+Shift+6");
        assert_eq!(back.record_fps, 30);
        assert!(back.record_cursor);
    }

    #[test]
    fn is_configured_requires_host_and_template() {
        let mut u = UploadSettings::default();
        assert!(!u.is_configured());
        u.host = "h".into();
        assert!(!u.is_configured());
        u.url_template = "https://x/{filename}".into();
        assert!(u.is_configured());
    }
}
