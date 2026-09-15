//! MCP stdio adapter for the `aisshd` daemon.
//!
//! Two responsibilities beyond translating tool calls: shaping responses so a
//! page of command output travels once instead of several times, and rendering
//! a `content[].text` channel that carries the actual output rather than a
//! re-serialized copy of the structured payload.

use aissh_config::{Config, Paths};
use aissh_protocol::{
    CommandInfo, CommandStatus, ErrorPayload, FileContentInfo, FileOutcome, FileStatInfo,
    MAX_WAIT_SECONDS, Request, RequestFrame, Response, ResponseData, TerminalEvent,
    command_preview, read_frame, write_frame,
};
use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Mutex, OnceLock},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static TEXT_DECODERS: OnceLock<Mutex<HashMap<String, DecoderState>>> = OnceLock::new();

/// Ceiling for the rendered `content[].text` channel. The structured payload is
/// the machine-readable channel; this keeps a single-copy text rendering bounded
/// so a large page does not travel twice in full.
const TEXT_RENDER_BYTES: usize = 8 * 1024;
/// Default block time for `ssh_command_poll`. Polling a running command returns
/// as soon as output lands, so this only ever elapses during a quiet phase.
const DEFAULT_POLL_WAIT_SECONDS: u64 = 30;

#[derive(Default)]
struct DecoderState {
    last_sequence: u64,
    pending: Vec<u8>,
}

/// How much of a response to render.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Detail {
    /// Default: one text copy per event group, no base64, no repeated command text.
    Compact,
    /// Compatibility shape: per-event objects with both `text` and `data_base64`
    /// and the full command text.
    Full,
}

impl Detail {
    fn parse(args: &Value) -> Result<Self> {
        match args.get("detail").and_then(Value::as_str) {
            None | Some("compact") => Ok(Self::Compact),
            Some("full") => Ok(Self::Full),
            Some(other) => Err(anyhow!(
                "detail must be \"compact\" or \"full\", got {other:?}"
            )),
        }
    }
    fn is_compact(self) -> bool {
        self == Self::Compact
    }
}

/// The response-shaping arguments, validated once so a typo is reported instead
/// of quietly rendering something other than what was asked for.
struct Shaping {
    detail: Detail,
    encoding: Encoding,
    /// The caller's cursor. A repeated page omits the fields it already has, so
    /// polling a long command does not re-send them on every page.
    after_sequence: u64,
}

impl Shaping {
    fn parse(args: &Value) -> Result<Self> {
        Ok(Self {
            detail: Detail::parse(args)?,
            encoding: Encoding::parse(args)?,
            after_sequence: args
                .get("after_sequence")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        })
    }

    fn first_page(&self) -> bool {
        self.after_sequence == 0
    }
}

/// Requested encoding for file content.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Text,
    Base64,
}

impl Encoding {
    fn parse(args: &Value) -> Result<Self> {
        match args.get("encoding").and_then(Value::as_str) {
            None | Some("text") => Ok(Self::Text),
            Some("base64") => Ok(Self::Base64),
            Some(other) => Err(anyhow!(
                "encoding must be \"text\" or \"base64\", got {other:?}"
            )),
        }
    }
}

/// Why a daemon call failed, kept typed so the `retryable` flag the daemon
/// computed is preserved instead of being re-derived from a string.
enum CallError {
    /// The daemon answered with a structured error.
    Daemon(ErrorPayload),
    /// The daemon could not be reached or the exchange broke down.
    Transport(anyhow::Error),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let paths = Paths::discover()?;
    let mut client_name = String::from("MCP client");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let message: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(error) => {
                write_json(
                    &mut stdout,
                    &rpc_error(Value::Null, -32700, error.to_string()),
                )
                .await?;
                continue;
            }
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let response = match method {
            "initialize" => {
                if let Some(name) = message
                    .pointer("/params/clientInfo/name")
                    .and_then(Value::as_str)
                {
                    client_name = name.to_owned();
                }
                json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":message.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or("2025-03-26"),"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"aissh-mcp","version":env!("CARGO_PKG_VERSION")}}})
            }
            "ping" => json!({"jsonrpc":"2.0","id":id,"result":{}}),
            "tools/list" => json!({"jsonrpc":"2.0","id":id,"result":{"tools":tools()}}),
            "tools/call" => {
                let name = message
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let args = message
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match Shaping::parse(&args)
                    .and_then(|shaping| tool_request(name, &args).map(|request| (shaping, request)))
                {
                    Ok((shaping, request)) => {
                        match daemon_call(&paths, &client_name, request).await {
                            Ok(data) => tool_result(id, public_json(data, &shaping, &paths), false),
                            Err(error) => tool_result(id, public_error(error), true),
                        }
                    }
                    Err(error) => tool_result(
                        id,
                        json!({"code":"INVALID_ARGUMENT","message":error.to_string(),"retryable":false}),
                        true,
                    ),
                }
            }
            _ => rpc_error(id, -32601, format!("method {method:?} not found")),
        };
        write_json(&mut stdout, &response).await?;
    }
    Ok(())
}

async fn daemon_call(
    paths: &Paths,
    client_name: &str,
    request: Request,
) -> Result<ResponseData, CallError> {
    let mut stream = UnixStream::connect(&paths.socket)
        .await
        .with_context(|| {
            format!(
                "cannot connect to {}; start aisshd first",
                paths.socket.display()
            )
        })
        .map_err(CallError::Transport)?;
    let handshake_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    write_frame(
        &mut stream,
        &RequestFrame {
            request_id: handshake_id,
            request: Request::Handshake {
                protocol_version: aissh_protocol::PROTOCOL_VERSION,
                client_name: client_name.into(),
            },
        },
    )
    .await
    .map_err(|error| CallError::Transport(error.into()))?;
    let handshake: Response = read_frame(&mut stream)
        .await
        .map_err(|error| CallError::Transport(error.into()))?;
    handshake.result.map_err(CallError::Daemon)?;
    let request_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    write_frame(
        &mut stream,
        &RequestFrame {
            request_id,
            request,
        },
    )
    .await
    .map_err(|error| CallError::Transport(error.into()))?;
    let response: Response = read_frame(&mut stream)
        .await
        .map_err(|error| CallError::Transport(error.into()))?;
    response.result.map_err(CallError::Daemon)
}

