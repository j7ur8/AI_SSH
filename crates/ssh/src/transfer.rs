//! Staged file transfer primitives.
//!
//! Every write lands at a staged path inside the destination directory and is
//! moved into place only after its own bytes have been hashed and, when a
//! [`RemoteHash`] is available, confirmed against the remote copy. A truncated
//! or mangled transfer therefore cannot appear at the destination path, which
//! is the failure mode that a hand-rolled heredoc transfer cannot rule out.

use async_trait::async_trait;
use russh_sftp::{
    client::{SftpSession, error::Error as SftpError, fs::Metadata},
    protocol::{FileAttributes, StatusCode},
};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const TRANSFER_CHUNK_BYTES: usize = 64 * 1024;

static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Confirms that a staged remote file matches the local digest.
///
/// Implemented by the session layer over an exec channel, because SFTP itself
/// has no hashing operation. Returning `None` means the remote host has no
/// usable hashing tool; the transfer proceeds but is reported as unverified.
#[async_trait]
pub trait RemoteHash: Send + Sync {
    async fn sha256(&self, remote_path: &str) -> Option<String>;
}

#[derive(Debug, Clone)]
pub struct TransferOutcome {
    pub bytes: u64,
    pub sha256: String,
    pub verified: bool,
    pub changed: bool,
}

pub fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Streams a local file through SHA-256 without holding it in memory.
pub async fn hash_local_file(path: &Path) -> std::io::Result<(u64, String)> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; TRANSFER_CHUNK_BYTES];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

/// A sibling of `remote` so the final rename stays within one directory and
/// cannot fail by crossing a filesystem boundary.
pub fn staged_path(remote: &str) -> String {
    let trimmed = remote.trim_end_matches('/');
    let (dir, name) = match trimmed.rfind('/') {
        Some(0) => ("/", &trimmed[1..]),
        Some(index) => (&trimmed[..index], &trimmed[index + 1..]),
        None => ("", trimmed),
    };
    let unique = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default(),
        STAGE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let staged = format!("{name}.aissh-tmp-{unique}");
    match dir {
        "" => staged,
        "/" => format!("/{staged}"),
        other => format!("{other}/{staged}"),
    }
}

pub fn parent_of(remote: &str) -> Option<String> {
    let trimmed = remote.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => Some("/".into()),
        Some(index) => Some(trimmed[..index].to_owned()),
        None => None,
    }
}

/// Maps an SFTP failure onto the daemon's stable error-code vocabulary.
pub fn sftp_error(error: SftpError, path: &str) -> anyhow::Error {
    let code = match &error {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => "FILE_NOT_FOUND",
            StatusCode::PermissionDenied => "PERMISSION_DENIED",
            StatusCode::Failure | StatusCode::OpUnsupported => "REMOTE_IO_ERROR",
            _ => "REMOTE_IO_ERROR",
        },
        SftpError::Timeout => "REMOTE_TIMEOUT",
        _ => "REMOTE_IO_ERROR",
    };
    anyhow::anyhow!("{code}: {path}: {error}")
}

pub fn local_error(error: std::io::Error, path: &Path) -> anyhow::Error {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => "FILE_NOT_FOUND",
        std::io::ErrorKind::PermissionDenied => "PERMISSION_DENIED",
        std::io::ErrorKind::AlreadyExists => "FILE_EXISTS",
        _ => "LOCAL_IO_ERROR",
    };
    anyhow::anyhow!("{code}: {}: {error}", path.display())
}

pub async fn stat(sftp: &SftpSession, path: &str) -> anyhow::Result<Metadata> {
    sftp.metadata(path)
        .await
        .map_err(|error| sftp_error(error, path))
}

pub async fn try_stat(sftp: &SftpSession, path: &str) -> anyhow::Result<Option<Metadata>> {
    match sftp.metadata(path).await {
        Ok(metadata) => Ok(Some(metadata)),
        Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => Ok(None),
        Err(error) => Err(sftp_error(error, path)),
    }
}

