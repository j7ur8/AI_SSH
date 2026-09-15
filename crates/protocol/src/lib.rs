use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u16 = 3;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Upper bound for `wait_seconds` on polling requests, so a caller stays well
/// inside a client-side tool timeout.
pub const MAX_WAIT_SECONDS: u64 = 300;
/// Characters kept when echoing a command back to a client. Without this, every
/// poll of a long heredoc command re-ships the whole script.
pub const COMMAND_PREVIEW_CHARS: usize = 200;

/// Truncates a command for display, on a character boundary so multi-byte input
/// is never split.
pub fn command_preview(command: &str) -> String {
    let trimmed = command.trim();
    if trimmed.chars().count() <= COMMAND_PREVIEW_CHARS {
        return trimmed.to_owned();
    }
    let mut preview: String = trimmed.chars().take(COMMAND_PREVIEW_CHARS).collect();
    preview.push('…');
    preview
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Connecting,
    Ready,
    ExecRunning,
    PtyOpen,
    Idle,
    Disconnected,
    Interrupted,
    Closed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
    /// The transport dropped before an exit status arrived. Whether the remote
    /// process is still running is unknown; this is deliberately distinct from
    /// `Failed`, which means the channel ended with a real non-zero status.
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Stdout,
    Stderr,
    Pty,
    System,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetSummary {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub host_verification: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub target_id: String,
    pub target_name: String,
    pub purpose: String,
    pub client_name: String,
    pub status: SessionStatus,
    pub current_command: Option<String>,
    pub last_exit_code: Option<u32>,
    pub recording_truncated: bool,
    pub host_fingerprint: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandInfo {
    pub id: String,
    pub session_id: String,
    pub command: String,
    pub status: CommandStatus,
    pub exit_code: Option<u32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub last_sequence: u64,
    pub recording_truncated: bool,
    /// Bytes of stdout/stderr recorded for this command. Used to tell "the drop
    /// happened after the process produced output" from "we never saw anything".
    #[serde(default)]
    pub output_bytes: u64,
}

/// A command as seen from a fleet-wide listing: previews only, never the full
/// command text, so a listing stays small even when commands are long scripts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSummary {
    pub id: String,
    pub session_id: String,
    pub command_preview: String,
    pub status: CommandStatus,
    pub exit_code: Option<u32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub last_sequence: u64,
    pub output_bytes: u64,
    pub recording_truncated: bool,
    /// Seconds since the command started; `None` once it is finished.
    pub running_for_seconds: Option<u64>,
    /// Seconds since the most recent output event; `None` when nothing has been
    /// seen yet or the command is no longer live.
    pub seconds_since_last_output: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalEvent {
    pub session_id: String,
    pub command_id: Option<String>,
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    pub stream: StreamKind,
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
    pub persisted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStatInfo {
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub user: Option<String>,
    pub group: Option<String>,
    pub modified_at: Option<DateTime<Utc>>,
    /// Only computed when the caller asks for it; hashing costs a full read.
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContentInfo {
    pub path: String,
    /// Full size on the remote host, even when the returned slice is shorter.
    pub size: u64,
    pub bytes_returned: usize,
    pub truncated: bool,
    /// False when the bytes are not valid UTF-8, so a text rendering would be
    /// lossy garbage.
    pub is_utf8: bool,
    pub sha256: Option<String>,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOutcome {
    pub path: String,
    pub bytes: u64,
    pub sha256: Option<String>,
    /// True when a remote `sha256sum` confirmed the transfer byte-for-byte.
    /// False also covers "the remote host has no hashing tool available".
    pub verified: bool,
    /// False when the content already matched and the write was skipped.
    pub changed: bool,
    pub mode: Option<u32>,
    pub local_path: Option<String>,
}

/// Progress and loss signals that describe how much of a command's output this
/// response actually carries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventProgress {
    /// Seconds since the most recent output event for this command. `None` when
    /// the command is not live or has produced nothing yet.
    #[serde(default)]
    pub seconds_since_last_output: Option<u64>,
    /// The in-memory overflow buffer discarded entries, so some output between
    /// the recording limit and here is genuinely gone.
    #[serde(default)]
    pub live_tail_dropped: bool,
    /// The request had a `wait_seconds` budget and returned because it expired
    /// with no new output, not because the command finished.
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub waited_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Handshake {
        protocol_version: u16,
        client_name: String,
    },
    TargetsList,
    SessionsList {
        include_history: bool,
    },
    SessionCreate {
        target_id: String,
        purpose: String,
    },
    SessionStatus {
        session_id: String,
    },
    SessionClose {
        session_id: String,
    },
    ExecStart {
        session_id: String,
        command: String,
        cwd: Option<String>,
        env: BTreeMap<String, String>,
        timeout_seconds: Option<u64>,
    },
    ExecBackground {
        session_id: String,
        command: String,
        cwd: Option<String>,
        env: BTreeMap<String, String>,
        timeout_seconds: Option<u64>,
    },
    CommandPoll {
        command_id: String,
        after_sequence: u64,
        max_bytes: usize,
        /// Block up to this many seconds for new output instead of returning an
        /// empty page immediately. 0 restores the immediate-return behavior.
        #[serde(default)]
        wait_seconds: u64,
    },
    CommandCancel {
        command_id: String,
    },
    CommandsList {
        session_id: Option<String>,
        include_finished: bool,
        limit: u32,
    },
    ShellOpen {
        session_id: String,
        cols: u32,
        rows: u32,
        term: String,
    },
    ShellWrite {
        session_id: String,
        data: Vec<u8>,
    },
    ShellRead {
        session_id: String,
        after_sequence: u64,
        max_bytes: usize,
        #[serde(default)]
        wait_seconds: u64,
    },
    ShellResize {
        session_id: String,
        cols: u32,
        rows: u32,
    },
    ShellClose {
        session_id: String,
    },
    FileStat {
        session_id: String,
        remote_path: String,
        hash: bool,
    },
    FileRead {
        session_id: String,
        remote_path: String,
        max_bytes: usize,
        /// Read through `sudo -n` over an exec channel. Needed for paths the
        /// login user cannot open, which SFTP also cannot open.
        sudo: bool,
    },
    FileWrite {
        session_id: String,
        remote_path: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        append: bool,
        create_dirs: bool,
        mode: Option<u32>,
        sudo: bool,
        /// Skip the write when the remote content already has this hash.
        if_changed: bool,
    },
    FileUpload {
        session_id: String,
        local_path: String,
        remote_path: String,
        create_dirs: bool,
        mode: Option<u32>,
        verify: bool,
    },
    FileDownload {
        session_id: String,
        remote_path: String,
        local_path: String,
        overwrite: bool,
        create_dirs: bool,
        verify: bool,
    },
    FileMkdir {
        session_id: String,
        remote_path: String,
        parents: bool,
        mode: Option<u32>,
    },
    ReloadConfig,
    DaemonShutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ResponseData {
    Handshake {
        protocol_version: u16,
        daemon_version: String,
    },
    Targets(Vec<TargetSummary>),
    Sessions(Vec<SessionInfo>),
    Session(SessionInfo),
    Command(CommandInfo),
    Commands(Vec<CommandSummary>),
    FileStat(FileStatInfo),
    FileContent(FileContentInfo),
    FileOutcome(FileOutcome),
    Events {
        command: Option<CommandInfo>,
        #[serde(default)]
        commands: Vec<CommandInfo>,
        events: Vec<TerminalEvent>,
        next_sequence: u64,
        has_more: bool,
        #[serde(default)]
        delivery_complete: bool,
        #[serde(default)]
        warnings: Vec<String>,
        #[serde(default)]
        progress: EventProgress,
    },
    Ack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub request_id: u64,
    #[serde(flatten)]
    pub result: Result<ResponseData, ErrorPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl ErrorPayload {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFrame {
    pub request_id: u64,
    pub request: Request,
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> io::Result<()> {
    let body = rmp_serde::to_vec_named(value).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC frame is too large",
        ));
    }
    writer.write_u32(body.len() as u32).await?;
    writer.write_all(&body).await?;
    writer.flush().await
}

pub async fn read_frame<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> io::Result<T> {
    let len = reader.read_u32().await? as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC frame is too large",
        ));
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body).await?;
    rmp_serde::from_slice(&body).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn messagepack_frame_round_trip() {
        let input = RequestFrame {
            request_id: 9,
            request: Request::TargetsList,
        };
        let (mut client, mut server) = tokio::io::duplex(1024);
        let send = tokio::spawn(async move { write_frame(&mut client, &input).await.unwrap() });
        let output: RequestFrame = read_frame(&mut server).await.unwrap();
        send.await.unwrap();
        assert_eq!(output.request_id, 9);
        assert!(matches!(output.request, Request::TargetsList));
    }

    #[tokio::test]
    async fn response_error_round_trip() {
        let input = Response {
            request_id: 4,
            result: Err(ErrorPayload::new("SESSION_BUSY", "busy")),
        };
        let (mut client, mut server) = tokio::io::duplex(1024);
        let send = tokio::spawn(async move { write_frame(&mut client, &input).await.unwrap() });
        let output: Response = read_frame(&mut server).await.unwrap();
        send.await.unwrap();
        assert_eq!(output.result.unwrap_err().code, "SESSION_BUSY");
    }

    #[tokio::test]
    async fn daemon_shutdown_frame_round_trip() {
        let input = RequestFrame {
            request_id: 10,
            request: Request::DaemonShutdown,
        };
        let (mut client, mut server) = tokio::io::duplex(1024);
        let send = tokio::spawn(async move { write_frame(&mut client, &input).await.unwrap() });
        let output: RequestFrame = read_frame(&mut server).await.unwrap();
        send.await.unwrap();
        assert!(matches!(output.request, Request::DaemonShutdown));
    }

    #[test]
    fn events_without_command_list_remain_compatible() {
        let value = serde_json::json!({
            "kind": "events",
            "data": {
                "command": null,
                "events": [],
                "next_sequence": 0,
                "has_more": false
            }
        });
        let response: ResponseData = serde_json::from_value(value).unwrap();
        assert!(matches!(
            response,
            ResponseData::Events {
                commands,
                delivery_complete: false,
                warnings,
                ..
            } if commands.is_empty() && warnings.is_empty()
        ));
    }

    #[tokio::test]
    async fn file_requests_round_trip_over_messagepack() {
        let input = RequestFrame {
            request_id: 11,
            request: Request::FileUpload {
                session_id: "s".into(),
                local_path: "/tmp/src.tgz".into(),
                remote_path: "/srv/src.tgz".into(),
                create_dirs: true,
                mode: Some(0o644),
                verify: true,
            },
        };
        let (mut client, mut server) = tokio::io::duplex(1024);
        let send = tokio::spawn(async move { write_frame(&mut client, &input).await.unwrap() });
        let output: RequestFrame = read_frame(&mut server).await.unwrap();
        send.await.unwrap();
        assert!(matches!(
            output.request,
            Request::FileUpload {
                verify: true,
                mode: Some(0o644),
                ..
            }
        ));
    }

    #[test]
    fn poll_requests_default_wait_seconds_to_zero() {
        let value = serde_json::json!({
            "method": "command_poll",
            "params": {"command_id": "c", "after_sequence": 3, "max_bytes": 1024}
        });
        let request: Request = serde_json::from_value(value).unwrap();
        assert!(matches!(
            request,
            Request::CommandPoll {
                wait_seconds: 0,
                ..
            }
        ));
    }

    #[test]
    fn interrupted_status_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&CommandStatus::Interrupted).unwrap(),
            "\"interrupted\""
        );
    }

    #[test]
    fn file_content_carries_raw_bytes() {
        let info = FileContentInfo {
            path: "/etc/hosts".into(),
            size: 3,
            bytes_returned: 3,
            truncated: false,
            is_utf8: true,
            sha256: None,
            data: b"abc".to_vec(),
        };
        let encoded = rmp_serde::to_vec_named(&info).unwrap();
        // Two bytes of bin header prove the payload is not an array of integers.
        let decoded: FileContentInfo = rmp_serde::from_slice(&encoded).unwrap();
        assert_eq!(decoded.data, b"abc");
    }

    #[test]
    fn event_progress_defaults_are_inert() {
        let progress = EventProgress::default();
        assert!(!progress.timed_out);
        assert!(!progress.live_tail_dropped);
        assert_eq!(progress.waited_seconds, 0);
        assert_eq!(progress.seconds_since_last_output, None);
    }

    #[test]
    fn previews_long_commands_without_splitting_characters() {
        let short = "docker compose build";
        assert_eq!(command_preview(short), short);
        assert_eq!(command_preview("  padded  "), "padded");

        let long: String = "中".repeat(COMMAND_PREVIEW_CHARS + 50);
        let preview = command_preview(&long);
        assert_eq!(preview.chars().count(), COMMAND_PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
        assert!(preview.is_char_boundary(preview.len()));
    }
}
