#!/bin/sh
# Generates the minisign key pair that signs update archives.
#
# The updater refuses an archive whose signature does not match the public key
# compiled into the app, so losing this key means every installed copy can no
# longer update itself and a new public key has to ship in a manual download.
# Keep the private key and its password backed up somewhere durable.
#
# The private half never enters the repository. Only the public key does, and
# this script prints exactly what to do with each half.
#
# Usage:
#   scripts/generate-updater-key.sh                  write the pair to ~/.tauri
#   AISSH_UPDATER_KEY_DIR=/path scripts/generate-updater-key.sh
#
# Rotating the key requires shipping a release built with the new public key
# before any update signed by it can be installed.

set -eu

PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TAURI_BIN="${PROJECT_ROOT}/apps/desktop/node_modules/.bin/tauri"
KEY_DIR="${AISSH_UPDATER_KEY_DIR:-${HOME}/.tauri}"
KEY="${KEY_DIR}/ai-ssh-updater.key"
PASSWORD_FILE="${KEY_DIR}/ai-ssh-updater.key.password"
PUBLIC_KEY="${KEY}.pub"
CONFIG="${PROJECT_ROOT}/apps/desktop/src-tauri/tauri.conf.json"

if [ ! -x "${TAURI_BIN}" ]; then
  printf '%s\n' "cannot find the Tauri CLI at ${TAURI_BIN}" >&2
  printf '%s\n' "run 'npm install' in apps/desktop first" >&2
  exit 1
fi

if [ -e "${KEY}" ] || [ -e "${PUBLIC_KEY}" ]; then
  printf '%s\n' "a key pair already exists at ${KEY}" >&2
  printf '%s\n' "remove it deliberately if you really mean to rotate; every installed" >&2
  printf '%s\n' "copy will need a manual download before it can auto-update again." >&2
  exit 1
fi

umask 077
mkdir -p "${KEY_DIR}"
chmod 700 "${KEY_DIR}"

PASSWORD=$(python3 -c 'import secrets; print(secrets.token_hex(24))')
printf '%s' "${PASSWORD}" > "${PASSWORD_FILE}"
chmod 600 "${PASSWORD_FILE}"

# The CLI prints the private key, so its output is not echoed anywhere.
"${TAURI_BIN}" signer generate -w "${KEY}" -p "${PASSWORD}" --ci >/dev/null 2>&1

if [ ! -s "${KEY}" ] || [ ! -s "${PUBLIC_KEY}" ]; then
  printf '%s\n' "key generation failed; no key pair was written" >&2
  exit 1
fi
chmod 600 "${KEY}" "${PUBLIC_KEY}"

printf '%s\n' "Wrote a key pair:"
printf '  private key  %s\n' "${KEY}"
printf '  password     %s\n' "${PASSWORD_FILE}"
printf '  public key   %s\n' "${PUBLIC_KEY}"
printf '\n%s\n' "1. Put the public key in ${CONFIG}:"
printf '%s\n' "     \"plugins\": { \"updater\": { \"pubkey\": \"<contents of ${PUBLIC_KEY}>\" } }"
printf '\n%s\n' "2. Add both halves as repository secrets:"
printf '     gh secret set TAURI_SIGNING_PRIVATE_KEY < "%s"\n' "${KEY}"
printf '     gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD < "%s"\n' "${PASSWORD_FILE}"
printf '\n%s\n' "3. Back both files up. Without them you cannot sign another update."
printf '%s\n' "   The password is not recoverable from the key file."