fn tool_request(name: &str, args: &Value) -> Result<Request> {
    let string = |key: &str| required_string(args, key);
    let after = || {
        args.get("after_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let max = || {
        args.get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(65536) as usize
    };
    let wait = |default: u64| {
        args.get("wait_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(default)
            .min(MAX_WAIT_SECONDS)
    };
    let mode = || {
        args.get("mode")
            .and_then(Value::as_u64)
            .map(|value| {
                u32::try_from(value)
                    .ok()
                    .filter(|value| *value <= 0o7777)
                    .ok_or_else(|| anyhow!("mode must be an octal permission value up to 07777"))
            })
            .transpose()
    };
    let flag = |key: &str, default: bool| args.get(key).and_then(Value::as_bool).unwrap_or(default);
    Ok(match name {
        "ssh_targets_list" => Request::TargetsList,
        "ssh_sessions_list" => Request::SessionsList {
            include_history: flag("include_history", false),
        },
        "ssh_session_create" => Request::SessionCreate {
            target_id: string("target_id")?,
            purpose: string("purpose")?,
        },
        "ssh_session_status" => Request::SessionStatus {
            session_id: string("session_id")?,
        },
        "ssh_session_close" => Request::SessionClose {
            session_id: string("session_id")?,
        },
        "ssh_exec_start" => Request::ExecStart {
            session_id: string("session_id")?,
            command: string("command")?,
            cwd: optional_string(args, "cwd")?,
            env: env_map(args)?,
            timeout_seconds: args.get("timeout_seconds").and_then(Value::as_u64),
        },
        "ssh_exec_background" => Request::ExecBackground {
            session_id: string("session_id")?,
            command: string("command")?,
            cwd: optional_string(args, "cwd")?,
            env: env_map(args)?,
            timeout_seconds: args.get("timeout_seconds").and_then(Value::as_u64),
        },
        "ssh_command_poll" => Request::CommandPoll {
            command_id: string("command_id")?,
            after_sequence: after(),
            max_bytes: max(),
            wait_seconds: wait(DEFAULT_POLL_WAIT_SECONDS),
        },
        "ssh_command_cancel" => Request::CommandCancel {
            command_id: string("command_id")?,
        },
        "ssh_commands_list" => Request::CommandsList {
            session_id: optional_string(args, "session_id")?,
            include_finished: flag("include_finished", false),
            limit: args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .clamp(1, 500) as u32,
        },
        "ssh_shell_open" => Request::ShellOpen {
            session_id: string("session_id")?,
            cols: u32_arg(args, "cols", 120),
            rows: u32_arg(args, "rows", 32),
            term: optional_string(args, "term")?.unwrap_or_else(|| "xterm-256color".into()),
        },
        "ssh_shell_write" => Request::ShellWrite {
            session_id: string("session_id")?,
            data: BASE64
                .decode(string("data_base64")?)
                .context("data_base64 is not valid base64")?,
        },
        "ssh_shell_read" => Request::ShellRead {
            session_id: string("session_id")?,
            after_sequence: after(),
            max_bytes: max(),
            wait_seconds: wait(0),
        },
        "ssh_shell_resize" => Request::ShellResize {
            session_id: string("session_id")?,
            cols: u32_arg(args, "cols", 120),
            rows: u32_arg(args, "rows", 32),
        },
        "ssh_shell_close" => Request::ShellClose {
            session_id: string("session_id")?,
        },
        "ssh_file_stat" => Request::FileStat {
            session_id: string("session_id")?,
            remote_path: string("remote_path")?,
            hash: flag("hash", false),
        },
        "ssh_file_read" => Request::FileRead {
            session_id: string("session_id")?,
            remote_path: string("remote_path")?,
            max_bytes: max(),
            sudo: flag("sudo", false),
        },
        "ssh_file_write" => Request::FileWrite {
            session_id: string("session_id")?,
            remote_path: string("remote_path")?,
            data: content_bytes(args)?,
            append: flag("append", false),
            create_dirs: flag("create_dirs", false),
            mode: mode()?,
            sudo: flag("sudo", false),
            if_changed: flag("if_changed", false),
        },
        "ssh_file_upload" => Request::FileUpload {
            session_id: string("session_id")?,
            local_path: string("local_path")?,
            remote_path: string("remote_path")?,
            create_dirs: flag("create_dirs", false),
            mode: mode()?,
            verify: flag("verify", true),
        },
        "ssh_file_download" => Request::FileDownload {
            session_id: string("session_id")?,
            remote_path: string("remote_path")?,
            local_path: string("local_path")?,
            overwrite: flag("overwrite", false),
            create_dirs: flag("create_dirs", false),
            verify: flag("verify", true),
        },
        "ssh_file_mkdir" => Request::FileMkdir {
            session_id: string("session_id")?,
            remote_path: string("remote_path")?,
            parents: flag("parents", true),
            mode: mode()?,
        },
        _ => return Err(anyhow!("unknown tool {name:?}")),
    })
}

fn content_bytes(args: &Value) -> Result<Vec<u8>> {
    let text = args.get("content").and_then(Value::as_str);
    let encoded = args.get("content_base64").and_then(Value::as_str);
    match (text, encoded) {
        (Some(_), Some(_)) => Err(anyhow!(
            "provide either content or content_base64, not both"
        )),
        (Some(text), None) => Ok(text.as_bytes().to_vec()),
        (None, Some(encoded)) => BASE64
            .decode(encoded)
            .context("content_base64 is not valid base64"),
        (None, None) => Err(anyhow!("content or content_base64 is required")),
    }
}

fn public_json(data: ResponseData, shaping: &Shaping, paths: &Paths) -> Value {
    let detail = shaping.detail;
    let encoding = shaping.encoding;
    match data {
        ResponseData::Events {
            command,
            commands,
            events,
            next_sequence,
            has_more,
            delivery_complete,
            warnings,
            progress,
        } => {
            let command_id = command.as_ref().map(|value| value.id.as_str());
            let decoded = events
                .into_iter()
                .map(|event| decode_event(&event, command_id))
                .collect::<Vec<_>>();
            if delivery_complete {
                if let Some(command) = &command {
                    clear_decoders_for_command(&command.id);
                }
            }
            if !has_more {
                for value in &commands {
                    if !matches!(value.status, CommandStatus::Running) {
                        clear_decoders_for_command(&value.id);
                    }
                }
            }
            let primary_id = command.as_ref().map(|value| value.id.clone());
            let output_value = if detail.is_compact() {
                json!({ "chunks": chunks_from(&decoded) })
            } else {
                json!({ "events": events_from(&decoded) })
            };
            let unknown_remote_state = command
                .as_ref()
                .is_some_and(|value| value.status == CommandStatus::Interrupted);
            let recording_truncated = command
                .as_ref()
                .is_some_and(|value| value.recording_truncated);
            let mut payload = json!({
                "command": command
                    .as_ref()
                    .map(|value| command_view(value, detail, shaping.first_page())),
                // The primary command is never repeated in this list; `commands`
                // carries only the additional ones a session-scoped read found.
                "commands": commands
                    .iter()
                    .filter(|value| Some(&value.id) != primary_id.as_ref())
                    .map(|value| command_view(value, detail, shaping.first_page()))
                    .collect::<Vec<_>>(),
                "next_sequence": next_sequence,
                "has_more": has_more,
                "poll_complete": delivery_complete,
                "sequence_scope": "session",
                "remote_state_unknown": unknown_remote_state,
                "truncation": {
                    "page_truncated": has_more,
                    "recording_truncated": recording_truncated,
                    "live_tail_dropped": progress.live_tail_dropped,
                    "recording_limit_mib": recording_limit_mib(paths),
                },
                "progress": {
                    "seconds_since_last_output": progress.seconds_since_last_output,
                    "timed_out": progress.timed_out,
                    "waited_seconds": progress.waited_seconds,
                },
                "warnings": warnings,
            });
            if let Some(object) = payload.as_object_mut() {
                if let Some(output) = output_value.as_object() {
                    for (key, value) in output {
                        object.insert(key.clone(), value.clone());
                    }
                }
            }
            payload
        }
        ResponseData::FileContent(info) => {
            enveloped("file_content", file_content_json(info, detail, encoding))
        }
        ResponseData::FileStat(info) => enveloped("file_stat", file_stat_json(&info)),
        ResponseData::FileOutcome(outcome) => {
            enveloped("file_outcome", file_outcome_json(&outcome))
        }
        ResponseData::Commands(summaries) => enveloped(
            "commands",
            json!({ "commands": serde_json::to_value(summaries).unwrap_or_else(|_| json!([])) }),
        ),
        // A one-shot response: the caller's own command text is echoed in full
        // here. Only the repeating poll path drops it.
        ResponseData::Command(command) => {
            enveloped("command", command_view(&command, Detail::Full, true))
        }
        other => serde_json::to_value(other).unwrap_or_else(|_| json!({})),
    }
}

/// Pre-existing simple responses are wrapped as `{kind, data}`; new ones follow
/// the same shape rather than introducing a second convention.
fn enveloped(kind: &str, data: Value) -> Value {
    json!({ "kind": kind, "data": data })
}

/// The recording budget applies to a whole session, so it is reported alongside
/// the page flag to keep the two truncation levels distinguishable.
fn recording_limit_mib(paths: &Paths) -> Option<u64> {
    Config::load(paths)
        .ok()
        .map(|config| config.recording_limit_mib)
}

/// One decoded event, ready to be grouped for compact rendering.
struct DecodedEvent {
    stream: &'static str,
    text: String,
    sequence: u64,
    persisted: bool,
    payload: Vec<u8>,
}

fn decode_event(event: &TerminalEvent, command_id: Option<&str>) -> DecodedEvent {
    let key = event
        .command_id
        .as_deref()
        .or(command_id)
        .map(|id| format!("command:{id}:{}", stream_name(&event.stream)))
        .unwrap_or_else(|| {
            format!(
                "session:{}:{}",
                event.session_id,
                stream_name(&event.stream)
            )
        });
    let text = decode_event_text(&key, event.sequence, &event.payload);
    DecodedEvent {
        stream: stream_name(&event.stream),
        text,
        sequence: event.sequence,
        persisted: event.persisted,
        payload: event.payload.clone(),
    }
}

struct Chunk {
    stream: &'static str,
    persisted: bool,
    first_sequence: u64,
    last_sequence: u64,
    count: u64,
    text: String,
}

/// Merges runs of same-stream events so per-event metadata stops repeating once
/// per line of output.
fn chunks_from(decoded: &[DecodedEvent]) -> Vec<Value> {
    let mut chunks: Vec<Chunk> = Vec::new();
    for event in decoded {
        match chunks.last_mut() {
            Some(chunk) if chunk.stream == event.stream && chunk.persisted == event.persisted => {
                chunk.text.push_str(&event.text);
                chunk.last_sequence = event.sequence;
                chunk.count += 1;
            }
            _ => chunks.push(Chunk {
                stream: event.stream,
                persisted: event.persisted,
                first_sequence: event.sequence,
                last_sequence: event.sequence,
                count: 1,
                text: event.text.clone(),
            }),
        }
    }
    chunks
        .into_iter()
        .map(|chunk| {
            json!({
                "stream": chunk.stream,
                "text": chunk.text,
                "first_sequence": chunk.first_sequence,
                "last_sequence": chunk.last_sequence,
                "events": chunk.count,
                "persisted": chunk.persisted,
            })
        })
        .collect()
}

fn events_from(decoded: &[DecodedEvent]) -> Vec<Value> {
    decoded
        .iter()
        .map(|event| {
            json!({
                "stream": event.stream,
                "text": event.text,
                "data_base64": BASE64.encode(&event.payload),
                "sequence": event.sequence,
                "persisted": event.persisted,
            })
        })
        .collect()
}

fn command_view(command: &CommandInfo, detail: Detail, include_preview: bool) -> Value {
    let mut value = serde_json::to_value(command).unwrap_or_else(|_| json!({}));
    if let Some(object) = value.as_object_mut() {
        if include_preview {
            object.insert(
                "command_preview".into(),
                json!(command_preview(&command.command)),
            );
        }
        if detail.is_compact() {
            // The full text is already summarised by command_preview; re-sending
            // a long heredoc on every poll is pure overhead.
            object.remove("command");
        }
    }
    value
}

fn file_stat_json(info: &FileStatInfo) -> Value {
    serde_json::to_value(info).unwrap_or_else(|_| json!({}))
}

fn file_outcome_json(outcome: &FileOutcome) -> Value {
    serde_json::to_value(outcome).unwrap_or_else(|_| json!({}))
}

fn file_content_json(info: FileContentInfo, detail: Detail, encoding: Encoding) -> Value {
    let text = info
        .is_utf8
        .then(|| String::from_utf8_lossy(&info.data).into_owned());
    let mut value = json!({
        "path": info.path,
        "size": info.size,
        "bytes_returned": info.bytes_returned,
        "truncated": info.truncated,
        "is_utf8": info.is_utf8,
        "sha256": info.sha256,
    });
    // Text is the useful form; base64 is the lossless one. Send base64 when the
    // caller asked for it, when the bytes are not text at all, or in full detail.
    let include_base64 = encoding == Encoding::Base64 || !info.is_utf8 || !detail.is_compact();
    let include_text = encoding == Encoding::Text && info.is_utf8;
    if let Some(object) = value.as_object_mut() {
        if include_text {
            object.insert("text".into(), json!(text));
        }
        if include_base64 {
            object.insert("data_base64".into(), json!(BASE64.encode(&info.data)));
            object.insert(
                "encoding".into(),
                json!(if info.is_utf8 { "text" } else { "binary" }),
            );
        }
        if !include_text && encoding == Encoding::Base64 {
            object.insert("encoding".into(), json!("base64"));
        }
    }
    value
}

fn stream_name(stream: &aissh_protocol::StreamKind) -> &'static str {
    match stream {
        aissh_protocol::StreamKind::Stdout => "stdout",
        aissh_protocol::StreamKind::Stderr => "stderr",
        aissh_protocol::StreamKind::Pty => "pty",
        aissh_protocol::StreamKind::System => "system",
    }
}

fn decode_event_text(key: &str, sequence: u64, payload: &[u8]) -> String {
    let decoders = TEXT_DECODERS.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut decoders) = decoders.lock() else {
        return String::from_utf8_lossy(payload).into_owned();
    };
    let state = decoders.entry(key.to_owned()).or_default();
    if sequence <= state.last_sequence {
        state.pending.clear();
    }
    state.last_sequence = state.last_sequence.max(sequence);

    let mut bytes = Vec::with_capacity(state.pending.len() + payload.len());
    bytes.extend_from_slice(&state.pending);
    bytes.extend_from_slice(payload);
    match std::str::from_utf8(&bytes) {
        Ok(text) => {
            state.pending.clear();
            text.to_owned()
        }
        Err(error) if error.error_len().is_none() => {
            let valid = error.valid_up_to();
            let text = String::from_utf8(bytes[..valid].to_vec()).unwrap_or_default();
            state.pending = bytes[valid..].to_vec();
            text
        }
        Err(_) => {
            state.pending.clear();
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }
}

fn clear_decoders_for_command(command_id: &str) {
    let Some(decoders) = TEXT_DECODERS.get() else {
        return;
    };
    let Ok(mut decoders) = decoders.lock() else {
        return;
    };
    decoders.retain(|key, _| !key.starts_with(&format!("command:{command_id}:")));
}

fn public_error(error: CallError) -> Value {
    match error {
        // The daemon already decided whether this is retryable.
        CallError::Daemon(payload) => json!({
            "code": payload.code,
            "message": payload.message,
            "retryable": payload.retryable,
        }),
        CallError::Transport(error) => {
            json!({"code":"DAEMON_UNAVAILABLE","message":error.to_string(),"retryable":true})
        }
    }
}

/// Renders the `content[].text` channel.
///
/// For output-bearing responses this carries the actual output rather than a
/// JSON re-serialization of the same payload, bounded by [`TEXT_RENDER_BYTES`].
fn render_text(payload: &Value, is_error: bool) -> String {
    if is_error {
        return serde_json::to_string_pretty(payload).unwrap_or_default();
    }
    // Simple responses are wrapped as {kind, data}; the text rendering describes
    // the payload itself rather than repeating the envelope.
    let body = match payload.get("data") {
        Some(data) if payload.get("kind").is_some() => data,
        _ => payload,
    };
    if let Some(chunks) = body.get("chunks").and_then(Value::as_array) {
        return render_output_text(body, chunks);
    }
    if let Some(events) = body.get("events").and_then(Value::as_array) {
        return render_output_text(body, events);
    }
    if body.get("path").is_some() && body.get("bytes").is_some() {
        return render_outcome_text(body);
    }
    if let Some(text) = body.get("text").and_then(Value::as_str) {
        let mut out = format!("{}\n", file_header(body));
        out.push_str(text);
        if body.get("truncated").and_then(Value::as_bool) == Some(true) {
            out.push_str("\n[file is larger than bytes_returned; raise max_bytes for more]");
        }
        return out;
    }
    if let Some(commands) = body.get("commands").and_then(Value::as_array)
        && commands
            .iter()
            .all(|item| item.get("command_preview").is_some())
    {
        let mut out = String::new();
        for item in commands {
            out.push_str(&format!(
                "{} {} {}\n",
                item.get("id").and_then(Value::as_str).unwrap_or_default(),
                item.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                item.get("command_preview")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ));
        }
        return out;
    }
    if body.get("kind").is_some() {
        return format!(
            "{} {} {} bytes, mode {}, sha256 {}\n",
            data_kind(payload),
            body.get("path").and_then(Value::as_str).unwrap_or_default(),
            body.get("size").and_then(Value::as_u64).unwrap_or(0),
            body.get("mode")
                .and_then(Value::as_u64)
                .map(|value| format!("{value:04o}"))
                .unwrap_or_else(|| "unknown".into()),
            body.get("sha256")
                .and_then(Value::as_str)
                .unwrap_or("not computed"),
        );
    }
    serde_json::to_string(payload).unwrap_or_default()
}

/// Human label for an enveloped response, used by the text channel.
fn data_kind(payload: &Value) -> &str {
    match payload.get("kind").and_then(Value::as_str) {
        Some("file_stat") => "stat",
        Some("file_content") => "read",
        Some("file_outcome") => "wrote",
        _ => "file",
    }
}

fn render_output_text(payload: &Value, items: &[Value]) -> String {
    let command = payload.get("command");
    let mut header = String::from("status=");
    header.push_str(
        command
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
    );
    if let Some(code) = command
        .and_then(|value| value.get("exit_code"))
        .filter(|value| !value.is_null())
    {
        header.push_str(&format!(" exit_code={code}"));
    }
    header.push_str(&format!(
        " next_sequence={} has_more={} poll_complete={}",
        payload
            .get("next_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        payload
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        payload
            .get("poll_complete")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    ));
    if let Some(progress) = payload.get("progress") {
        if let Some(age) = progress
            .get("seconds_since_last_output")
            .and_then(Value::as_u64)
        {
            header.push_str(&format!(" quiet_for={age}s"));
        }
        if progress.get("timed_out").and_then(Value::as_bool) == Some(true) {
            header.push_str(&format!(
                " waited={}s_timed_out",
                progress
                    .get("waited_seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ));
        }
    }
    if payload.get("remote_state_unknown").and_then(Value::as_bool) == Some(true) {
        header.push_str(" remote_state_unknown=true");
    }
    header.push('\n');

    let mut budget = TEXT_RENDER_BYTES;
    let mut output = String::new();
    for item in items {
        let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
        let stream = item
            .get("stream")
            .and_then(Value::as_str)
            .unwrap_or("stdout");
        if stream != "stdout" && !text.is_empty() {
            output.push_str(&format!("[{stream}] "));
        }
        if !append_capped(&mut output, text, &mut budget) {
            output.push_str(
                "\n[text rendering capped; use the structured payload or continue polling]\n",
            );
            break;
        }
    }
    let mut out = header;
    out.push_str(&output);
    if let Some(warnings) = payload.get("warnings").and_then(Value::as_array) {
        for warning in warnings {
            out.push_str(&format!(
                "warning: {}\n",
                warning.as_str().unwrap_or_default()
            ));
        }
    }
    out
}

fn render_outcome_text(payload: &Value) -> String {
    let mut out = format!(
        "{} {} bytes sha256={} verified={} changed={}\n",
        payload
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        payload.get("bytes").and_then(Value::as_u64).unwrap_or(0),
        payload
            .get("sha256")
            .and_then(Value::as_str)
            .unwrap_or("not computed"),
        payload
            .get("verified")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        payload
            .get("changed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    if let Some(local) = payload.get("local_path").and_then(Value::as_str) {
        out.push_str(&format!("local_path={local}\n"));
    }
    if payload.get("verified").and_then(Value::as_bool) != Some(true) {
        out.push_str(
            "warning: the remote host reported no digest, so the transfer is unverified\n",
        );
    }
    out
}

fn file_header(payload: &Value) -> String {
    format!(
        "{} {} bytes ({} returned) truncated={}",
        payload
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        payload.get("size").and_then(Value::as_u64).unwrap_or(0),
        payload
            .get("bytes_returned")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        payload
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )
}

/// Appends at most `budget` bytes on a character boundary, reporting whether the
/// whole value fit.
fn append_capped(out: &mut String, value: &str, budget: &mut usize) -> bool {
    if value.len() <= *budget {
        *budget -= value.len();
        out.push_str(value);
        return true;
    }
    let cut = value
        .char_indices()
        .take_while(|(index, _)| *index < *budget)
        .last()
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    out.push_str(&value[..cut]);
    *budget = 0;
    false
}

fn required_string(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{key} must be a non-empty string"))
}
fn optional_string(args: &Value, key: &str) -> Result<Option<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(v)) => Ok(Some(v.clone())),
        _ => Err(anyhow!("{key} must be a string")),
    }
}
fn u32_arg(args: &Value, key: &str, default: u32) -> u32 {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(default)
}
fn env_map(args: &Value) -> Result<BTreeMap<String, String>> {
    match args.get("env") {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(values)) => values
            .iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|s| (k.clone(), s.to_owned()))
                    .ok_or_else(|| anyhow!("env.{k} must be a string"))
            })
            .collect(),
        _ => Err(anyhow!("env must be an object")),
    }
}
fn tool_result(id: Value, payload: Value, is_error: bool) -> Value {
    let text = render_text(&payload, is_error);
    json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":text}],"structuredContent":payload,"isError":is_error}})
}
fn rpc_error(id: Value, code: i32, message: String) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
async fn write_json<W: AsyncWriteExt + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    writer
        .write_all(serde_json::to_string(value)?.as_bytes())
        .await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

