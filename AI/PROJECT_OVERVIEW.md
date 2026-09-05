<!--
AI-DOC: Concise project overview generated on 2026-09-01.
Keep this aligned with README.md and the current workspace manifests.
-->

# AI SSH Project Overview

AI SSH is a macOS local SSH session manager for AI clients. `aissh-mcp` exposes MCP tools over stdio, `aisshd` owns SSH connections over a same-user Unix socket, and a Tauri menubar application manages configuration and observes session history.

## Technology

| Area | Technology |
| --- | --- |
| Service and protocol | Rust 2024, Tokio, Serde, MessagePack |
| SSH | `russh` |
| Persistence | SQLite WAL through `rusqlite` |
| Desktop | Tauri 2, React 19, TypeScript, Vite, xterm.js |
| Platform | macOS 12+ |

## Layout

```text
apps/
  daemon/       Same-user socket daemon
  mcp-server/   MCP stdio adapter
  desktop/      React UI and Tauri native shell
crates/
  config/       Configuration and permissions
  protocol/     Shared IPC contract
  session/      Session and command orchestration
  ssh/          SSH transport
  storage/      SQLite persistence
scripts/        Local install and LaunchAgent support
AI/             Generated architecture, analysis and test documentation
```

## Quick Start

Prerequisites: stable Rust, Node.js 20+, Xcode command-line tools and macOS 12+.

```sh
cd apps/desktop
npm install
npm run tauri dev
```

Configure an MCP client to run `~/.aissh/bin/aissh-mcp`. Add targets through the desktop Configuration tab; managed private keys belong under `~/.aissh/keys` with mode `0600`.

## Verification

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets
cd apps/desktop && npm run build
```

## Key Documents

- `README.md`: user setup and security model
- `AI/ARCHITECTURE.md`: component and state architecture
- `AI/CODEBASE_ANALYSIS.md`: implementation analysis
- `AI/TEST_REPORT.md`: MCP test evidence and defects
- `AI/NAMING.md`: observed naming conventions
- `AI/UPDATE.md`: documentation change history