/// Reads at most `max_bytes`, reporting the full size so the caller can tell a
/// small file from a truncated read.
pub async fn read_capped(
    sftp: &SftpSession,
    path: &str,
    max_bytes: usize,
) -> anyhow::Result<(Vec<u8>, u64, bool)> {
    let file = sftp
        .open(path)
        .await
        .map_err(|error| sftp_error(error, path))?;
    let size = file.metadata().await.map(|value| value.len()).unwrap_or(0);
    let mut limited = file.take(max_bytes as u64);
    let mut data = Vec::new();
    limited
        .read_to_end(&mut data)
        .await
        .map_err(|error| sftp_error(error.into(), path))?;
    let truncated = size > data.len() as u64;
    Ok((data, size, truncated))
}

async fn apply_mode(sftp: &SftpSession, path: &str, mode: Option<u32>) -> anyhow::Result<()> {
    let Some(mode) = mode else {
        return Ok(());
    };
    sftp.set_metadata(
        path,
        FileAttributes {
            permissions: Some(mode),
            ..Default::default()
        },
    )
    .await
    .map_err(|error| sftp_error(error, path))
}

/// Moves a verified staged file onto its destination.
///
/// SFTP v3 has no atomic overwrite, so an existing destination is removed
/// first. The content is already complete and verified at this point; only the
/// final swap is non-atomic.
async fn promote(sftp: &SftpSession, staged: &str, remote: &str) -> anyhow::Result<()> {
    match try_stat(sftp, remote).await? {
        Some(_) => {
            sftp.remove_file(remote)
                .await
                .map_err(|error| sftp_error(error, remote))?;
        }
        None => {}
    }
    sftp.rename(staged, remote)
        .await
        .map_err(|error| sftp_error(error, remote))
}

async fn discard(sftp: &SftpSession, staged: &str) {
    let _ = sftp.remove_file(staged).await;
}

/// Streams `data` to a staged path, verifies it, and promotes it.
pub async fn write_staged(
    sftp: &SftpSession,
    remote: &str,
    data: &[u8],
    mode: Option<u32>,
    verify: Option<&dyn RemoteHash>,
) -> anyhow::Result<TransferOutcome> {
    let staged = staged_path(remote);
    let mut file = sftp
        .create(staged.as_str())
        .await
        .map_err(|error| sftp_error(error, &staged))?;
    if let Err(error) = file.write_all(data).await {
        discard(sftp, &staged).await;
        return Err(sftp_error(error.into(), &staged));
    }
    if let Err(error) = file.flush().await {
        discard(sftp, &staged).await;
        return Err(sftp_error(error.into(), &staged));
    }
    if let Err(error) = file.close().await {
        discard(sftp, &staged).await;
        return Err(sftp_error(error.into(), &staged));
    }
    let digest = hex_digest(data);
    let verified = confirm(sftp, &staged, &digest, verify).await?;
    if let Err(error) = apply_mode(sftp, &staged, mode).await {
        discard(sftp, &staged).await;
        return Err(error);
    }
    promote(sftp, &staged, remote).await?;
    Ok(TransferOutcome {
        bytes: data.len() as u64,
        sha256: digest,
        verified,
        changed: true,
    })
}

