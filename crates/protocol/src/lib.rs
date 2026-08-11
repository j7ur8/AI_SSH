use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Stdout,
    Stderr,
    Pty,
    System,
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
    CommandPoll {
        command_id: String,
        after_sequence: u64,
        max_bytes: usize,
    },
    CommandCancel {
        command_id: String,
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
    },
    ShellResize {
        session_id: String,
        cols: u32,
        rows: u32,
    },
    ShellClose {
        session_id: String,
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
    Events {
        command: Option<CommandInfo>,
        events: Vec<TerminalEvent>,
        next_sequence: u64,
        has_more: bool,
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
}