fn tools() -> Vec<Value> {
    vec![
        tool(
            "ssh_targets_list",
            "List configured SSH targets. Credentials are never returned.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "ssh_sessions_list",
            "List daemon-owned SSH sessions; sessions survive this MCP process.",
            json!({"type":"object","properties":{"include_history":{"type":"boolean","default":false}},"additionalProperties":false}),
        ),
        tool(
            "ssh_session_create",
            "Create and authenticate a logical SSH session.",
            object(
                &[("target_id", "string"), ("purpose", "string")],
                &["target_id", "purpose"],
            ),
        ),
        tool(
            "ssh_session_status",
            "Get a session's state and active command.",
            object(&[("session_id", "string")], &["session_id"]),
        ),
        tool(
            "ssh_session_close",
            "Close a session and its SSH connection.",
            object(&[("session_id", "string")], &["session_id"]),
        ),
        tool(
            "ssh_exec_start",
            "Start a non-interactive command. Prefer this for ordinary commands. Use a PTY only for persistent cwd/env, prompts, or full-screen programs. Login credentials are never supplied to sudo or remote prompts. A dropped transport marks the command interrupted rather than failed, with a warning saying whether the remote process is known to have started.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"string"},"cwd":{"type":"string"},"env":{"type":"object","additionalProperties":{"type":"string"}},"timeout_seconds":{"type":"integer","minimum":1,"maximum":86400}},"required":["session_id","command"],"additionalProperties":false}),
        ),
        tool(
            "ssh_exec_background",
            "Start a non-interactive background command without occupying the session foreground. Returns a command_id for ssh_command_poll and ssh_command_cancel; the default timeout is 24 hours.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"string"},"cwd":{"type":"string"},"env":{"type":"object","additionalProperties":{"type":"string"}},"timeout_seconds":{"type":"integer","minimum":1,"maximum":86400}},"required":["session_id","command"],"additionalProperties":false}),
        ),
        tool(
            "ssh_command_poll",
            "Read command output starting after a session-scoped sequence number. Blocks up to wait_seconds for new output instead of returning an empty page, so a quiet build does not need a client-side sleep loop; progress.seconds_since_last_output and progress.timed_out tell you whether it is working or stalled. Continue until poll_complete is true. Responses report the three truncation levels separately in `truncation`.",
            json!({"type":"object","properties":{"command_id":{"type":"string"},"after_sequence":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":1048576,"default":65536},"wait_seconds":{"type":"integer","minimum":0,"maximum":300,"default":30},"detail":{"type":"string","enum":["compact","full"],"default":"compact"}},"required":["command_id"],"additionalProperties":false}),
        ),
        tool(
            "ssh_command_cancel",
            "Cancel a running exec command.",
            object(&[("command_id", "string")], &["command_id"]),
        ),
        tool(
            "ssh_commands_list",
            "List commands across sessions, newest first, so concurrent long tasks are visible in one call. Defaults to running commands only. Pair with ssh_command_cancel.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"include_finished":{"type":"boolean","default":false},"limit":{"type":"integer","minimum":1,"maximum":500,"default":50}},"additionalProperties":false}),
        ),
        tool(
            "ssh_shell_open",
            "Open an interactive PTY only when exec cannot satisfy the task.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"cols":{"type":"integer","default":120},"rows":{"type":"integer","default":32},"term":{"type":"string","default":"xterm-256color"}},"required":["session_id"],"additionalProperties":false}),
        ),
        tool(
            "ssh_shell_write",
            "Write base64-encoded bytes to an open PTY.",
            object(
                &[("session_id", "string"), ("data_base64", "string")],
                &["session_id", "data_base64"],
            ),
        ),
        tool(
            "ssh_shell_read",
            "Read raw ANSI PTY output after a sequence number, optionally blocking up to wait_seconds for it.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"after_sequence":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":1048576,"default":65536},"wait_seconds":{"type":"integer","minimum":0,"maximum":300,"default":0},"detail":{"type":"string","enum":["compact","full"],"default":"compact"}},"required":["session_id"],"additionalProperties":false}),
        ),
        tool(
            "ssh_shell_resize",
            "Resize the remote PTY. The observer window never resizes it.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"cols":{"type":"integer"},"rows":{"type":"integer"}},"required":["session_id","cols","rows"],"additionalProperties":false}),
        ),
        tool(
            "ssh_shell_close",
            "Close an open PTY without closing the SSH session.",
            object(&[("session_id", "string")], &["session_id"]),
        ),
        tool(
            "ssh_file_stat",
            "Stat a remote path via SFTP. Follows symlinks. Set hash=true to also compute the remote SHA-256.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"remote_path":{"type":"string"},"hash":{"type":"boolean","default":false}},"required":["session_id","remote_path"],"additionalProperties":false}),
        ),
        tool(
            "ssh_file_read",
            "Read a remote file with truncation and encoding handled for you: no shell quoting, and no base64 round trip unless you ask. Returns text when the bytes are UTF-8, otherwise base64. Use sudo=true for paths the login user cannot open, such as /var/lib/docker volumes.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"remote_path":{"type":"string"},"max_bytes":{"type":"integer","minimum":1,"maximum":1048576,"default":65536},"encoding":{"type":"string","enum":["text","base64"],"default":"text"},"sudo":{"type":"boolean","default":false},"detail":{"type":"string","enum":["compact","full"],"default":"compact"}},"required":["session_id","remote_path"],"additionalProperties":false}),
        ),
        tool(
            "ssh_file_write",
            "Write a remote file from content or content_base64. The bytes land at a staged path first and are moved into place only after a remote SHA-256 confirms them, so a partial or mangled write cannot appear at the destination. Set if_changed=true to skip an identical write, and sudo=true for root-owned paths.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"remote_path":{"type":"string"},"content":{"type":"string"},"content_base64":{"type":"string"},"append":{"type":"boolean","default":false},"create_dirs":{"type":"boolean","default":false},"mode":{"type":"integer","description":"octal permissions, e.g. 420 for 0644"},"sudo":{"type":"boolean","default":false},"if_changed":{"type":"boolean","default":false}},"required":["session_id","remote_path"],"additionalProperties":false}),
        ),
        tool(
            "ssh_file_upload",
            "Upload a local file over SFTP, verifying it against a remote SHA-256 before it replaces the destination. Use this instead of piping base64 through a heredoc. local_path must be absolute or start with ~, and refers to this machine.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"local_path":{"type":"string"},"remote_path":{"type":"string"},"create_dirs":{"type":"boolean","default":false},"mode":{"type":"integer","description":"octal permissions, e.g. 420 for 0644"},"verify":{"type":"boolean","default":true}},"required":["session_id","local_path","remote_path"],"additionalProperties":false}),
        ),
        tool(
            "ssh_file_download",
            "Download a remote file over SFTP to an absolute local path, verified, staged, and renamed into place so a failure never leaves a partial file. Refuses to replace an existing path unless overwrite=true.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"remote_path":{"type":"string"},"local_path":{"type":"string"},"overwrite":{"type":"boolean","default":false},"create_dirs":{"type":"boolean","default":false},"verify":{"type":"boolean","default":true}},"required":["session_id","remote_path","local_path"],"additionalProperties":false}),
        ),
        tool(
            "ssh_file_mkdir",
            "Create a remote directory, optionally including missing parents.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"remote_path":{"type":"string"},"parents":{"type":"boolean","default":true},"mode":{"type":"integer","description":"octal permissions, e.g. 493 for 0755"}},"required":["session_id","remote_path"],"additionalProperties":false}),
        ),
    ]
}
fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({"name":name,"description":description,"inputSchema":input_schema})
}
fn object(properties: &[(&str, &str)], required: &[&str]) -> Value {
    let mut map = Map::new();
    for (name, kind) in properties {
        map.insert((*name).into(), json!({"type":kind}));
    }
    json!({"type":"object","properties":map,"required":required,"additionalProperties":false})
}

