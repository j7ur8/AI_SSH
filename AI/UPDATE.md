<!--
AI-DOC: Newest documentation update must remain at the top.
Do not record secrets, real hosts, session identifiers, commands, or terminal output.
-->

# AI Documentation Updates

## 2026-09-15 - Automatic updates

### Project changes

- Added `tauri-plugin-updater` to the desktop app, with a check shortly after launch and a **Check for Updates…** item in the menu bar.
- The install step is confirmed through a native dialog rather than applied silently, and the decision crosses to the waiting task over a channel so the `Update` value never moves between threads. Re-checking after the prompt, which the obvious implementation does, would also reopen the window where the announced version changes between the question and the install.
- A check nobody asked for stays silent about being up to date and about failing, so an offline launch cannot open an error dialog; the reporting rule is a tested function.
- Added `apps/desktop/src-tauri/src/updates.rs` and registered the plugin. `plugins.updater` in `tauri.conf.json` carries the endpoint and public key, and `bundle.createUpdaterArtifacts` turns on the signed `.app.tar.gz`.
- The plugin's default permission set is not added to the capability file, so the webview cannot start an install; the update path is Rust-only.
- Added `scripts/generate-updater-key.sh`, which writes the minisign pair outside the repository with mode 0600, prints the `gh secret set` commands, and refuses to overwrite an existing pair.
- Added `scripts/generate-update-manifest.py`, which writes `latest.json` with both macOS platform keys pointing at one universal archive.
- Added `apps/desktop/src-tauri/tests/update_release.rs`, which parses a manifest into `tauri_plugin_updater::RemoteRelease` and verifies an archive against the configured public key using the same `minisign-verify` build the plugin resolves to. A third test asserts a tampered archive is rejected, so the verification cannot pass vacuously.
- Added a committed manifest fixture so the schema is pinned in hermetic runs; the artifact-dependent tests report that they were skipped when the environment does not name a build.
- The release workflow now requires `TAURI_SIGNING_PRIVATE_KEY`, signs the update archive, renames it to a URL-safe asset name, generates the manifest and validates both before publishing.
- Added `serde_json` to the desktop crate, which `generate_context!` needs once a plugin carries configuration.

### Verification

- Built and signed the universal app for real: `AI SSH.app.tar.gz` plus its `.sig`, with a single top-level directory in the archive as the macOS installer expects.
- Verified the real `.sig` against the public key in `tauri.conf.json` using `minisign-verify`, and confirmed the signature still matches after the asset rename the workflow performs.
- Ran the workflow's packaging step verbatim; it generated `latest.json` and passed all three release tests against real artifacts.
- Not verified: the running app's check and install flow, which needs a published release and a GUI session.

### Documentation updates

- `README.md`: added an automatic updates section covering the confirmation prompt, the signing secrets, how to generate and keep the key pair, and the fixed manifest URL; noted the outbound request in the security model and the new release assets.
- `AI/ARCHITECTURE.md`: added an updates section covering the Rust-only path, why the frontend permission is withheld, and how the asset is named.

---


## 2026-09-15 - App icon, CI, tag and release workflows

### Project changes

