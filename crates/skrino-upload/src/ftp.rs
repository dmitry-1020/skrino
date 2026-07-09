//! FTP / FTPS (explicit TLS) backend, built on `suppaftp`.
//!
//! `suppaftp`'s sync client is generic over the transport stream
//! (`ImplFtpStream<T>`), but the `T: TlsStream` bound is a crate-private
//! trait, so we can't write one generic function over both the plain
//! (`FtpStream`) and TLS (`RustlsFtpStream`) instantiations from outside the
//! crate. The `ftp_impl!` macro below expands the same logic twice — once
//! per concrete stream type — instead of hand-duplicating it.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use suppaftp::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use suppaftp::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use suppaftp::rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use suppaftp::{RustlsConnector, RustlsFtpStream};

use crate::{Auth, Protocol, UploadConfig, UploadError};

/// Cap on establishing the TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap applied to the socket itself (reads/writes) so a stalled login or
/// transfer can't hang forever; `suppaftp`'s sync API has no per-call
/// deadline of its own, so this is the closest equivalent to the "~60s
/// overall operation" budget for this blocking backend.
const OP_TIMEOUT: Duration = Duration::from_secs(60);

fn resolve_addr(host: &str, port: u16) -> Result<SocketAddr, UploadError> {
    (host, port)
        .to_socket_addrs()
        .map_err(|e| UploadError::Connect(format!("failed to resolve host '{host}': {e}")))?
        .next()
        .ok_or_else(|| UploadError::Connect(format!("no addresses found for host '{host}'")))
}