#[cfg(test)]
mod tests {
    use super::*;
    use aissh_protocol::{EventProgress, StreamKind};

    fn event(sequence: u64, stream: StreamKind, payload: &[u8]) -> TerminalEvent {
        TerminalEvent {
            session_id: "s".into(),
            command_id: Some("c".into()),
            sequence,
            timestamp: chrono::Utc::now(),
            stream,
            payload: payload.to_vec(),
            persisted: true,
        }
    }

    fn shaping(detail: Detail) -> Shaping {
        Shaping {
            detail,
            encoding: Encoding::Text,
            after_sequence: 0,
        }
    }

    fn events_payload(events: Vec<TerminalEvent>, detail: Detail) -> Value {
        public_json(
            ResponseData::Events {
                command: Some(CommandInfo {
                    id: "c".into(),
                    session_id: "s".into(),
                    command: "docker compose build".into(),
                    status: CommandStatus::Running,
                    exit_code: None,
                    started_at: chrono::Utc::now(),
                    finished_at: None,
                    last_sequence: 0,
                    recording_truncated: false,
                    output_bytes: 0,
                }),
                commands: vec![],
                events,
                next_sequence: 0,
                has_more: false,
                delivery_complete: false,
                warnings: vec![],
                progress: EventProgress::default(),
            },
            &shaping(detail),
            &Paths::under(std::path::PathBuf::from("/unused")),
        )
    }

