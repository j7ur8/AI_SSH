# AI SSH

AI SSH is a macOS-only local SSH session service. AI clients use the `aissh-mcp` stdio server; the MCP process delegates all connection ownership to `aisshd` over a same-user Unix socket. The Tauri menubar app observes active and historical sessions without sending terminal input.

## Security model

- Host-key verification is deliberately disabled. The accepted SHA256 fingerprint is recorded and displayed.
- Passwords and private-key passphrases are plaintext in `~/.aissh/config.toml`; recordings are plaintext SQLite data.
- `config.toml` and private keys must be mode `0600`; directories are mode `0700`. Keys outside `~/.aissh/keys` are rejected.
- Login passwords are never returned through MCP and are never injected into `sudo` or other prompts.
- The local socket is mode `0600` and the daemon additionally checks the peer UID.
- `ssh_file_upload` and `ssh_file_download` read and write **local** paths on this machine at the MCP client's request. They require an absolute path (or `~`), and a download refuses to replace an existing file unless `overwrite` is set. This grants a same-UID MCP client nothing it could not already do for itself, but it is a deliberate widening of what the daemon touches and is stated here explicitly.
- Update checks fetch a manifest from `github.com` over https; that is the only outbound request the app makes on its own. An update is installed only if a minisign signature matches the public key compiled into the app, and never without confirmation.
- `ssh_file_write` and `ssh_file_read` accept `sudo: true`, which runs the privileged step through `sudo -n` over an exec channel. That only works when passwordless sudo is permitted for the login user; the login password is never supplied to it.

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

A local build produces the app but not the signed update archive. That is deliberate: enabling `createUpdaterArtifacts` in `tauri.conf.json` makes every `tauri build` require the private signing key, which only the release workflow holds. The release enables it with a `--config` override instead.

After the app is running, click the AI SSH icon in the macOS menu bar and choose **Open AI SSH**. Quitting the configuration window does not stop the menubar app; use **Quit Menubar** from its menu to exit it.

Targets and credentials can be edited in the app's **Configuration** tab. Private keys must be placed in `~/.aissh/keys` with mode `0600`.

The legacy `scripts/install-local.sh` command installs `aisshd` as a login LaunchAgent. It is not required for normal desktop development because the app installs and starts its bundled helper automatically.

`aissh-mcp` and `aisshd` are versioned together: both exchange protocol version 3, and a mismatch is refused with `PROTOCOL_MISMATCH` rather than half-working. After rebuilding, reinstall both helpers in `~/.aissh/bin` and restart the daemon (and rebuild the desktop app, which bundles its own copies).

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

### Moving files

Use `ssh_file_upload`, `ssh_file_download`, `ssh_file_write`, and `ssh_file_read` rather than piping base64 through a shell heredoc. They run over a real SFTP subsystem channel, so nothing passes through shell quoting and no encoding round trip can alter the bytes.

Every write is staged: the content lands at a sibling temp path in the destination directory, its bytes are hashed, the remote host is asked for the digest of what actually arrived, and only a match is renamed into place. A truncated or mangled transfer therefore cannot appear at the destination, and a mismatch fails with `TRANSFER_VERIFY_FAILED` while the previous content is left untouched. Downloads use the same discipline locally. Verification needs a hashing tool (`sha256sum` or `shasum -a 256`) on the remote host; without one the transfer still completes and reports `verified: false` rather than pretending otherwise.

`ssh_file_write` with `if_changed: true` compares the remote digest first and skips an identical write, and `ssh_file_read` reports the full size alongside `truncated` so a capped read is never mistaken for a complete file. `ssh_file_read` uses SFTP, so for a path the login user cannot open (for example a Docker volume under `/var/lib/docker`) pass `sudo: true`. `ssh_file_stat` follows symlinks, and `ssh_file_mkdir` creates parents by default.

Event sequence numbers are scoped to the session, not to an individual command. Start polling a new command with `after_sequence: 0`, then pass the returned `next_sequence` to the next call. Output is returned in ascending order. A terminal `command.status` means remote execution has ended, but paged output may remain; stop only when `poll_complete` is true.

### Waiting for output instead of sleeping