- Generated the full macOS icon set from `apps/desktop/src-tauri/icons/icon.svg` (PNG sizes, `icon.icns`, `icon.ico`) and added the `bundle.icon` list, which was absent. `tauri-utils` defaults that list to empty, so the built `.app` had no `.icns` and Finder and the Dock showed a generic icon.
- The icon list places `icons/128x128.png` first because Tauri derives the window and tray icon from the first `.png` and embeds its decoded pixels; the previous fallback embedded a 1024px copy.
- Skipped the Android, iOS and Windows Store icon outputs, since the app is macOS-only.
- `npm run icons` deletes the Android, iOS and Windows Store outputs `tauri icon` emits, so the command is safe to re-run. Regenerating rewrites `icon.icns` with different bytes but identical embedded images, because the icns writer orders its elements non-deterministically.
- Removed `src-tauri/binaries/.gitkeep` and ignored that directory outright. The placeholder was being copied into the shipped `.app`, which now contains exactly the four helpers. `prepare-helpers.mjs` creates the directory on every build.
- Added `apps/desktop/public/favicon.svg` and `favicon.png` and linked them from `index.html`.
- Added `npm run icons` so the generated set can be reproduced from the SVG.
- `scripts/prepare-helpers.mjs` now takes `--target <triple>` or `AISSH_HELPER_TARGET`, stages one helper per architecture for `universal-apple-darwin`, clears stale helpers first, honours `--help`, and fails without a stack trace.
- Bundled helper lookup in `apps/desktop/src-tauri/src/main.rs` is now architecture-aware and reports what it searched for. The previous single expected name could not resolve inside a universal app.
- Added `scripts/bump-version.sh` with `--check`, `--print` and `--set`, covering all three version declarations and `Cargo.lock`.
- Added `.github/workflows/ci.yml`: hermetic Rust checks, a job that runs the SFTP and MCP integration harnesses against a real `sshd` on the runner, and desktop type-check plus native tests.
- Added `.github/workflows/tag.yml`: validates a requested version, runs the tests, bumps, commits, creates an annotated tag, pushes, then invokes the release workflow.
- Added `.github/workflows/release.yml`: verifies the tag against the declared version, builds the universal `.app`, asserts the bundle contains the icns, both helper architectures and a universal main binary, then publishes the zipped app plus a SHA-256 file.
- `scripts/e2e_mcp_check.py` takes `AISSH_TEST_SSH_USER`, so the harness can run as a CI runner user instead of root.

### Documentation updates

- `README.md`: replaced the release section with the tag and release flow, the three-file version rule, why the app is unsigned, and how a universal app carries per-architecture helpers. Added an icon section covering generation and the first-PNG rule.
- `AI/ARCHITECTURE.md`: added a packaging and release section.
- `AI/CODEBASE_ANALYSIS.md`, `AI/PROJECT_OVERVIEW.md` and `AI/NAMING.md`: checked; no naming or layout rules changed.

---


## 2026-09-15 - File transfer, response shaping, reconnect

### Project changes

- Added a real SFTP transport (`russh-sftp`) to `crates/ssh` with `open_sftp`, a transport liveness check, and a new `transfer` module: local hashing, capped reads, directory creation, and staged upload/download/write helpers.
- Every write is staged beside its destination, hashed, confirmed against the remote host's own digest over an exec channel, and only then renamed into place; a mismatch fails with `TRANSFER_VERIFY_FAILED` and leaves the previous content untouched.
- Added `ssh_file_stat`, `ssh_file_read`, `ssh_file_write`, `ssh_file_upload`, `ssh_file_download` and `ssh_file_mkdir`, replacing base64-through-heredoc transfers. `ssh_file_read` and `ssh_file_write` accept `sudo: true` for root-owned paths.
- Added `ssh_commands_list` for fleet-wide command visibility alongside `ssh_command_cancel`.
- `ssh_command_poll` and `ssh_shell_read` accept `wait_seconds`; a session-scoped `watch` channel wakes a blocked poll as soon as output is recorded.
- Poll responses report `progress.seconds_since_last_output` and `progress.timed_out`, so a quiet command is distinguishable from a stalled one.
- Added `CommandStatus::Interrupted`, set when a channel ends without an exit status, plus a warning that separates "the process started and may still be running" from "whether it started is unknown". `SessionStatus::Disconnected` is now assigned, and the dead handle is discarded.
- Sessions reconnect lazily on the next operation, bounded by `reconnect_attempts` and `reconnect_backoff_seconds`.
- MCP responses gained a `detail` argument (`compact` by default): output is coalesced into `chunks`, per-event `data_base64` is dropped, the command text is replaced by a first-page-only `command_preview`, and `content[].text` carries the rendered output bounded at 8 KiB instead of a JSON re-serialization.
- Truncation is reported at all three levels in one `truncation` object: `page_truncated`, `recording_truncated` and the previously silent `live_tail_dropped`.
- `CommandInfo` carries `output_bytes`, and `PROTOCOL_VERSION` moved to 3.
- Measured against a local OpenSSH server: a 338-line hash listing is 36.7 KB in compact mode versus 103.2 KB in `full`.

