use aissh_config::Paths;
use aissh_protocol::{
    PROTOCOL_VERSION, Request, RequestFrame, Response, ResponseData, read_frame, write_frame,
};
use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

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
                match tool_request(name, &args).and_then(|request| Ok(request)) {
                    Ok(request) => match daemon_call(&paths, &client_name, request).await {
                        Ok(data) => tool_result(id, public_json(data), false),
                        Err(error) => tool_result(id, public_error(error), true),
                    },
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

async fn daemon_call(paths: &Paths, client_name: &str, request: Request) -> Result<ResponseData> {
    let mut stream = UnixStream::connect(&paths.socket).await.with_context(|| {
        format!(
            "cannot connect to {}; start aisshd first",
            paths.socket.display()
        )
    })?;
    let handshake_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    write_frame(
        &mut stream,
        &RequestFrame {
            request_id: handshake_id,
            request: Request::Handshake {
                protocol_version: PROTOCOL_VERSION,
                client_name: client_name.into(),
            },
        },
    )
    .await?;
    let handshake: Response = read_frame(&mut stream).await?;
    handshake.result.map_err(|e| anyhow!(e))?;
    let request_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    write_frame(
        &mut stream,
        &RequestFrame {
            request_id,
            request,
        },
    )
    .await?;
    let response: Response = read_frame(&mut stream).await?;
    response.result.map_err(|e| anyhow!(e))
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
    Ok(match name {
        "ssh_targets_list" => Request::TargetsList,
        "ssh_sessions_list" => Request::SessionsList {
            include_history: args
                .get("include_history")
                .and_then(Value::as_bool)
                .unwrap_or(false),
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
        "ssh_command_poll" => Request::CommandPoll {
            command_id: string("command_id")?,
            after_sequence: after(),
            max_bytes: max(),
        },
        "ssh_command_cancel" => Request::CommandCancel {
            command_id: string("command_id")?,
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
        },
        "ssh_shell_resize" => Request::ShellResize {
            session_id: string("session_id")?,
            cols: u32_arg(args, "cols", 120),
            rows: u32_arg(args, "rows", 32),
        },
        "ssh_shell_close" => Request::ShellClose {
            session_id: string("session_id")?,
        },
        _ => return Err(anyhow!("unknown tool {name:?}")),
    })
}

fn public_json(data: ResponseData) -> Value {
    match data {
        ResponseData::Events {
            command,
            events,
            next_sequence,
            has_more,
        } => {
            json!({"command":command,"events":events.into_iter().map(|event|json!({"session_id":event.session_id,"command_id":event.command_id,"sequence":event.sequence,"timestamp":event.timestamp,"stream":event.stream,"data_base64":BASE64.encode(&event.payload),"text":String::from_utf8_lossy(&event.payload),"persisted":event.persisted})).collect::<Vec<_>>(),"next_sequence":next_sequence,"has_more":has_more})
        }
        other => serde_json::to_value(other).unwrap_or_else(|_| json!({})),
    }
}

fn public_error(error: anyhow::Error) -> Value {
    let message = error.to_string();
    if let Some((code, detail)) = message.split_once(": ") {
        if code.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            return json!({"code":code,"message":detail,"retryable":matches!(code,"SSH_ERROR"|"SESSION_BUSY"|"SESSION_DISCONNECTED")});
        }
    }
    json!({"code":"DAEMON_UNAVAILABLE","message":message,"retryable":true})
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
    json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":serde_json::to_string(&payload).unwrap()}],"structuredContent":payload,"isError":is_error}})
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
            "Start a non-interactive command. Prefer this for ordinary commands. Use a PTY only for persistent cwd/env, prompts, or full-screen programs. Login credentials are never supplied to sudo or remote prompts.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"string"},"cwd":{"type":"string"},"env":{"type":"object","additionalProperties":{"type":"string"}},"timeout_seconds":{"type":"integer","minimum":1,"maximum":86400}},"required":["session_id","command"],"additionalProperties":false}),
        ),
        tool(
            "ssh_command_poll",
            "Read stdout/stderr events after a sequence number and inspect command completion.",
            json!({"type":"object","properties":{"command_id":{"type":"string"},"after_sequence":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":1048576,"default":65536}},"required":["command_id"],"additionalProperties":false}),
        ),
        tool(
            "ssh_command_cancel",
            "Cancel a running exec command.",
            object(&[("command_id", "string")], &["command_id"]),
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
            "Read raw ANSI PTY output after a sequence number.",
            json!({"type":"object","properties":{"session_id":{"type":"string"},"after_sequence":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":1048576,"default":65536}},"required":["session_id"],"additionalProperties":false}),
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
    #[test]
    fn tool_list_has_all_public_tools() {
        assert_eq!(tools().len(), 13);
    }
    #[test]
    fn credentials_are_not_in_target_tool() {
        assert!(
            !serde_json::to_string(&tools()[0])
                .unwrap()
                .contains("password")
        );
    }
}