`ssh_command_poll` blocks up to `wait_seconds` (default 30, maximum 300) and returns as soon as output arrives. It only ever waits during a genuinely quiet phase, so a long build no longer needs a client-side sleep loop. When the budget expires with nothing new, `progress.timed_out` is set and `progress.seconds_since_last_output` reports how long the command has been silent — that pair is what distinguishes steady work from a stall. `ssh_shell_read` takes the same argument but defaults to 0 so an observer never blocks.

### Reading responses efficiently

`ssh_command_poll`, `ssh_shell_read`, and `ssh_file_read` accept `detail`, which defaults to `compact`:

- Output is returned as `chunks`: one entry per run of same-stream events, carrying the merged text and the sequence range it covers. `detail: "full"` restores the per-event `events` array with a `data_base64` field on each entry.
- The full command text is not echoed back on every page. The first page carries `command_preview` (at most 200 characters); later pages omit it, since the caller already has it.
- `content[].text` carries the rendered output, bounded at 8 KiB, rather than a JSON re-serialization of the whole payload. `structuredContent` remains the machine-readable channel.

For reference, a 338-line file-hash listing arrives as about 37 KB in the default mode instead of about 103 KB in `full`.

### Truncation has three separate causes

`truncation` names each one instead of leaving `recording_truncated: false` to be misread as "the output was complete":

| Field | Meaning |
| --- | --- |
| `page_truncated` | This response was cut short. More output is waiting; keep polling. Same as `has_more`. |
| `recording_truncated` | The session's `recording_limit_mib` budget dropped earlier output. It is no longer in storage. |
| `live_tail_dropped` | The in-memory overflow buffer discarded its oldest entries. Some output is unrecoverable. |

### When a connection drops

If the SSH transport ends before an exit status arrives, the command is reported as `interrupted`, not `failed` — the two are deliberately different, because a dropped link says nothing about the exit code. The response sets `remote_state_unknown` and adds a warning that distinguishes the two cases: if output was received, the remote process definitely started and may still be running; if nothing was received, whether it started is unknown.

The daemon never re-runs an interrupted command, since it cannot know how far it got. It drops the dead connection and reconnects on the next command, up to `reconnect_attempts` times with `reconnect_backoff_seconds` between attempts. A session whose reconnect fails is reported as `disconnected` rather than keeping a dead handle around.

### Watching concurrent work

`ssh_commands_list` lists commands across every session, newest first, defaulting to running ones, with a `command_preview` and `seconds_since_last_output` for each. Pair it with `ssh_command_cancel` to manage several long tasks at once.

Long-running non-interactive work can use `ssh_exec_background`. It returns the same command handle used by `ssh_command_poll` and `ssh_command_cancel`, does not occupy the session foreground, and defaults to a 24-hour timeout. Closing the session cancels its foreground and background commands.

## Workspace

- `apps/daemon`: daemon, Unix socket server, lifecycle tasks
- `apps/mcp-server`: MCP JSON-RPC stdio adapter
- `apps/desktop`: Tauri 2 tray and React/xterm.js observer
- `crates/protocol`: versioned MessagePack IPC contract
- `crates/config`: versioned TOML and permission enforcement
- `crates/ssh`: `russh` authentication, exec and PTY channels, SFTP subsystem, and the staged/verified transfer engine
- `crates/session`: concurrency, timeout, cancellation, and idle state
- `crates/storage`: SQLite WAL history, retention, and recording caps

## Icons

`apps/desktop/src-tauri/icons/icon.svg` is the source. The raster set that the bundle and the UI favicon are built from is generated, not hand-edited:

```sh
cd apps/desktop
npm run icons
```

That writes every size plus `icon.icns` and `icon.ico` into `src-tauri/icons`. Two things about the set are deliberate:

- `bundle.icon` lists `icons/128x128.png` first. Tauri derives the window and menu bar tray icon from the first `.png` in that list, and it embeds the decoded pixels in the binary, so a 128px source is crisp where it matters without carrying a megabyte of unused pixels.
- The Android and iOS outputs and the Windows Store logos that `tauri icon` can emit are not committed. This is a macOS-only app; the script deletes them so re-running it leaves a clean tree.

Re-running `npm run icons` rewrites `icon.icns` even when the artwork has not changed: the writer emits its image elements in a non-deterministic order, so the bytes differ while every embedded image is identical. Commit that file when the design changes, not on every regeneration.

## Automatic updates