    #[test]
    fn tool_list_has_all_public_tools() {
        assert_eq!(tools().len(), 21);
    }

    #[test]
    fn parses_background_exec_request() {
        let request = tool_request(
            "ssh_exec_background",
            &json!({"session_id":"s","command":"sleep 1"}),
        )
        .unwrap();
        assert!(matches!(request, Request::ExecBackground { .. }));
    }

    #[test]
    fn poll_defaults_to_blocking_for_output() {
        let request = tool_request("ssh_command_poll", &json!({"command_id":"c"})).unwrap();
        assert!(
            matches!(
                request,
                Request::CommandPoll {
                    wait_seconds: 30,
                    ..
                }
            ),
            "a poll that returns instantly forces a client-side sleep loop"
        );
    }

    #[test]
    fn shell_read_does_not_block_unless_asked() {
        let request = tool_request("ssh_shell_read", &json!({"session_id":"s"})).unwrap();
        assert!(matches!(
            request,
            Request::ShellRead {
                wait_seconds: 0,
                ..
            }
        ));
    }

    #[test]
    fn wait_seconds_is_capped() {
        let request = tool_request(
            "ssh_command_poll",
            &json!({"command_id":"c","wait_seconds":100000}),
        )
        .unwrap();
        assert!(matches!(
            request,
            Request::CommandPoll { wait_seconds, .. } if wait_seconds == MAX_WAIT_SECONDS
        ));
    }