/// Accepts any TLS certificate the server presents.
///
/// TODO(security): this is intentionally permissive — skrino talks to a
/// server the user configured themselves, which is very often a self-signed
/// or otherwise unverifiable certificate, and there is no UI (yet) for
/// pinning/importing a CA. A future version should pin the certificate
/// fingerprint on first connect (trust-on-first-use) instead of disabling
/// verification outright, mirroring the TODO on the SFTP host-key check in
/// `sftp.rs`.
#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, suppaftp::rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, suppaftp::rustls::Error> {
        suppaftp::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &suppaftp::rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, suppaftp::rustls::Error> {
        suppaftp::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &suppaftp::rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        suppaftp::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn build_tls_config() -> Arc<ClientConfig> {
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    Arc::new(config)
}

/// Expands, once per concrete stream type, the connect/login/mkdir/put/rm
/// logic shared by FTP and FTPS. `$connect` is an expression of type
/// `fn(SocketAddr, &UploadConfig) -> Result<$stream_ty, UploadError>` that
/// performs whatever is specific to that transport (plain TCP vs. TCP+TLS).
macro_rules! ftp_impl {
    ($module:ident, $stream_ty:ty, $connect:expr) => {
        mod $module {
            use super::*;

            fn connect_and_login(config: &UploadConfig) -> Result<$stream_ty, UploadError> {
                let addr = resolve_addr(&config.host, config.port)?;
                let mut stream: $stream_ty = ($connect)(addr, config)?;
                stream
                    .get_ref()
                    .set_read_timeout(Some(OP_TIMEOUT))
                    .map_err(|e| UploadError::Connect(format!("failed to configure socket timeout: {e}")))?;
                stream
                    .get_ref()
                    .set_write_timeout(Some(OP_TIMEOUT))
                    .map_err(|e| UploadError::Connect(format!("failed to configure socket timeout: {e}")))?;
                login(&mut stream, config)?;
                Ok(stream)
            }

            fn login(stream: &mut $stream_ty, config: &UploadConfig) -> Result<(), UploadError> {
                let password = match &config.auth {
                    Auth::Password(p) => p.as_str(),
                    Auth::KeyFile { .. } => {
                        return Err(UploadError::Auth(
                            "key file authentication is only supported for SFTP".into(),
                        ));
                    }
                };
                stream.login(config.username.as_str(), password).map_err(|e| {
                    UploadError::Auth(format!(
                        "login failed for user '{}': {e}",
                        config.username
                    ))
                })
            }

            /// `cd`s into `remote_dir`, creating any missing path segments
            /// along the way (segment-by-segment `mkdir` + `cwd`, ignoring
            /// "already exists" style failures).
            fn ensure_remote_dir(stream: &mut $stream_ty, remote_dir: &str) -> Result<(), UploadError> {
                let dir = remote_dir.trim_matches('/');
                if dir.is_empty() {
                    return Ok(());
                }
                if dir.starts_with('/') || remote_dir.starts_with('/') {
                    let _ = stream.cwd("/");
                }
                for segment in dir.split('/').filter(|s| !s.is_empty()) {
                    if stream.cwd(segment).is_err() {
                        // mkdir failing here (e.g. it already exists but we
                        // couldn't cwd into it for another reason) will
                        // surface as a clear error on the following cwd.
                        let _ = stream.mkdir(segment);
                        stream.cwd(segment).map_err(|e| {
                            UploadError::Transfer(format!(
                                "failed to create/enter remote directory '{segment}': {e}"
                            ))
                        })?;
                    }
                }
                Ok(())
            }

            fn put(stream: &mut $stream_ty, filename: &str, bytes: &[u8]) -> Result<(), UploadError> {
                stream
                    .transfer_type(suppaftp::types::FileType::Binary)
                    .map_err(|e| UploadError::Transfer(format!("failed to set binary transfer mode: {e}")))?;
                let mut cursor = std::io::Cursor::new(bytes);
                stream
                    .put_file(filename, &mut cursor)
                    .map_err(|e| UploadError::Transfer(format!("failed to upload '{filename}': {e}")))?;
                Ok(())
            }

            fn remove(stream: &mut $stream_ty, filename: &str) -> Result<(), UploadError> {
                stream
                    .rm(filename)
                    .map_err(|e| UploadError::Transfer(format!("failed to remove '{filename}': {e}")))
            }

            pub(crate) fn upload(
                config: &UploadConfig,
                filename: &str,
                bytes: &[u8],
            ) -> Result<(), UploadError> {
                let mut stream = connect_and_login(config)?;
                ensure_remote_dir(&mut stream, &config.remote_dir)?;
                let result = put(&mut stream, filename, bytes);
                let _ = stream.quit();
                result
            }

            pub(crate) fn test_connection(
                config: &UploadConfig,
                probe_name: &str,
                probe_bytes: &[u8],
            ) -> Result<(), UploadError> {
                let mut stream = connect_and_login(config)?;
                ensure_remote_dir(&mut stream, &config.remote_dir)?;
                let put_result = put(&mut stream, probe_name, probe_bytes);
                let remove_result = if put_result.is_ok() {
                    remove(&mut stream, probe_name)
                } else {
                    Ok(())
                };
                let _ = stream.quit();
                put_result.and(remove_result)
            }
        }
    };
}

ftp_impl!(
    plain,
    suppaftp::FtpStream,
    (|addr: SocketAddr, config: &UploadConfig| -> Result<suppaftp::FtpStream, UploadError> {
        suppaftp::FtpStream::connect_timeout(addr, CONNECT_TIMEOUT).map_err(|e| {
            UploadError::Connect(format!(
                "failed to connect to {}:{}: {e}",
                config.host, config.port
            ))
        })
    })
);

ftp_impl!(
    secure,
    RustlsFtpStream,
    (|addr: SocketAddr, config: &UploadConfig| -> Result<RustlsFtpStream, UploadError> {
        let stream = RustlsFtpStream::connect_timeout(addr, CONNECT_TIMEOUT).map_err(|e| {
            UploadError::Connect(format!(
                "failed to connect to {}:{}: {e}",
                config.host, config.port
            ))
        })?;
        let tls_config = build_tls_config();
        stream
            .into_secure(RustlsConnector::from(tls_config), &config.host)
            .map_err(|e| {
                UploadError::Connect(format!("TLS handshake with {} failed: {e}", config.host))
            })
    })
);

pub(crate) fn upload(config: &UploadConfig, filename: &str, bytes: &[u8]) -> Result<(), UploadError> {
    match config.protocol {
        Protocol::Ftp => plain::upload(config, filename, bytes),
        Protocol::Ftps => secure::upload(config, filename, bytes),
        Protocol::Sftp => unreachable!("ftp module only handles Ftp/Ftps"),
    }
}

pub(crate) fn test_connection(
    config: &UploadConfig,
    probe_name: &str,
    probe_bytes: &[u8],
) -> Result<(), UploadError> {
    match config.protocol {
        Protocol::Ftp => plain::test_connection(config, probe_name, probe_bytes),
        Protocol::Ftps => secure::test_connection(config, probe_name, probe_bytes),
        Protocol::Sftp => unreachable!("ftp module only handles Ftp/Ftps"),
    }
}
