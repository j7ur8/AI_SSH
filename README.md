# AI SSH

AI SSH is a macOS-only local SSH session service. AI clients use the `aissh-mcp` stdio server; the MCP process delegates all connection ownership to `aisshd` over a same-user Unix socket. The Tauri menubar app observes active and historical sessions without sending terminal input.

## Security model

- Host-key verification is deliberately disabled. The accepted SHA256 fingerprint is recorded and displayed.
- Passwords and private-key passphrases are plaintext in `~/.aissh/config.toml`; recordings are plaintext SQLite data.
- `config.toml` and private keys must be mode `0600`; directories are mode `0700`. Keys outside `~/.aissh/keys` are rejected.
- Login passwords are never returned through MCP and are never injected into `sudo` or other prompts.
- The local socket is mode `0600` and the daemon additionally checks the peer UID.

## Development

Prerequisites are current stable Rust, Node.js 20+, Xcode command-line tools, and macOS 12+.

```sh
cd apps/desktop
npm install
npm run tauri dev
```

`npm run tauri dev` builds the daemon and MCP helpers, starts the desktop app, and adds the AI SSH icon to the macOS menu bar. If `aisshd` is not already running, approve the native startup dialog. The app creates `~/.aissh/config.toml` automatically and opens the configuration window when no targets exist.

To build and open a standalone debug app:

```sh
cd apps/desktop
npm run tauri build -- --debug
open "../../target/debug/bundle/macos/AI SSH.app"
```

After the app is running, click the AI SSH icon in the macOS menu bar and choose **Open AI SSH**. Quitting the configuration window does not stop the menubar app; use **Quit Menubar** from its menu to exit it.

Targets and credentials can be edited in the app's **Configuration** tab. Private keys must be placed in `~/.aissh/keys` with mode `0600`.

The legacy `scripts/install-local.sh` command installs `aisshd` as a login LaunchAgent. It is not required for normal desktop development because the app installs and starts its bundled helper automatically.

Configure an MCP client with the stable executable path:

```json
{
  "mcpServers": {
    "ai-ssh": {
      "command": "/Users/YOUR_USER/.aissh/bin/aissh-mcp"
    }
  }
}
```

Ordinary commands should use `ssh_exec_start` followed by `ssh_command_poll`. The command runs through `/bin/sh -c`, so shell builtins, pipelines, redirections, and compound scripts are supported. PTY tools are reserved for interactive prompts, persistent shell state, and full-screen terminal programs. A logical session permits only one foreground exec or PTY at a time.

Event sequence numbers are scoped to the session, not to an individual command. Start polling a new command with `after_sequence: 0`, then pass the returned `next_sequence` to the next call. Events are returned in ascending order. A terminal `command.status` means remote execution has ended, but paged output may remain; stop only when `poll_complete` is true. The response also includes `has_more`, `sequence_scope: "session"`, and explicit `warnings` for a completed command with no output or output truncated by the configured recording limit.

Long-running non-interactive work can use `ssh_exec_background`. It returns the same command handle used by `ssh_command_poll` and `ssh_command_cancel`, does not occupy the session foreground, and defaults to a 24-hour timeout. Closing the session cancels its foreground and background commands.

## Workspace

- `apps/daemon`: daemon, Unix socket server, lifecycle tasks
- `apps/mcp-server`: MCP JSON-RPC stdio adapter
- `apps/desktop`: Tauri 2 tray and React/xterm.js observer
- `crates/protocol`: versioned MessagePack IPC contract
- `crates/config`: versioned TOML and permission enforcement
- `crates/ssh`: `russh` authentication, exec, and PTY channels
- `crates/session`: concurrency, timeout, cancellation, and idle state
- `crates/storage`: SQLite WAL history, retention, and recording caps

## Release

Local installation is automated. Distribution still requires project-specific Apple credentials: build both `aarch64-apple-darwin` and `x86_64-apple-darwin` binaries, combine helpers with `lipo`, configure the Tauri updater public key, then sign and notarize the `.app` with the owning Developer ID. Those identity-bound steps are intentionally not hard-coded.
