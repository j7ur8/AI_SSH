//! Live SFTP integration tests.
//!
//! These talk to a real SSH server because the interesting behavior — the SFTP
//! subsystem handshake, staging a file, verifying it against a remote digest,
//! and only then moving it into place — cannot be exercised against a stub.
//!
//! They are `#[ignore]`d so `cargo test` stays hermetic, and are driven by:
//!
//! ```sh
//! AISSH_TEST_SSH_PORT=2222 AISSH_TEST_SSH_KEY=/path/to/id \
//!   cargo test -p aissh-ssh --test sftp_live -- --ignored --test-threads=1
//! ```
//!
//! `AISSH_TEST_SSH_HOST` (default `127.0.0.1`) and `AISSH_TEST_SSH_USER`
//! (default `root`) are optional.

use aissh_config::{Auth, Paths, Target};
use aissh_ssh::{
    RemoteHash, SftpSession, SshConnection, TransferOutcome, download_staged, hex_digest, make_dir,
    read_capped, stat, try_stat, upload_staged, write_staged,
};
use async_trait::async_trait;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Hashes a remote file the way the session layer does: over an exec channel,
/// using the remote host's own tool.
struct ExecHash {
    connection: Arc<SshConnection>,
    /// When set, report this digest instead of the real one, so the verification
    /// path itself can be exercised.
    override_with: Option<String>,
}

#[async_trait]
impl RemoteHash for ExecHash {
    async fn sha256(&self, remote_path: &str) -> Option<String> {
        if let Some(value) = &self.override_with {
            return Some(value.clone());
        }
        let command = format!(
            "if command -v sha256sum >/dev/null 2>&1; then sha256sum -- '{remote_path}'; \
             elif command -v shasum >/dev/null 2>&1; then shasum -a 256 -- '{remote_path}'; \
             else exit 3; fi"
        );
        let mut channel = self.connection.open_exec(&command).await.ok()?;
        let (stdout, _, exit, _) = channel.collect(64 * 1024).await.ok()?;
        if exit != Some(0) {
            return None;
        }
        String::from_utf8(stdout)
            .ok()?
            .split_whitespace()
            .next()
            .filter(|digest| digest.len() == 64)
            .map(str::to_owned)
    }
}

fn required_env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| {
        panic!("{key} must be set to run the live SFTP tests; see the module docs")
    })
}

