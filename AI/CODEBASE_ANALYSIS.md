<!--
AI-DOC: Codebase analysis generated from current source and tests on 2026-09-01.
Update facts after changes; never include real credentials or recorded terminal content.
-->

# Codebase Analysis

## Overview And Stack

AI SSH is a macOS-only local service that exposes persistent SSH sessions to AI clients through MCP and provides a read-only Tauri observer. The workspace uses Rust 2024 (minimum 1.85), Tokio, `russh`, MessagePack, SQLite/`rusqlite`, Tauri 2, React 19, TypeScript, Vite and xterm.js.

## Core Entry Points

| Entry | Role |
| --- | --- |
| `apps/mcp-server/src/main.rs:27` | MCP stdio event loop |
| `apps/mcp-server/src/main.rs:95` | Per-call daemon handshake and request |
| `apps/mcp-server/src/main.rs:129` | MCP argument-to-protocol conversion |
| `apps/daemon/src/main.rs:15` | Daemon startup and socket accept loop |
| `apps/daemon/src/main.rs:103` | Local IPC dispatch |
| `crates/session/src/lib.rs:99` | SSH session creation |
| `crates/session/src/lib.rs:233` | Foreground/background exec start |
| `crates/session/src/lib.rs:384` | Command polling |
| `crates/session/src/lib.rs:453` | PTY open |
| `crates/storage/src/lib.rs:175` | Event pagination |
| `crates/ssh/src/lib.rs:47` | Remote connection and authentication |
| `crates/ssh/src/lib.rs:212` | Safe shell command construction |
| `apps/desktop/src-tauri/src/main.rs:572` | Tauri application entry |
| `apps/desktop/src/main.tsx` | Session workspace selection, event paging and embedded read-only xterm transcript |

## Core Algorithms

- Shell command construction quotes `cwd` and environment values, validates environment names, selects a remote UTF-8 locale, then executes the caller command through `/bin/sh -c`.
- Event persistence assigns session-wide sequence numbers, writes data until the configured per-session recording cap, and retains a 4 MiB live overflow tail afterward.
- Polling pages persisted events by byte and event-count limits, then appends newer live events only after the persisted page is exhausted so cursors cannot skip data.
- MCP text decoding retains incomplete UTF-8 suffix bytes per command and stream while always returning lossless base64 alongside text.

## Main Flows

1. MCP initialize -> tools/list -> tool call -> daemon handshake -> typed request -> structured MCP result.
2. Session create -> SSH connect/authenticate -> fingerprint capture -> session persistence.
3. Exec start -> channel spawn -> stdout/stderr recording -> poll pages -> terminal status and `poll_complete`.
4. PTY open/write/resize/read/close with foreground exclusivity.
5. Historical session open -> persisted status lookup -> persisted command/event pagination without requiring a live runtime.
6. Desktop session selection -> locally scrollable session list -> embedded transcript detail -> paged command/event rendering in the main Webview.

## Data Model

- `Target`: identity, host, port, username and tagged password/private-key authentication.
- `SessionInfo`: lifecycle status, client/purpose metadata, current command, last exit code, fingerprint and timestamps.
- `CommandInfo`: command text, status, exit code, sequence boundary, timestamps and truncation flag.
- `TerminalEvent`: session/optional command ID, sequence, timestamp, stream kind, bytes and persistence flag.
- Session states: `connecting`, `ready`, `exec_running`, `pty_open`, `idle`, `disconnected`, `interrupted`, `closed`, `failed`.
- Command states: `running`, `completed`, `failed`, `timed_out`, `cancelled`.

Sensitive fields are password, private-key passphrase, private-key file contents, recorded command text/output, host fingerprint and connection identifiers. SQLite and TOML storage are deliberately plaintext, protected only by filesystem permissions.

## Public Interfaces

The MCP server publishes 14 tools: target/session listing, session create/status/close, foreground/background exec, command poll/cancel, and PTY open/write/read/resize/close. The daemon additionally exposes handshake, config reload and shutdown requests to local native clients. The Tauri bridge exposes configuration, target test, session observation and native folder/window actions. The desktop uses one main Webview: its session page constrains scrolling to the left list, embeds the selected command transcript on the right, and accepts tray selections through a `select-session` event.

## Error Handling And Configuration

Daemon errors use `ErrorPayload { code, message, retryable }`. MCP tool failures are returned as successful JSON-RPC responses with `isError: true`. Configuration rejects unknown TOML fields, invalid settings, duplicate/empty targets, insecure file modes and private keys outside the managed keys directory.

## Test Strategy And Current Result

- `cargo fmt --all -- --check`: passed.
- `cargo test --workspace --all-targets`: 31 passed, 0 failed, including historical status/event recovery without a runtime.
- `npm run build`: passed; Vite warns that the main JavaScript chunk is over 500 kB.
- `npm run tauri build -- --debug`: passed and produced a macOS application bundle; the packaged main window registered as a foreground application and appeared in the Dock while open.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: not run because the stable toolchain lacks the Clippy component.
- MCP stdio and live-daemon probes: initialize/list/error shapes tested; installed helper exposes 14 tools and connects to daemon.
- Historical command poll: 519 stored events were delivered exactly once over 61 pages with monotonic cursors and terminal `poll_complete: true`.
- Fresh remote exec/background/PTY end-to-end tests were not possible because the current configuration exposes zero targets.

## Confirmed Issues

Detailed reproduction evidence is in `AI/TEST_REPORT.md`.

1. High: MCP initialization accepts and echoes an unsupported arbitrary protocol version.
2. Medium: closed sessions remain in the in-memory active set; `include_history=false` returned only closed sessions in the live probe.
3. Medium: tool arguments are not validated against published JSON Schemas; wrong types and extra properties can silently succeed.
4. Medium: daemon startup interrupts unfinished sessions but does not transition unfinished commands, which can leave a command permanently `running` after a crash.
5. Low: the MCP loop does not validate the `jsonrpc` member and accepts a JSON-RPC 1.0 request as if it were 2.0.

## Hidden Considerations

- The daemon is a shared same-UID service. `client_name` is metadata, not an authorization boundary.
- A repository test build does not necessarily refresh `target/debug/aissh-mcp`; explicitly run `cargo build -p aissh-mcp` before manual binary probes.
- Installed daemon and MCP helpers must use the same internal protocol version.
- Database timestamp parsing uses `expect`; malformed local database timestamps can panic a read path.
- The desktop changes macOS activation policy at runtime: opening the main window uses regular mode and closing it returns to accessory mode so the tray process can remain resident without a permanent Dock icon.