    #[test]
    fn compact_events_carry_one_text_copy_and_no_base64() {
        let payload = events_payload(
            vec![event(1, StreamKind::Stdout, b"layer 1\n")],
            Detail::Compact,
        );
        assert_eq!(payload["chunks"][0]["text"], "layer 1\n");
        assert!(payload.get("events").is_none());
        assert!(
            payload["chunks"][0].get("data_base64").is_none(),
            "compact rendering must not ship the same bytes base64-encoded too"
        );
    }

    #[test]
    fn compact_events_merge_consecutive_same_stream_output() {
        let payload = events_payload(
            vec![
                event(1, StreamKind::Stdout, b"one\n"),
                event(2, StreamKind::Stdout, b"two\n"),
                event(3, StreamKind::Stderr, b"oops\n"),
                event(4, StreamKind::Stdout, b"three\n"),
            ],
            Detail::Compact,
        );
        let chunks = payload["chunks"].as_array().unwrap();
        assert_eq!(chunks.len(), 3, "runs of the same stream collapse");
        assert_eq!(chunks[0]["text"], "one\ntwo\n");
        assert_eq!(chunks[0]["events"], 2);
        assert_eq!(chunks[0]["first_sequence"], 1);
        assert_eq!(chunks[0]["last_sequence"], 2);
        assert_eq!(chunks[1]["stream"], "stderr");
        assert_eq!(chunks[2]["text"], "three\n");
    }