async fn connect() -> (Arc<SshConnection>, Paths) {
    let paths = Paths::under(std::env::temp_dir().join("aissh-sftp-live"));
    let target = Target {
        id: "live".into(),
        name: "Live".into(),
        host: std::env::var("AISSH_TEST_SSH_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: required_env("AISSH_TEST_SSH_PORT").parse().expect("port"),
        username: std::env::var("AISSH_TEST_SSH_USER").unwrap_or_else(|_| "root".into()),
        auth: Auth::PrivateKey {
            path: PathBuf::from(required_env("AISSH_TEST_SSH_KEY")),
            passphrase: None,
        },
    };
    let connection = SshConnection::connect(
        &target,
        &paths,
        Duration::from_secs(15),
        Duration::from_secs(30),
    )
    .await
    .expect("connect to the test server");
    (Arc::new(connection), paths)
}

fn workdir(name: &str) -> String {
    format!(
        "/tmp/aissh-live-{name}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

async fn openssh_connection() -> (Arc<SshConnection>, SftpSession, Paths) {
    let (connection, paths) = connect().await;
    let sftp = connection
        .open_sftp()
        .await
        .expect("the SFTP subsystem handshake must succeed against OpenSSH");
    (connection, sftp, paths)
}

/// No staged leftovers in a directory means every write was promoted or cleaned up.
async fn assert_no_staged_leftovers(sftp: &SftpSession, dir: &str) {
    let entries = sftp
        .read_dir(dir)
        .await
        .expect("list the working directory");
    let leftovers = entries
        .map(|entry| entry.file_name())
        .filter(|name| name.contains(".aissh-tmp-"))
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "staged files must never be left behind: {leftovers:?}"
    );
}

async fn cleanup(sftp: &SftpSession, dir: &str) {
    if let Ok(entries) = sftp.read_dir(dir).await {
        for entry in entries.collect::<Vec<_>>() {
            let path = format!("{dir}/{}", entry.file_name());
            let _ = sftp.remove_file(&path).await;
        }
    }
    let _ = sftp.remove_dir(dir).await;
}

#[tokio::test]
#[ignore = "requires a live SSH server; see the module docs"]
async fn opens_the_sftp_subsystem_and_reports_metadata() {
    let (_, sftp, _) = openssh_connection().await;
    let dir = workdir("stat");
    make_dir(&sftp, &dir, true, None).await.expect("mkdir -p");

    let file = format!("{dir}/notes.txt");
    write_staged(&sftp, &file, b"hello\n", Some(0o600), None)
        .await
        .expect("write");

    let metadata = stat(&sftp, &file).await.expect("stat");
    assert!(metadata.is_regular(), "the file must be a regular file");
    assert_eq!(metadata.len(), 6);
    assert_eq!(
        metadata.permissions.map(|mode| mode & 0o777),
        Some(0o600),
        "the requested mode must be applied"
    );

    assert!(
        try_stat(&sftp, &format!("{dir}/missing"))
            .await
            .expect("lstat a missing path")
            .is_none(),
        "a missing path reports None rather than an error"
    );

    cleanup(&sftp, &dir).await;
}

#[tokio::test]
#[ignore = "requires a live SSH server; see the module docs"]
async fn writes_replace_existing_content_without_leaving_a_staged_file() {
    let (_, sftp, _) = openssh_connection().await;
    let dir = workdir("overwrite");
    make_dir(&sftp, &dir, true, None).await.unwrap();
    let file = format!("{dir}/config.yaml");

    write_staged(&sftp, &file, b"first version\n", None, None)
        .await
        .expect("initial write");
    // SFTP v3 has no atomic overwrite, so this exercises the remove-then-rename
    // path that a second deployment actually hits.
    write_staged(&sftp, &file, b"second version\n", None, None)
        .await
        .expect("overwrite");

    let (data, size, truncated) = read_capped(&sftp, &file, 64 * 1024).await.unwrap();
    assert_eq!(data, b"second version\n");
    assert_eq!(size, 15);
    assert!(!truncated);
    assert_no_staged_leftovers(&sftp, &dir).await;

    cleanup(&sftp, &dir).await;
}

#[tokio::test]
#[ignore = "requires a live SSH server; see the module docs"]
async fn detects_a_truncated_read() {
    let (_, sftp, _) = openssh_connection().await;
    let dir = workdir("truncate");
    make_dir(&sftp, &dir, true, None).await.unwrap();
    let file = format!("{dir}/big.log");
    write_staged(&sftp, &file, &vec![b'z'; 4096], None, None)
        .await
        .unwrap();

    let (data, size, truncated) = read_capped(&sftp, &file, 100).await.unwrap();
    assert_eq!(data.len(), 100);
    assert_eq!(
        size, 4096,
        "the full size is reported, not just what was read"
    );
    assert!(truncated, "a capped read must say so");

    cleanup(&sftp, &dir).await;
}

#[tokio::test]
#[ignore = "requires a live SSH server; see the module docs"]
async fn uploads_and_downloads_byte_for_byte_with_remote_verification() {
    let (connection, sftp, paths) = openssh_connection().await;
    let dir = workdir("transfer");
    make_dir(&sftp, &dir, true, None).await.unwrap();
    let hasher = ExecHash {
        connection: Arc::clone(&connection),
        override_with: None,
    };

    // Content with bytes that a heredoc or a shell quote would mangle.
    let mut payload = Vec::new();
    for index in 0..64u32 {
        payload.extend_from_slice(
            format!("line {index} 'quoted' \"double\" $VAR `tick` \\\n").as_bytes(),
        );
    }
    payload.extend_from_slice(&[0x00, 0xff, 0xfe, 0x0a, 0xe4, 0xb8, 0xad]);
    let local = paths.root.join("payload.bin");
    std::fs::create_dir_all(&paths.root).unwrap();
    std::fs::write(&local, &payload).unwrap();
    let expected = hex_digest(&payload);

    let remote = format!("{dir}/payload.bin");
    let outcome = upload_staged(&sftp, &local, &remote, Some(0o644), Some(&hasher))
        .await
        .expect("upload");
    assert_eq!(outcome.bytes, payload.len() as u64);
    assert_eq!(outcome.sha256, expected);
    assert!(
        outcome.verified,
        "the remote host reports a digest, so the upload must be verified"
    );

    let (remote_bytes, _, _) = read_capped(&sftp, &remote, 1024 * 1024).await.unwrap();
    assert_eq!(
        hex_digest(&remote_bytes),
        expected,
        "the bytes that landed must match the local file exactly"
    );

    let back = paths.root.join("payload.back.bin");
    let outcome = download_staged(&sftp, &remote, &back, Some(&hasher))
        .await
        .expect("download");
    assert!(outcome.verified);
    assert_eq!(std::fs::read(&back).unwrap(), payload);
    assert_no_staged_leftovers(&sftp, &dir).await;
    assert!(
        !std::fs::read_dir(&paths.root)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains(".aissh-tmp-")),
        "a completed download must not leave a local staged file"
    );

    cleanup(&sftp, &dir).await;
}

#[tokio::test]
#[ignore = "requires a live SSH server; see the module docs"]
async fn a_digest_mismatch_leaves_the_destination_untouched() {
    let (connection, sftp, _) = openssh_connection().await;
    let dir = workdir("verify-fail");
    make_dir(&sftp, &dir, true, None).await.unwrap();
    let file = format!("{dir}/release.tgz");

    write_staged(&sftp, &file, b"the good version\n", None, None)
        .await
        .unwrap();

    // A hasher that always disagrees stands in for a corrupted transfer.
    let liar = ExecHash {
        connection: Arc::clone(&connection),
        override_with: Some("0".repeat(64)),
    };
    let error = write_staged(&sftp, &file, b"the bad version\n", None, Some(&liar))
        .await
        .expect_err("a digest mismatch must fail the write");
    assert!(
        error.to_string().starts_with("TRANSFER_VERIFY_FAILED"),
        "the failure must be reported as a verification failure, got: {error}"
    );

    let (data, _, _) = read_capped(&sftp, &file, 4096).await.unwrap();
    assert_eq!(
        data, b"the good version\n",
        "a failed verification must not reach the destination path"
    );
    assert_no_staged_leftovers(&sftp, &dir).await;

    cleanup(&sftp, &dir).await;
}

#[tokio::test]
#[ignore = "requires a live SSH server; see the module docs"]
async fn reports_a_missing_subsystem_path_as_file_not_found() {
    let (_, sftp, _) = openssh_connection().await;
    let error = stat(&sftp, "/definitely/not/here")
        .await
        .expect_err("stat of a missing path");
    assert!(
        error.to_string().starts_with("FILE_NOT_FOUND"),
        "stable error codes must survive the transfer layer, got: {error}"
    );
}

#[tokio::test]
#[ignore = "requires a live SSH server; see the module docs"]
async fn a_failed_upload_leaves_no_partial_file_at_the_destination() {
    let (connection, sftp, paths) = openssh_connection().await;
    let dir = workdir("partial");
    make_dir(&sftp, &dir, true, None).await.unwrap();
    let target = format!("{dir}/big.bin");

    // Write a good file first so there is something to preserve.
    write_staged(&sftp, &target, b"original\n", None, None)
        .await
        .unwrap();

    // A local file that disappears mid-flight is one way a transfer dies.
    let missing = paths.root.join("does-not-exist.bin");
    let hasher = ExecHash {
        connection: Arc::clone(&connection),
        override_with: None,
    };
    let error = upload_staged(&sftp, &missing, &target, None, Some(&hasher))
        .await
        .expect_err("uploading a missing local file must fail");
    assert!(error.to_string().starts_with("FILE_NOT_FOUND"), "{error}");

    let (data, _, _) = read_capped(&sftp, &target, 4096).await.unwrap();
    assert_eq!(data, b"original\n", "the destination must be preserved");
    assert_no_staged_leftovers(&sftp, &dir).await;

    cleanup(&sftp, &dir).await;
}

#[tokio::test]
#[ignore = "requires a live SSH server; see the module docs"]
async fn a_staged_file_survives_as_a_visible_artifact_only_until_promotion() {
    // Guards the invariant the whole design rests on: the destination never
    // sees a partial file, and a failed transfer cleans its stage up.
    let (_, sftp, _) = openssh_connection().await;
    let dir = workdir("invariant");
    make_dir(&sftp, &dir, true, None).await.unwrap();
    let file = format!("{dir}/app.py");

    let outcome: TransferOutcome = write_staged(&sftp, &file, b"print('hi')\n", None, None)
        .await
        .unwrap();
    assert!(outcome.bytes > 0);

    let names = sftp
        .read_dir(&dir)
        .await
        .unwrap()
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["app.py".to_string()],
        "only the final name exists once a write returns"
    );

    cleanup(&sftp, &dir).await;
}

