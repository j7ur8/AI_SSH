#!/bin/sh
set -eu

AISSH_ROOT="${HOME}/.aissh"
AISSH_BIN="${AISSH_ROOT}/bin"
AISSH_AGENT="${HOME}/Library/LaunchAgents/com.aissh.daemon.plist"
PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

cargo build --release --manifest-path "${PROJECT_ROOT}/Cargo.toml" -p aisshd -p aissh-mcp
install -d -m 700 "${AISSH_ROOT}" "${AISSH_ROOT}/keys" "${AISSH_ROOT}/data" "${AISSH_ROOT}/run" "${AISSH_BIN}" "${HOME}/Library/LaunchAgents"
install -m 700 "${PROJECT_ROOT}/target/release/aisshd" "${AISSH_BIN}/aisshd"
install -m 700 "${PROJECT_ROOT}/target/release/aissh-mcp" "${AISSH_BIN}/aissh-mcp"

if [ ! -f "${AISSH_ROOT}/config.toml" ]; then
  install -m 600 "${PROJECT_ROOT}/config.example.toml" "${AISSH_ROOT}/config.toml"
fi

sed -e "s|__DAEMON__|${AISSH_BIN}/aisshd|g" "${PROJECT_ROOT}/scripts/com.aissh.daemon.plist.in" > "${AISSH_AGENT}"
chmod 600 "${AISSH_AGENT}"
launchctl bootout "gui/$(id -u)/com.aissh.daemon" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "${AISSH_AGENT}"
launchctl enable "gui/$(id -u)/com.aissh.daemon"
printf '%s\n' "Installed aisshd and aissh-mcp in ${AISSH_BIN}"
printf '%s\n' "Edit ${AISSH_ROOT}/config.toml before using a target."
