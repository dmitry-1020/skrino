//! skrino-upload — publish screenshots to the user's server and produce a
//! shareable URL.
//!
//! Protocols: FTP, FTPS (explicit TLS), SFTP (SSH). All calls are blocking —
//! run them from a worker thread; internally SFTP may spin up a small tokio
//! runtime (russh), that's an implementation detail.

mod ftp;
mod sftp;

use rand::Rng;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    Ftp,
    Ftps,
    Sftp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Auth {
    Password(String),
    /// SFTP only: path to a private key file + optional passphrase.
    KeyFile {
        path: String,
        passphrase: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadConfig {
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: Auth,
    /// Remote directory to upload into, e.g. "/var/www/s" or "htdocs/shots".
    pub remote_dir: String,
    /// Public URL template; `{filename}` is replaced with the uploaded name.
    /// Example: "https://example.com/s/{filename}"
    pub url_template: String,
}

impl UploadConfig {
    pub fn default_port(protocol: Protocol) -> u16 {
        match protocol {
            Protocol::Ftp | Protocol::Ftps => 21,
            Protocol::Sftp => 22,
        }
    }

    /// Build the public URL for an uploaded filename.
    pub fn public_url(&self, filename: &str) -> String {
        self.url_template.replace("{filename}", filename)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("transfer failed: {0}")]
    Transfer(String),
}

/// Charset used for the random parts of generated filenames: lowercase
/// letters and digits only, so results are safe to embed in URLs/paths
/// without escaping.
const RANDOM_CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Generate `len` random lowercase-alphanumeric characters.
fn random_token(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| RANDOM_CHARSET[rng.random_range(0..RANDOM_CHARSET.len())] as char)
        .collect()
}

/// Generate a collision-safe, non-guessable filename:
/// "2026-07-09_14-03-22_kx3f9a.png"
pub fn generate_filename(extension: &str) -> String {
    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let suffix = random_token(6);
    // Strip any dots/slashes the caller might have passed in (e.g. a
    // leading "." or a path-like value); we only want a bare extension.
    let ext: String = extension
        .trim_start_matches('.')
        .chars()
        .filter(|c| !matches!(c, '.' | '/' | '\\'))
        .collect();
    if ext.is_empty() {
        format!("{timestamp}_{suffix}")
    } else {
        format!("{timestamp}_{suffix}.{ext}")
    }
}

/// Upload `bytes` as `filename` into `config.remote_dir`.
/// Creates the remote directory if missing. Returns the public URL.
pub fn upload(config: &UploadConfig, filename: &str, bytes: &[u8]) -> Result<String, UploadError> {
    match config.protocol {
        Protocol::Ftp | Protocol::Ftps => ftp::upload(config, filename, bytes)?,
        Protocol::Sftp => sftp::upload(config, filename, bytes)?,
    }
    Ok(config.public_url(filename))
}

/// Settings-dialog "Test connection": connect, authenticate, verify the remote
/// dir is writable (upload + delete a tiny probe file). Returns a short
/// human-readable success description.
pub fn test_connection(config: &UploadConfig) -> Result<String, UploadError> {
    let probe_name = format!("skrino_test_{}.txt", random_token(8));
    let probe_bytes = b"skrino connectivity probe\n";
    match config.protocol {
        Protocol::Ftp | Protocol::Ftps => ftp::test_connection(config, &probe_name, probe_bytes)?,
        Protocol::Sftp => sftp::test_connection(config, &probe_name, probe_bytes)?,
    }
    Ok("OK: подключение и запись в каталог работают".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::time::Duration;

    #[test]
    fn generate_filename_matches_expected_format() {
        let name = generate_filename("png");
        let re = regex_lite_check(&name);
        assert!(
            re,
            "filename '{name}' does not match YYYY-MM-DD_HH-MM-SS_xxxxxx.png"
        );
        assert!(name.ends_with(".png"));
    }

    /// Hand-rolled format check (no regex dependency in this crate): validates
    /// "YYYY-MM-DD_HH-MM-SS_" followed by 6 lowercase-alphanumeric chars and
    /// then ".png".
    fn regex_lite_check(name: &str) -> bool {
        let stem = match name.strip_suffix(".png") {
            Some(s) => s,
            None => return false,
        };
        let bytes = stem.as_bytes();
        // "YYYY-MM-DD_HH-MM-SS_" is 20 chars, then 6 random chars = 26 total.
        if bytes.len() != 26 {
            return false;
        }
        let is_digit = |b: u8| b.is_ascii_digit();
        type ByteCheck = fn(u8) -> bool;
        let checks: [(usize, ByteCheck); 20] = [
            (0, is_digit),
            (1, is_digit),
            (2, is_digit),
            (3, is_digit),
            (4, |b| b == b'-'),
            (5, is_digit),
            (6, is_digit),
            (7, |b| b == b'-'),
            (8, is_digit),
            (9, is_digit),
            (10, |b| b == b'_'),
            (11, is_digit),
            (12, is_digit),
            (13, |b| b == b'-'),
            (14, is_digit),
            (15, is_digit),
            (16, |b| b == b'-'),
            (17, is_digit),
            (18, is_digit),
            (19, |b| b == b'_'),
        ];
        for (idx, check) in checks {
            if !check(bytes[idx]) {
                return false;
            }
        }
        bytes[20..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    }

    #[test]
    fn generate_filename_strips_leading_dot_and_separators() {
        let name = generate_filename(".png");
        assert!(name.ends_with(".png"));
        assert!(!name.contains(".png.png"));

        let name = generate_filename("jpg/../evil");
        assert!(!name.contains('/'));
        assert!(!name.contains('\\'));
    }

    #[test]
    fn generate_filename_without_extension_has_no_trailing_dot() {
        let name = generate_filename("");
        assert!(!name.ends_with('.'));
        assert!(!name.contains('.'));
    }

    #[test]
    fn generate_filename_is_unique_across_calls() {
        let mut seen = HashSet::new();
        for _ in 0..50 {
            let name = generate_filename("png");
            assert!(seen.insert(name), "generated a duplicate filename");
        }
    }

    #[test]
    fn public_url_substitutes_filename() {
        let config = UploadConfig {
            protocol: Protocol::Ftp,
            host: "example.com".into(),
            port: 21,
            username: "user".into(),
            auth: Auth::Password("pw".into()),
            remote_dir: "/var/www/s".into(),
            url_template: "https://example.com/s/{filename}".into(),
        };
        assert_eq!(
            config.public_url("2026-07-09_14-03-22_kx3f9a.png"),
            "https://example.com/s/2026-07-09_14-03-22_kx3f9a.png"
        );
    }

    #[test]
    fn default_port_matches_protocol() {
        assert_eq!(UploadConfig::default_port(Protocol::Ftp), 21);
        assert_eq!(UploadConfig::default_port(Protocol::Ftps), 21);
        assert_eq!(UploadConfig::default_port(Protocol::Sftp), 22);
    }

    /// Connecting to a closed port on localhost must fail fast with
    /// `UploadError::Connect`, and must not hang: this exercises the
    /// connection-timeout path without needing a real server.
    #[test]
    fn upload_to_closed_port_returns_connect_error_within_bounded_time() {
        // Port 1 is reserved and essentially guaranteed to refuse connections
        // immediately (rather than requiring us to wait out a full timeout),
        // which keeps this test fast while still exercising the Connect
        // error path end-to-end.
        let config = UploadConfig {
            protocol: Protocol::Ftp,
            host: "127.0.0.1".into(),
            port: 1,
            username: "user".into(),
            auth: Auth::Password("pw".into()),
            remote_dir: "/".into(),
            url_template: "https://example.com/{filename}".into(),
        };

        let start = std::time::Instant::now();
        let result = upload(&config, "probe.png", b"hello");
        let elapsed = start.elapsed();

        assert!(matches!(result, Err(UploadError::Connect(_))));
        assert!(
            elapsed < Duration::from_secs(15),
            "upload() took too long to fail: {elapsed:?}"
        );
    }

    #[test]
    fn sftp_to_closed_port_returns_connect_error_within_bounded_time() {
        let config = UploadConfig {
            protocol: Protocol::Sftp,
            host: "127.0.0.1".into(),
            port: 1,
            username: "user".into(),
            auth: Auth::Password("pw".into()),
            remote_dir: "/".into(),
            url_template: "https://example.com/{filename}".into(),
        };

        let start = std::time::Instant::now();
        let result = upload(&config, "probe.png", b"hello");
        let elapsed = start.elapsed();

        assert!(matches!(result, Err(UploadError::Connect(_))));
        assert!(
            elapsed < Duration::from_secs(15),
            "upload() took too long to fail: {elapsed:?}"
        );
    }
}
