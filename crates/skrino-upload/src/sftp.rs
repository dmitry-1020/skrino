//! SFTP (SSH File Transfer Protocol) backend, built on `russh` + `russh-sftp`.
//!
//! The public API (`upload`/`test_connection` in `lib.rs`) is synchronous, so
//! each call here spins up a small single-threaded tokio runtime, does its
//! work, and tears the runtime down again. That's an implementation detail:
//! callers never touch tokio directly.

use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use russh::Disconnect;
use russh_sftp::client::SftpSession;
use tokio::io::AsyncWriteExt;

use crate::{Auth, UploadConfig, UploadError};

/// Cap on TCP connect + SSH handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on the whole upload/test-connection operation (connect, auth, mkdir,
/// transfer, cleanup).
const OP_TIMEOUT: Duration = Duration::from_secs(60);

/// Our `russh` client handler. It has no state of its own; its only job is
/// to answer the host-key check.
struct Client;

impl client::Handler for Client {
    type Error = anyhow::Error;

    /// Accepts any server host key.
    ///
    /// TODO(security): this means there is no known_hosts-style pinning of
    /// the SSH host identity — a network attacker able to intercept the
    /// first connection could impersonate the server undetected. This
    /// matches the equivalent TODO on the FTPS certificate verifier in
    /// `ftp.rs`. A future version should persist the host key fingerprint on
    /// first connect and verify it on subsequent connections (trust-on-first-
    /// use), similar to OpenSSH's `known_hosts`.
    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// A live SSH connection plus its SFTP subsystem, kept together so we can
/// clean both up when done.
struct Session {
    handle: Handle<Client>,
    sftp: SftpSession,
}

async fn connect(config: &UploadConfig) -> Result<Session, UploadError> {
    let ssh_config = Arc::new(client::Config::default());

    let mut handle = tokio::time::timeout(
        CONNECT_TIMEOUT,
        client::connect(ssh_config, (config.host.as_str(), config.port), Client),
    )
    .await
    .map_err(|_| {
        UploadError::Connect(format!(
            "connection to {}:{} timed out",
            config.host, config.port
        ))
    })?
    .map_err(|e| {
        UploadError::Connect(format!(
            "failed to connect to {}:{}: {e}",
            config.host, config.port
        ))
    })?;

    authenticate(&mut handle, config).await?;

    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| UploadError::Connect(format!("failed to open SSH channel: {e}")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| UploadError::Connect(format!("failed to start sftp subsystem: {e}")))?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| UploadError::Connect(format!("sftp handshake failed: {e}")))?;

    Ok(Session { handle, sftp })
}

async fn authenticate(handle: &mut Handle<Client>, config: &UploadConfig) -> Result<(), UploadError> {
    let result = match &config.auth {
        Auth::Password(password) => handle
            .authenticate_password(&config.username, password)
            .await
            .map_err(|e| UploadError::Auth(format!("authentication failed: {e}")))?,
        Auth::KeyFile { path, passphrase } => {
            let key = load_secret_key(path, passphrase.as_deref())
                .map_err(|e| UploadError::Auth(format!("failed to load private key '{path}': {e}")))?;
            // For RSA keys, ask the server which signature hash it prefers
            // (rsa-sha2-256/512 vs. the legacy ssh-rsa/SHA-1); irrelevant for
            // ed25519/ecdsa keys.
            let hash_alg = handle.best_supported_rsa_hash().await.ok().flatten().flatten();
            handle
                .authenticate_publickey(
                    &config.username,
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
                )
                .await
                .map_err(|e| UploadError::Auth(format!("authentication failed: {e}")))?
        }
    };

    if result.success() {
        Ok(())
    } else {
        Err(UploadError::Auth(
            "server rejected the provided credentials".into(),
        ))
    }
}

/// Creates `remote_dir` (and any missing parent segments), tolerating
/// "already exists" style failures.
async fn ensure_remote_dir(sftp: &SftpSession, remote_dir: &str) -> Result<(), UploadError> {
    let dir = remote_dir.trim_end_matches('/');
    if dir.is_empty() {
        return Ok(());
    }

    let mut current = String::new();
    if dir.starts_with('/') {
        current.push('/');
    }
    for segment in dir.split('/').filter(|s| !s.is_empty()) {
        if !current.is_empty() && !current.ends_with('/') {
            current.push('/');
        }
        current.push_str(segment);

        if let Err(e) = sftp.create_dir(current.clone()).await {
            // The SFTPv3 protocol has no dedicated "already exists" status,
            // servers just return a generic failure — so treat mkdir errors
            // as fatal only if the directory truly isn't there afterwards.
            let exists = sftp.try_exists(current.clone()).await.unwrap_or(false);
            if !exists {
                return Err(UploadError::Transfer(format!(
                    "failed to create remote directory '{current}': {e}"
                )));
            }
        }
    }
    Ok(())
}

fn join_remote_path(dir: &str, filename: &str) -> String {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        filename.to_string()
    } else {
        format!("{dir}/{filename}")
    }
}

async fn write_file(sftp: &SftpSession, path: &str, bytes: &[u8]) -> Result<(), UploadError> {
    let mut file = sftp
        .create(path)
        .await
        .map_err(|e| UploadError::Transfer(format!("failed to create remote file '{path}': {e}")))?;
    file.write_all(bytes)
        .await
        .map_err(|e| UploadError::Transfer(format!("failed to write remote file '{path}': {e}")))?;
    file.shutdown()
        .await
        .map_err(|e| UploadError::Transfer(format!("failed to finalize remote file '{path}': {e}")))?;
    Ok(())
}

async fn close(session: Session) {
    let _ = session.sftp.close().await;
    let _ = session
        .handle
        .disconnect(Disconnect::ByApplication, "", "English")
        .await;
}

fn run_blocking<F>(future: F) -> Result<(), UploadError>
where
    F: std::future::Future<Output = Result<(), UploadError>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| UploadError::Connect(format!("failed to start async runtime: {e}")))?;
    rt.block_on(async {
        match tokio::time::timeout(OP_TIMEOUT, future).await {
            Ok(result) => result,
            Err(_) => Err(UploadError::Transfer("operation timed out".into())),
        }
    })
}

pub(crate) fn upload(config: &UploadConfig, filename: &str, bytes: &[u8]) -> Result<(), UploadError> {
    run_blocking(async move {
        let session = connect(config).await?;
        ensure_remote_dir(&session.sftp, &config.remote_dir).await?;
        let path = join_remote_path(&config.remote_dir, filename);
        let result = write_file(&session.sftp, &path, bytes).await;
        close(session).await;
        result
    })
}

pub(crate) fn test_connection(
    config: &UploadConfig,
    probe_name: &str,
    probe_bytes: &[u8],
) -> Result<(), UploadError> {
    run_blocking(async move {
        let session = connect(config).await?;
        ensure_remote_dir(&session.sftp, &config.remote_dir).await?;
        let path = join_remote_path(&config.remote_dir, probe_name);
        let write_result = write_file(&session.sftp, &path, probe_bytes).await;
        let remove_result = if write_result.is_ok() {
            session.sftp.remove_file(&path).await.map_err(|e| {
                UploadError::Transfer(format!("failed to remove probe file '{path}': {e}"))
            })
        } else {
            Ok(())
        };
        close(session).await;
        write_result.and(remove_result)
    })
}