/// Streams a local file to a staged remote path, hashing on the way out.
pub async fn upload_staged(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    mode: Option<u32>,
    verify: Option<&dyn RemoteHash>,
) -> anyhow::Result<TransferOutcome> {
    let staged = staged_path(remote);
    let mut source = tokio::fs::File::open(local)
        .await
        .map_err(|error| local_error(error, local))?;
    let mut file = sftp
        .create(staged.as_str())
        .await
        .map_err(|error| sftp_error(error, &staged))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; TRANSFER_CHUNK_BYTES];
    let mut total = 0u64;
    loop {
        let read = match source.read(&mut buffer).await {
            Ok(read) => read,
            Err(error) => {
                discard(sftp, &staged).await;
                return Err(local_error(error, local));
            }
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        if let Err(error) = file.write_all(&buffer[..read]).await {
            discard(sftp, &staged).await;
            return Err(sftp_error(error.into(), &staged));
        }
        total += read as u64;
    }
    if let Err(error) = file.flush().await {
        discard(sftp, &staged).await;
        return Err(sftp_error(error.into(), &staged));
    }
    if let Err(error) = file.close().await {
        discard(sftp, &staged).await;
        return Err(sftp_error(error.into(), &staged));
    }
    let digest = format!("{:x}", hasher.finalize());
    let verified = confirm(sftp, &staged, &digest, verify).await?;
    if let Err(error) = apply_mode(sftp, &staged, mode).await {
        discard(sftp, &staged).await;
        return Err(error);
    }
    promote(sftp, &staged, remote).await?;
    Ok(TransferOutcome {
        bytes: total,
        sha256: digest,
        verified,
        changed: true,
    })
}

/// Streams a remote file into a staged local file, then promotes it.
pub async fn download_staged(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    verify: Option<&dyn RemoteHash>,
) -> anyhow::Result<TransferOutcome> {
    let mut file = sftp
        .open(remote)
        .await
        .map_err(|error| sftp_error(error, remote))?;
    let staged = local_staged_path(local);
    let mut sink = tokio::fs::File::create(&staged)
        .await
        .map_err(|error| local_error(error, &staged))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; TRANSFER_CHUNK_BYTES];
    let mut total = 0u64;
    loop {
        let read = match file.read(&mut buffer).await {
            Ok(read) => read,
            Err(error) => {
                let _ = tokio::fs::remove_file(&staged).await;
                return Err(sftp_error(error.into(), remote));
            }
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        if let Err(error) = sink.write_all(&buffer[..read]).await {
            let _ = tokio::fs::remove_file(&staged).await;
            return Err(local_error(error, &staged));
        }
        total += read as u64;
    }
    if let Err(error) = sink.flush().await {
        let _ = tokio::fs::remove_file(&staged).await;
        return Err(local_error(error, &staged));
    }
    drop(sink);
    let digest = format!("{:x}", hasher.finalize());
    let verified = match verify {
        Some(hasher) => match hasher.sha256(remote).await {
            Some(remote_digest) if remote_digest != digest => {
                let _ = tokio::fs::remove_file(&staged).await;
                return Err(anyhow::anyhow!(
                    "TRANSFER_VERIFY_FAILED: {remote} downloaded {digest} but the remote host reports {remote_digest}"
                ));
            }
            Some(_) => true,
            None => false,
        },
        None => false,
    };
    tokio::fs::rename(&staged, local)
        .await
        .map_err(|error| local_error(error, local))?;
    Ok(TransferOutcome {
        bytes: total,
        sha256: digest,
        verified,
        changed: true,
    })
}

/// Local counterpart of [`staged_path`], used so a failed download never leaves
/// a half-written file at the destination.
pub fn local_staged_path(local: &Path) -> std::path::PathBuf {
    let name = local
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    let staged = format!("{name}.aissh-tmp-{}", std::process::id());
    match local.parent() {
        Some(parent) => parent.join(staged),
        None => std::path::PathBuf::from(staged),
    }
}

/// Compares a staged file's digest with the remote copy. Missing hashing tools
/// downgrade the outcome to "unverified" rather than failing the transfer.
async fn confirm(
    sftp: &SftpSession,
    staged: &str,
    expected: &str,
    verify: Option<&dyn RemoteHash>,
) -> anyhow::Result<bool> {
    let Some(hasher) = verify else {
        return Ok(false);
    };
    match hasher.sha256(staged).await {
        Some(actual) if actual == expected => Ok(true),
        Some(actual) => {
            discard(sftp, staged).await;
            Err(anyhow::anyhow!(
                "TRANSFER_VERIFY_FAILED: staged bytes hash to {expected} but the remote host reports {actual}"
            ))
        }
        None => Ok(false),
    }
}