### Verification

- 87 hermetic unit tests across the workspace, up from 31.
- 10 ignored live SFTP tests in `crates/ssh/tests/sftp_live.rs` cover the subsystem handshake, binary round trips verified by remote digest, overwrite, truncation, a forced digest mismatch that must not reach the destination, and the absence of staged leftovers.
- `scripts/e2e_mcp_check.py` runs the real daemon and MCP server against a local SSH server under an isolated `HOME`; 47 checks pass, including an end-to-end tarball upload whose digest is confirmed by the remote host.

### Documentation updates

- `README.md`: added a file-transfer section, the waiting and response-shaping contract, the three truncation levels, drop/reconnect semantics, and the local-path trust-model note.
- `AI/ARCHITECTURE.md`: updated the module table to 21 tools, protocol version 3, SFTP/staging, blocking polls and lazy reconnect.
- `AI/CODEBASE_ANALYSIS.md`: recorded the new error codes and the retryable-flag ownership.
- `config.example.toml`: documented `reconnect_attempts` and `reconnect_backoff_seconds`.

---


## 2026-09-01 - Incremental sync

### Project changes

- Reworked `apps/desktop/src/main.tsx` and `apps/desktop/src/styles.css` into a two-pane session workspace with list-local scrolling and an embedded read-only command transcript.
- Updated `apps/desktop/src-tauri/src/main.rs` so tray session actions select a session in the main Webview instead of creating per-session windows.
- Added dynamic macOS activation policy handling: showing the main UI exposes AI SSH in the Dock, while closing it hides the window and returns the resident tray app to accessory mode.
- Limited the default Tauri capability to the single `main` window.

### Documentation updates

- `AI/ARCHITECTURE.md`: documented the single-Webview session selection flow and dynamic Dock presence.
- `AI/CODEBASE_ANALYSIS.md`: documented the embedded session workflow, local list scrolling, Tauri event bridge and packaged-app verification.
- `AI/PROJECT_OVERVIEW.md` and `AI/NAMING.md`: checked; no project structure, setup or naming rules changed.

---

## 2026-09-01 - Incremental sync

### Project changes

- Updated `crates/storage/src/lib.rs` with a single-session SQLite lookup.
- Updated `crates/session/src/lib.rs` so historical status and event reads fall back to SQLite when no live runtime exists.
- Added a daemon-restart regression covering persisted status, command metadata, terminal events, and missing-session errors.
- Rebuilt and installed both local helpers, restarted the daemon with no active sessions, and verified a persisted historical session through MCP without exposing recorded content.

### Documentation updates

- `AI/ARCHITECTURE.md`: documented live-runtime and historical SQLite read paths.
- `AI/CODEBASE_ANALYSIS.md`: added the historical trace flow and updated the passing test count to 31.
- `AI/TEST_REPORT.md`: recorded that historical lookup is fixed while active closed-session retention remains open.
- `AI/PROJECT_OVERVIEW.md` and `AI/NAMING.md`: checked; no project layout or naming facts changed.

---

## 2026-09-01 - Initial analysis

- Generated `ARCHITECTURE.md`, `CODEBASE_ANALYSIS.md`, `PROJECT_OVERVIEW.md`, `NAMING.md`, and `TEST_REPORT.md` from the current worktree.
- Scanned 40 tracked files and 10 Rust/TypeScript source files across 9 implementation modules.
- Verified 14 public MCP tools, 30 Rust tests, the desktop production build, live daemon calls, SQLite integrity, and a 519-event historical pagination flow.
- Recorded 5 confirmed issues and 4 explicit coverage gaps; all runtime evidence was redacted.
- Existing non-`AI/` working-tree changes were checked and documented; no project source was modified by this review.

---
