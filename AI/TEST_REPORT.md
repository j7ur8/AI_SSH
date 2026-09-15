<!--
AI-DOC: MCP test report for the current worktree on 2026-09-01.
Evidence is deliberately redacted; do not add real target, session, command, or output data.
-->

# AI SSH MCP Test Report

## Scope

The review covered the current dirty worktree, all 40 tracked files, 10 Rust/TypeScript source files, 14 MCP tools, internal daemon IPC, persistence, session lifecycle and the installed local runtime. Existing user changes were preserved.

## Passing Evidence

| Check | Result |
| --- | --- |
| Rust format | Passed |
| Rust workspace tests | 31 passed, 0 failed |
| TypeScript and Vite production build | Passed; one chunk-size warning |
| Installed MCP helper | 21 tools exposed; daemon connection succeeded |
| Credentials in target-list result | No password/passphrase field observed |
| Malformed JSON | Correct JSON-RPC parse error `-32700` |
| Unknown method | Correct JSON-RPC method error `-32601` |
| Missing target/command | Stable `TARGET_NOT_FOUND` / `COMMAND_NOT_FOUND` tool errors |
| Historical event pagination | 519/519 events over 61 pages, monotonic cursor, final completion true |
| Historical session after daemon restart | Persisted status, 3 command records and 3 events returned through the installed MCP helper |
| SQLite integrity | `integrity_check` passed; no foreign-key violations |

## Findings

### F1 High: Unsupported MCP Versions Are Accepted

`apps/mcp-server/src/main.rs:56-63` returns the client-supplied `protocolVersion` without checking a supported-version set. A probe using `2099-01-01` received a successful initialize response echoing `2099-01-01`.

The MCP lifecycle specification requires the server to return the requested version only when supported; otherwise it must return another version it supports. A client can therefore believe it negotiated features this server does not implement.

Recommended fix: define supported MCP revisions, select the requested revision only on an exact match, otherwise return the newest supported revision, and add initialize negotiation tests.

### F2 Medium: Closed Sessions Remain In The Active Result

`crates/session/src/lib.rs:79-97` treats every runtime in the in-memory map as active. `close` at lines 185-207 changes status but never removes the runtime. `reap_idle` at lines 600-617 repeatedly calls `close` without excluding terminal states.

Live evidence: `ssh_sessions_list(include_history=false)` returned 3 sessions and all 3 had status `closed`; `include_history=true` returned 98 historical entries. This also retains each runtime and any live overflow queue until daemon restart.

Recommended fix: remove terminal runtimes from the active map after closing, or explicitly filter terminal states and make the reaper skip them. Preserve status lookup requirements through SQLite rather than indefinitely retaining runtime objects.

Update: persisted status and event lookup now works without a runtime, so historical sessions remain traceable after daemon restart. The separate active-list filtering and runtime-retention problem described above remains open.

### F3 Medium: Published Tool Schemas Are Not Enforced

The schemas at `apps/mcp-server/src/main.rs:369-447` declare types, required fields, ranges and `additionalProperties: false`, but conversion at lines 129-207 silently defaults several invalid values and ignores unknown fields.

A probe sent `include_history: "yes"` plus an unexpected property to `ssh_sessions_list`; the call succeeded. Similar fallback behavior exists for invalid `after_sequence`, `max_bytes`, `cols`, `rows` and optional timeouts. Clients therefore receive behavior different from the advertised contract, and caller mistakes are hidden.

Recommended fix: validate `arguments` against each input schema or use typed deserialization with `deny_unknown_fields` plus explicit numeric range checks. Return `INVALID_ARGUMENT` for every mismatch.

### F4 Medium: Crash Recovery Does Not Finish Running Commands

`crates/storage/src/lib.rs:42-48` changes unfinished session rows to `interrupted` during daemon startup but does not update commands whose status is `running`. After an unclean daemon stop, `ssh_command_poll` can therefore report a command as running forever while `ssh_command_cancel` cannot find a live task.

The current database did not contain a stale running command, so this finding is source-confirmed rather than reproduced against live state.

Recommended fix: in one startup transaction, mark running commands `cancelled` or add an `interrupted` command status, set `finished_at`, then interrupt their sessions. Add a restart recovery test.

### F5 Low: JSON-RPC Version Is Not Validated

The MCP loop parses arbitrary JSON and dispatches solely on `id` and `method` (`apps/mcp-server/src/main.rs:37-55`). A request declaring `"jsonrpc":"1.0"` received a normal JSON-RPC 2.0 ping response.

Recommended fix: reject non-object requests and any request whose `jsonrpc` member is not exactly `"2.0"` with `-32600 Invalid Request`.

## Coverage Gaps

- No target is currently configured, so session creation, fresh exec, background execution, cancellation and PTY behavior were not exercised against a remote SSH server.
- Clippy was unavailable because its stable toolchain component is not installed.
- There are no automated stdio lifecycle tests, daemon restart recovery tests, or full MCP-to-daemon-to-SSH integration tests.
- Host-key verification is intentionally disabled and was treated as documented product behavior, not a newly discovered defect.

## Suggested Fix Order

1. Correct MCP version negotiation (F1) because it can break all interoperability with future clients.
2. Fix terminal session removal/filtering and crash recovery (F2, F4).
3. Centralize argument validation (F3).
4. Tighten JSON-RPC envelope validation (F5).
5. Add a disposable local SSH fixture for full foreground/background/PTY integration coverage.
