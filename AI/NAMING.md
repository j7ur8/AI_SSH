<!--
AI-DOC: Observed naming conventions, not speculative style guidance.
Update when public protocol, workspace layout, or frontend conventions change.
-->

# Naming Conventions

## Rust

- Packages and crate imports use kebab-case package names and `aissh_*` snake_case crate names, for example `aissh-mcp` and `aissh_protocol`.
- Types and enum variants use PascalCase: `SessionManager`, `ResponseData`, `ExecBackground`.
- Functions, modules, fields and locals use snake_case: `command_poll`, `after_sequence`.
- Stable external error codes use uppercase snake case: `SESSION_BUSY`, `COMMAND_NOT_FOUND`.
- Protocol enums serialize with snake_case through Serde.

## Protocol And API

- Public MCP tools use an `ssh_<domain>_<action>` form, such as `ssh_session_create` and `ssh_command_poll`.
- MCP argument names are snake_case and align with protocol fields.
- Internal daemon request variants use noun/action PascalCase without the public `ssh_` prefix.
- Boolean fields use affirmative predicates such as `include_history`, `has_more`, `poll_complete`, `recording_truncated`.
- IDs carry an entity suffix: `target_id`, `session_id`, `command_id`, `request_id`.

## Frontend

- React components and TypeScript types use PascalCase.
- Functions, hooks and values use camelCase.
- Tauri command names remain snake_case because they mirror Rust function names; React invocation arguments use camelCase at the bridge boundary.
- CSS classes use kebab-case, such as `session-page` and `target-list-header`.

## Configuration And Resources

- TOML fields use snake_case.
- Managed executables are `aisshd` and `aissh-mcp`.
- Tauri sidecar resources append the Rust target triple to the executable name.
- Database tables and columns use plural snake_case table names and snake_case columns.

## Allowed Exceptions

- `AI SSH` is the user-facing product name; `aissh` is used for binary, package and filesystem identifiers.
- JSON-RPC and MCP-defined names such as `tools/list`, `tools/call`, `structuredContent` and `isError` retain specification casing.
- Existing Tauri-generated schema files retain upstream names and formatting.

## Migration Policy

Public tool names, serialized enum values, database columns and config fields are compatibility surfaces. Rename them only with an explicit protocol/config/schema migration and corresponding tests. Internal Rust or React identifiers may be renamed mechanically when behavior and serialized forms remain unchanged.

