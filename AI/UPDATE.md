<!--
AI-DOC: Newest documentation update must remain at the top.
Do not record secrets, real hosts, session identifiers, commands, or terminal output.
-->

# AI Documentation Updates

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
