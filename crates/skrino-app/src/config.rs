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
        use skrino_upload::{Auth, UploadConfig};

        let auth = if self.use_key_file {
            let passphrase = if self.has_passphrase {
                secret_get(KEY_PASSPHRASE).ok().flatten()
            } else {
                None
            };
            Auth::KeyFile {
                path: self.key_file.clone(),
                passphrase,
            }
        } else {
            let password = secret_get(KEY_PASSWORD).ok().flatten()?;
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
    /// Global hotkey for region capture, e.g. "PrintScreen" or "Ctrl+Shift+S".
    pub hotkey: String,
    pub format: ImageFormat,
    /// JPEG quality 1..=100 (ignored for PNG).
    pub jpeg_quality: u8,
    /// Launch in the background on system start (Windows Run key).
    pub autostart: bool,
    /// Last directory used in the Save dialog (remembered between sessions).
    pub last_save_dir: Option<PathBuf>,
    pub upload: UploadSettings,
    /// Destination for the editor's «Поделиться» action.
    #[serde(default)]
    pub share_dest: ShareDestination,
    /// Set once the user has completed first-run setup.
    pub configured: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: Theme::Light,
            hotkey: "PrintScreen".to_string(),
            format: ImageFormat::Png,
            jpeg_quality: 90,
            autostart: false,
            last_save_dir: None,
            upload: UploadSettings::default(),
            share_dest: ShareDestination::default(),
            configured: false,
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

    /// Default directory offered in the Save dialog.
    pub fn default_save_dir(&self) -> PathBuf {
        self.last_save_dir
            .clone()
            .unwrap_or_else(Self::fallback_dir)
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
    fn config_round_trips_through_json() {
        let mut cfg = AppConfig::default();
        cfg.theme = Theme::Light;
        cfg.hotkey = "Ctrl+Shift+S".into();
        cfg.format = ImageFormat::Jpeg;
        cfg.jpeg_quality = 75;
        cfg.autostart = true;
        cfg.last_save_dir = Some(PathBuf::from("C:/shots"));
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
    fn is_configured_requires_host_and_template() {
        let mut u = UploadSettings::default();
        assert!(!u.is_configured());
        u.host = "h".into();
        assert!(!u.is_configured());
        u.url_template = "https://x/{filename}".into();
        assert!(u.is_configured());
    }
}