The app checks for a new release shortly after launch, and asks before installing one. A check the user did not ask for stays silent about being up to date and about failing, so an offline launch never opens a dialog; **Check for Updates…** in the menu bar reports the outcome either way.

Installing downloads the update archive, verifies a minisign signature over those exact bytes, replaces the app bundle and restarts the app. The `aisshd` daemon and any SSH session it owns are separate processes, so a restart does not interrupt them.

The signature is independent of Apple code signing: it is what makes a download tamper-proof, and it is the reason the app can update itself while releases remain unsigned. The public key lives in `apps/desktop/src-tauri/tauri.conf.json`; the private key and its password are repository secrets used only when building a release.

```
TAURI_SIGNING_PRIVATE_KEY           contents of ~/.tauri/ai-ssh-updater.key
TAURI_SIGNING_PRIVATE_KEY_PASSWORD  contents of ~/.tauri/ai-ssh-updater.key.password
```

Without `TAURI_SIGNING_PRIVATE_KEY` the release workflow stops before building, because a release that cannot sign an update archive would ship an app that can never update itself.

Generate the key pair once with `scripts/generate-updater-key.sh`. It writes both halves outside the repository, prints the exact `gh secret set` commands, and refuses to overwrite an existing pair. Keep both files backed up: they cannot be regenerated, and rotating the key means every installed copy needs one manual download before it can auto-update again. The first release that contains the updater also has to be installed by hand for the same reason.

An installed app polls `https://github.com/j7ur8/AI_SSH/releases/latest/download/latest.json`, so a release is only picked up once it is the repository's latest published release and carries that file as an asset.

## Release

Releases are cut from a tag, and the tag workflow keeps the version and the tag in step.

1. Run the **Tag** workflow from the Actions tab (or `workflow_dispatch`) with the version to release, for example `0.2.0`. Use `dry_run` first to see the diff without pushing.
2. That run validates the version, confirms it is newer and the tag is free, runs the tests, bumps the version in `Cargo.toml`, `apps/desktop/package.json` and `apps/desktop/src-tauri/tauri.conf.json`, commits, creates the annotated tag, and pushes it.
3. It then invokes the **Release** workflow, which checks that the tag matches the declared version, builds the universal `.app` for `aarch64-apple-darwin` and `x86_64-apple-darwin`, verifies the bundle contents, and publishes a GitHub Release.

A release publishes the zipped app with a SHA-256 file, plus three things the updater needs: the `.app.tar.gz`, its `.sig`, and `latest.json`. `scripts/generate-update-manifest.py` writes the manifest, and the workflow then checks it against `tauri_plugin_updater::RemoteRelease` and checks the archive against the configured public key before anything is published, so a release cannot go out that installed copies would refuse to install.

Pushing a `v*` tag by hand also starts the Release workflow, so tagging locally with `git tag -a v0.2.0 && git push origin v0.2.0` works too. In that case bump the version first, or the tag check will fail on purpose.

The version is declared in three files because three toolchains read it. They must agree, and `scripts/bump-version.sh` is the only thing that should change them:

```sh
scripts/bump-version.sh --check       # fail if the three disagree
scripts/bump-version.sh --print       # the agreed version
scripts/bump-version.sh --set 0.2.0   # write it everywhere and sync Cargo.lock
```

### Why the app is unsigned

Signing and notarization need credentials tied to a specific Apple Developer ID, and the repository intentionally holds none. The Release workflow therefore publishes an unsigned app and the release notes tell the reader to open it once with Control-click > Open. To sign, add the Apple secrets to the repository and a signing step to the `build` job; Tauri reads `APPLE_CERTIFICATE`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID` from the environment.

### How the app carries its helpers

`npm run prepare:helpers` builds `aisshd` and `aissh-mcp` and stages them into `src-tauri/binaries`, which the bundle ships as a resource. A universal build stages one helper per architecture, and the app installs whichever one matches the machine it runs on, so a single download is native on both. `scripts/prepare-helpers.mjs --target universal-apple-darwin` does that staging by hand; `AISSH_HELPER_TARGET` does the same for `npm run tauri build`.

CI runs in `.github/workflows/ci.yml`: hermetic Rust checks, a job that starts a real `sshd` and runs the SFTP and MCP integration harnesses against it, and the desktop app type-check and native tests.