    #[test]
    fn full_detail_keeps_the_legacy_per_event_shape() {
        let payload = events_payload(vec![event(7, StreamKind::Stdout, b"x")], Detail::Full);
        let events = payload["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["sequence"], 7);
        assert_eq!(events[0]["data_base64"], BASE64.encode(b"x"));
        assert_eq!(payload["command"]["command"], "docker compose build");
    }

    #[test]
    fn compact_never_repeats_the_full_command_text() {
        let payload = events_payload(vec![], Detail::Compact);
        assert!(payload["command"].get("command").is_none());
        assert_eq!(
            payload["command"]["command_preview"],
            "docker compose build"
        );
    }

    #[test]
    fn a_long_command_is_previewed_not_repeated() {
        let long = "x".repeat(5000);
        let payload = events_payload(vec![], Detail::Compact);
        assert_eq!(
            payload["command"]["command_preview"]
                .as_str()
                .unwrap()
                .len(),
            "docker compose build".len()
        );

        let value = command_view(
            &CommandInfo {
                id: "c".into(),
                session_id: "s".into(),
                command: long.clone(),
                status: CommandStatus::Running,
                exit_code: None,
                started_at: chrono::Utc::now(),
                finished_at: None,
                last_sequence: 0,
                recording_truncated: false,
                output_bytes: 0,
            },
            Detail::Compact,
            true,
        );
        let preview = value["command_preview"].as_str().unwrap();
        assert!(preview.len() < long.len());
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn a_repeated_page_does_not_resend_the_command_preview() {
        let command = || CommandInfo {
            id: "c".into(),
            session_id: "s".into(),
            command: "docker compose build --no-cache".into(),
            status: CommandStatus::Running,
            exit_code: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            last_sequence: 0,
            recording_truncated: false,
            output_bytes: 0,
        };
        let page = |after_sequence| {
            public_json(
                ResponseData::Events {
                    command: Some(command()),
                    commands: vec![],
                    events: vec![],
                    next_sequence: 4,
                    has_more: false,
                    delivery_complete: false,
                    warnings: vec![],
                    progress: EventProgress::default(),
                },
                &Shaping {
                    detail: Detail::Compact,
                    encoding: Encoding::Text,
                    after_sequence,
                },
                &Paths::under(std::path::PathBuf::from("/unused")),
            )
        };
        assert!(
            page(0)["command"].get("command_preview").is_some(),
            "the first page tells the caller which command it is looking at"
        );
        assert!(
            page(7)["command"].get("command_preview").is_none(),
            "a later page of the same command must not resend it"
        );
        // The fields that can actually change stay present on every page.
        assert_eq!(page(7)["command"]["status"], "running");
        assert!(page(7)["command"].get("id").is_some());
    }

    #[test]
    fn truncation_levels_are_reported_separately() {
        let payload = public_json(
            ResponseData::Events {
                command: Some(CommandInfo {
                    id: "c".into(),
                    session_id: "s".into(),
                    command: "build".into(),
                    status: CommandStatus::Running,
                    exit_code: None,
                    started_at: chrono::Utc::now(),
                    finished_at: None,
                    last_sequence: 0,
                    recording_truncated: true,
                    output_bytes: 0,
                }),
                commands: vec![],
                events: vec![],
                next_sequence: 9,
                has_more: true,
                delivery_complete: false,
                warnings: vec![],
                progress: EventProgress {
                    live_tail_dropped: true,
                    ..Default::default()
                },
            },
            &shaping(Detail::Compact),
            &Paths::under(std::path::PathBuf::from("/unused")),
        );
        let truncation = &payload["truncation"];
        assert_eq!(truncation["page_truncated"], true);
        assert_eq!(truncation["recording_truncated"], true);
        assert_eq!(truncation["live_tail_dropped"], true);
    }

    #[test]
    fn an_interrupted_command_is_flagged_at_the_top_level() {
        let payload = public_json(
            ResponseData::Events {
                command: Some(CommandInfo {
                    id: "c".into(),
                    session_id: "s".into(),
                    command: "build".into(),
                    status: CommandStatus::Interrupted,
                    exit_code: None,
                    started_at: chrono::Utc::now(),
                    finished_at: None,
                    last_sequence: 3,
                    recording_truncated: false,
                    output_bytes: 12,
                }),
                commands: vec![],
                events: vec![],
                next_sequence: 3,
                has_more: false,
                delivery_complete: true,
                warnings: vec!["transport dropped".into()],
                progress: EventProgress::default(),
            },
            &shaping(Detail::Compact),
            &Paths::under(std::path::PathBuf::from("/unused")),
        );
        assert_eq!(payload["remote_state_unknown"], true);
        assert_eq!(payload["command"]["status"], "interrupted");
    }

    #[test]
    fn shell_read_commands_do_not_repeat_the_primary_entry() {
        let primary = CommandInfo {
            id: "c".into(),
            session_id: "s".into(),
            command: "primary".into(),
            status: CommandStatus::Running,
            exit_code: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            last_sequence: 0,
            recording_truncated: false,
            output_bytes: 0,
        };
        let mut other = primary.clone();
        other.id = "other".into();
        other.command = "other command".into();
        let payload = public_json(
            ResponseData::Events {
                command: Some(primary),
                commands: vec![primary_again(), other],
                events: vec![],
                next_sequence: 0,
                has_more: false,
                delivery_complete: false,
                warnings: vec![],
                progress: EventProgress::default(),
            },
            &shaping(Detail::Compact),
            &Paths::under(std::path::PathBuf::from("/unused")),
        );
        let commands = payload["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 1, "the primary command is not repeated");
        assert_eq!(commands[0]["id"], "other");
    }

    fn primary_again() -> CommandInfo {
        CommandInfo {
            id: "c".into(),
            session_id: "s".into(),
            command: "primary".into(),
            status: CommandStatus::Running,
            exit_code: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            last_sequence: 0,
            recording_truncated: false,
            output_bytes: 0,
        }
    }

    #[test]
    fn text_channel_carries_the_output_not_a_json_copy() {
        let payload = events_payload(
            vec![event(1, StreamKind::Stdout, b"step 1 ok\n")],
            Detail::Compact,
        );
        let text = render_text(&payload, false);
        assert!(text.starts_with("status=running"));
        assert!(text.contains("step 1 ok"));
        assert!(
            !text.contains("\"chunks\""),
            "the text channel must not re-serialize the structured payload"
        );
        assert!(text.len() < serde_json::to_string(&payload).unwrap().len());
    }

    #[test]
    fn text_rendering_is_bounded() {
        let big = vec![b'y'; TEXT_RENDER_BYTES * 2];
        let payload = events_payload(vec![event(1, StreamKind::Stdout, &big)], Detail::Compact);
        let text = render_text(&payload, false);
        assert!(text.len() < TEXT_RENDER_BYTES + 1024);
        assert!(text.contains("text rendering capped"));
    }

    #[test]
    fn text_rendering_marks_stderr_and_warnings() {
        let payload = public_json(
            ResponseData::Events {
                command: None,
                commands: vec![],
                events: vec![event(1, StreamKind::Stderr, b"boom\n")],
                next_sequence: 1,
                has_more: false,
                delivery_complete: true,
                warnings: vec!["no output".into()],
                progress: EventProgress::default(),
            },
            &shaping(Detail::Compact),
            &Paths::under(std::path::PathBuf::from("/unused")),
        );
        let text = render_text(&payload, false);
        assert!(text.contains("[stderr] boom"));
        assert!(text.contains("warning: no output"));
    }

    #[test]
    fn binary_file_content_falls_back_to_base64() {
        let info = FileContentInfo {
            path: "/tmp/blob".into(),
            size: 2,
            bytes_returned: 2,
            truncated: false,
            is_utf8: false,
            sha256: None,
            data: vec![0xff, 0xfe],
        };
        let value = file_content_json(info, Detail::Compact, Encoding::Text);
        assert_eq!(value["is_utf8"], false);
        assert!(value.get("text").is_none());
        assert_eq!(value["data_base64"], BASE64.encode([0xff, 0xfe]));
    }

    #[test]
    fn text_file_content_omits_base64_in_compact_mode() {
        let value = file_content_json(
            FileContentInfo {
                path: "/etc/hosts".into(),
                size: 5,
                bytes_returned: 5,
                truncated: false,
                is_utf8: true,
                sha256: Some("abc".into()),
                data: b"hello".to_vec(),
            },
            Detail::Compact,
            Encoding::Text,
        );
        assert_eq!(value["text"], "hello");
        assert!(
            value.get("data_base64").is_none(),
            "text output must not also ship the same bytes base64-encoded"
        );
    }

    #[test]
    fn base64_encoding_is_honored_on_request() {
        let value = file_content_json(
            FileContentInfo {
                path: "/etc/hosts".into(),
                size: 5,
                bytes_returned: 5,
                truncated: false,
                is_utf8: true,
                sha256: None,
                data: b"hello".to_vec(),
            },
            Detail::Compact,
            Encoding::Base64,
        );
        assert_eq!(value["data_base64"], BASE64.encode(b"hello"));
        assert_eq!(value["encoding"], "base64");
        assert!(value.get("text").is_none());
    }

    #[test]
    fn file_write_requires_exactly_one_content_source() {
        assert!(content_bytes(&json!({"content":"a"})).is_ok());
        assert!(content_bytes(&json!({"content_base64":BASE64.encode(b"a")})).is_ok());
        assert!(content_bytes(&json!({})).is_err());
        assert!(content_bytes(&json!({"content":"a","content_base64":"YQ=="})).is_err());
    }

    #[test]
    fn file_requests_parse_into_protocol_requests() {
        let request = tool_request(
            "ssh_file_upload",
            &json!({"session_id":"s","local_path":"/tmp/a","remote_path":"/srv/a"}),
        )
        .unwrap();
        assert!(
            matches!(
                request,
                Request::FileUpload {
                    verify: true,
                    create_dirs: false,
                    ..
                }
            ),
            "verification is on unless explicitly disabled"
        );

        let request = tool_request(
            "ssh_file_download",
            &json!({"session_id":"s","remote_path":"/srv/a","local_path":"/tmp/a"}),
        )
        .unwrap();
        assert!(
            matches!(
                request,
                Request::FileDownload {
                    overwrite: false,
                    ..
                }
            ),
            "download must not clobber an existing local file by default"
        );

        let request = tool_request(
            "ssh_file_mkdir",
            &json!({"session_id":"s","remote_path":"/srv/a","mode":493}),
        )
        .unwrap();
        assert!(matches!(
            request,
            Request::FileMkdir {
                mode: Some(493),
                parents: true,
                ..
            }
        ));
    }

    #[test]
    fn rejects_an_out_of_range_mode() {
        let error = tool_request(
            "ssh_file_write",
            &json!({"session_id":"s","remote_path":"/a","content":"x","mode":99999}),
        )
        .unwrap_err();
        assert!(error.to_string().contains("mode"));
    }

    #[test]
    fn commands_list_parses_scoping_arguments() {
        let request = tool_request(
            "ssh_commands_list",
            &json!({"include_finished":true,"limit":9000}),
        )
        .unwrap();
        assert!(matches!(
            request,
            Request::CommandsList {
                session_id: None,
                include_finished: true,
                limit: 500
            }
        ));
    }

    #[test]
    fn rejects_an_unknown_detail_value() {
        assert!(Detail::parse(&json!({"detail":"verbose"})).is_err());
        assert!(
            !Detail::parse(&json!({"detail":"full"}))
                .unwrap()
                .is_compact()
        );
        // The whole call must fail rather than quietly rendering as compact.
        assert!(Shaping::parse(&json!({"detail":"verbose"})).is_err());
        assert!(Shaping::parse(&json!({"encoding":"hex"})).is_err());
        assert!(Shaping::parse(&json!({})).is_ok());
    }

    #[test]
    fn daemon_errors_keep_the_daemon_retryable_flag() {
        let value = public_error(CallError::Daemon(
            ErrorPayload::new("SESSION_DISCONNECTED", "gone").retryable(),
        ));
        assert_eq!(value["code"], "SESSION_DISCONNECTED");
        assert_eq!(value["retryable"], true);

        let value = public_error(CallError::Daemon(ErrorPayload::new(
            "COMMAND_NOT_FOUND",
            "missing",
        )));
        assert_eq!(value["retryable"], false);

        let value = public_error(CallError::Transport(anyhow!("socket refused")));
        assert_eq!(value["code"], "DAEMON_UNAVAILABLE");
        assert_eq!(value["retryable"], true);
    }

    #[test]
    fn event_json_exposes_the_polling_contract() {
        let value = public_json(
            ResponseData::Events {
                command: None,
                commands: vec![],
                events: vec![],
                next_sequence: 41,
                has_more: false,
                delivery_complete: true,
                warnings: vec!["no output".into()],
                progress: EventProgress::default(),
            },
            &shaping(Detail::Compact),
            &Paths::under(std::path::PathBuf::from("/unused")),
        );
        assert_eq!(value["next_sequence"], 41);
        assert_eq!(value["sequence_scope"], "session");
        assert_eq!(value["poll_complete"], true);
        assert_eq!(value["warnings"][0], "no output");
    }

    #[test]
    fn credentials_are_not_in_target_tool() {
        assert!(
            !serde_json::to_string(&tools()[0])
                .unwrap()
                .contains("password")
        );
    }

    #[test]
    fn file_tools_match_the_naming_convention() {
        for name in [
            "ssh_file_stat",
            "ssh_file_read",
            "ssh_file_write",
            "ssh_file_upload",
            "ssh_file_download",
            "ssh_file_mkdir",
            "ssh_commands_list",
        ] {
            assert!(
                tools()
                    .iter()
                    .any(|tool| tool["name"].as_str() == Some(name)),
                "{name} is missing from the tool list"
            );
        }
    }

    #[test]
    fn decodes_utf8_split_across_events() {
        let key = "test:split-utf8";
        if let Some(decoders) = TEXT_DECODERS.get() {
            decoders.lock().unwrap().remove(key);
        }
        assert_eq!(decode_event_text(key, 1, &[0xe4]), "");
        assert_eq!(decode_event_text(key, 2, &[0xb8, 0xad]), "中");
        if let Some(decoders) = TEXT_DECODERS.get() {
            decoders.lock().unwrap().remove(key);
        }
    }
}