#[tokio::test]
#[ignore = "requires a live SSH server; see the module docs"]
async fn make_dir_is_idempotent_and_nests() {
    let (_, sftp, _) = openssh_connection().await;
    let root = workdir("mkdir");
    let nested = format!("{root}/a/b/c");
    make_dir(&sftp, &nested, true, None)
        .await
        .expect("mkdir -p");
    make_dir(&sftp, &nested, true, None)
        .await
        .expect("mkdir -p must be idempotent");
    assert!(stat(&sftp, &nested).await.unwrap().is_dir());

    let _ = sftp.remove_dir(&nested).await;
    let _ = sftp.remove_dir(&format!("{root}/a/b")).await;
    let _ = sftp.remove_dir(&format!("{root}/a")).await;
    let _ = sftp.remove_dir(&root).await;
}

#[tokio::test]
#[ignore = "requires a live SSH server; see the module docs"]
async fn a_local_path_that_is_a_directory_fails_without_touching_the_remote() {
    let (_, sftp, paths) = openssh_connection().await;
    let dir = workdir("local-dir");
    make_dir(&sftp, &dir, true, None).await.unwrap();
    let hasher_connection = Path::new(&dir).to_path_buf();
    assert!(hasher_connection.is_absolute());

    let error = upload_staged(&sftp, &paths.root, &format!("{dir}/x"), None, None)
        .await
        .expect_err("uploading a directory must fail");
    assert!(
        !error.to_string().is_empty(),
        "the failure must be reported rather than hanging: {error}"
    );
    assert_no_staged_leftovers(&sftp, &dir).await;

    cleanup(&sftp, &dir).await;
}