/// Creates a directory, optionally including missing parents.
pub async fn make_dir(
    sftp: &SftpSession,
    path: &str,
    parents: bool,
    mode: Option<u32>,
) -> anyhow::Result<()> {
    if try_stat(sftp, path).await?.is_some() {
        return Ok(());
    }
    if parents && let Some(parent) = parent_of(path) {
        if parent != "/" && !parent.is_empty() {
            Box::pin(make_dir(sftp, &parent, true, None)).await?;
        }
    }
    match sftp.create_dir(path).await {
        Ok(()) => {}
        // A concurrent creation is not a failure.
        Err(error) => {
            if try_stat(sftp, path).await?.is_none() {
                return Err(sftp_error(error, path));
            }
        }
    }
    apply_mode(sftp, path, mode).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_beside_the_destination() {
        assert_eq!(
            parent_of("/srv/app/src.tgz").as_deref(),
            Some("/srv/app"),
            "a staged file must share the destination directory so the rename cannot cross filesystems"
        );
        let staged = staged_path("/srv/app/src.tgz");
        assert!(staged.starts_with("/srv/app/src.tgz.aissh-tmp-"));
        assert_eq!(parent_of(&staged).as_deref(), Some("/srv/app"));
    }

    #[test]
    fn stages_relative_and_root_paths() {
        assert!(staged_path("src.tgz").starts_with("src.tgz.aissh-tmp-"));
        assert!(staged_path("/src.tgz").starts_with("/src.tgz.aissh-tmp-"));
        assert_eq!(parent_of("src.tgz"), None);
        assert_eq!(parent_of("/src.tgz").as_deref(), Some("/"));
    }

    #[test]
    fn staged_paths_are_unique_per_call() {
        let first = staged_path("/tmp/x");
        let second = staged_path("/tmp/x");
        assert_ne!(first, second);
    }

    #[test]
    fn local_staged_path_stays_beside_the_target() {
        let staged = local_staged_path(Path::new("/tmp/out/jobs.sqlite3"));
        assert_eq!(
            parent_of(&staged.to_string_lossy()).as_deref(),
            Some("/tmp/out")
        );
        assert_ne!(staged, std::path::PathBuf::from("/tmp/out/jobs.sqlite3"));
    }

    #[test]
    fn maps_missing_files_to_a_stable_code() {
        let error = sftp_error(
            SftpError::Status(russh_sftp::protocol::Status {
                id: 0,
                status_code: StatusCode::NoSuchFile,
                error_message: "gone".into(),
                language_tag: "en".into(),
            }),
            "/tmp/gone",
        );
        assert!(error.to_string().starts_with("FILE_NOT_FOUND: "));
    }

    #[test]
    fn maps_denied_writes_to_a_stable_code() {
        let error = sftp_error(
            SftpError::Status(russh_sftp::protocol::Status {
                id: 0,
                status_code: StatusCode::PermissionDenied,
                error_message: "denied".into(),
                language_tag: "en".into(),
            }),
            "/root/secret",
        );
        assert!(error.to_string().starts_with("PERMISSION_DENIED: "));
    }

    #[test]
    fn maps_local_existing_files_to_file_exists() {
        let error = local_error(
            std::io::Error::new(std::io::ErrorKind::AlreadyExists, "present"),
            Path::new("/tmp/out"),
        );
        assert!(error.to_string().starts_with("FILE_EXISTS: "));
    }

    #[tokio::test]
    async fn hashes_a_local_file_without_buffering_it() {
        let path = std::env::temp_dir().join(format!(
            "aissh-transfer-hash-{}",
            STAGE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::write(&path, b"abc").await.unwrap();
        let (bytes, digest) = hash_local_file(&path).await.unwrap();
        assert_eq!(bytes, 3);
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = tokio::fs::remove_file(&path).await;
    }
}
