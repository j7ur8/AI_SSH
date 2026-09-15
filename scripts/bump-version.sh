#!/bin/sh
# Reads or writes the release version in every place it is declared.
#
# Usage:
#   scripts/bump-version.sh --check          report the versions and fail if they disagree
#   scripts/bump-version.sh --print          print the agreed version
#   scripts/bump-version.sh --set 0.2.0      write the version everywhere and sync Cargo.lock
#
# The version lives in three files because three toolchains consume it:
# the Cargo workspace members, the npm package, and the Tauri bundle that names
# the built .app. They must agree or a tag would describe something the built
# artifacts do not.

set -eu

PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
CARGO_TOML="${PROJECT_ROOT}/Cargo.toml"
PACKAGE_JSON="${PROJECT_ROOT}/apps/desktop/package.json"
TAURI_CONF="${PROJECT_ROOT}/apps/desktop/src-tauri/tauri.conf.json"

fail() {
  printf '%s\n' "$1" >&2
  exit 1
}

# Reads the workspace version from the [workspace.package] table, so a dependency
# version elsewhere in the file cannot be mistaken for it.
read_cargo_version() {
  awk '
    /^\[workspace\.package\]/ { in_table = 1; next }
    /^\[/ { in_table = 0 }
    in_table && /^version[[:space:]]*=/ {
      gsub(/.*=[[:space:]]*"/, "")
      gsub(/".*/, "")
      print
      exit
    }
  ' "${CARGO_TOML}"
}

read_json_version() {
  sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$1" | head -1
}

check_versions() {
  cargo_version=$(read_cargo_version)
  package_version=$(read_json_version "${PACKAGE_JSON}")
  tauri_version=$(read_json_version "${TAURI_CONF}")

  [ -n "${cargo_version}" ] || fail "cannot read the version from ${CARGO_TOML}"
  [ -n "${package_version}" ] || fail "cannot read the version from ${PACKAGE_JSON}"
  [ -n "${tauri_version}" ] || fail "cannot read the version from ${TAURI_CONF}"

  if [ "${cargo_version}" != "${package_version}" ] ||
     [ "${cargo_version}" != "${tauri_version}" ]; then
    fail "version mismatch:
  Cargo.toml ${cargo_version}
  package.json ${package_version}
  tauri.conf.json ${tauri_version}
Run 'scripts/bump-version.sh --set <version>' to make them agree."
  fi

  printf '%s\n' "${cargo_version}"
}

validate_semver() {
  case "$1" in
    *[!0-9.]* | "" | .* | *.) fail "version \"$1\" is not a plain X.Y.Z version" ;;
  esac
  [ "$(printf '%s' "$1" | awk -F. '{print NF}')" -eq 3 ] ||
    fail "version \"$1\" must have exactly three dot-separated parts"
}

# True when $1 is strictly newer than $2, comparing each numeric part.
is_newer() {
  [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)" = "$1" ]
}

set_version() {
  version="$1"

  # Cargo.toml: only the [workspace.package] version, never a dependency's.
  awk -v version="${version}" '
    /^\[workspace\.package\]/ { in_table = 1; print; next }
    /^\[/ { in_table = 0 }
    { if (in_table && /^version[[:space:]]*=/) { print "version = \"" version "\"" } else { print } }
  ' "${CARGO_TOML}" > "${CARGO_TOML}.tmp"
  mv "${CARGO_TOML}.tmp" "${CARGO_TOML}"

  # package.json and tauri.conf.json each declare exactly one version key.
  # Rewriting the first match in the file keeps the edit precise, and awk is used
  # rather than `sed '0,/re/'` because that address form is a GNU extension that
  # macOS sed silently ignores.
  for file in "${PACKAGE_JSON}" "${TAURI_CONF}"; do
    awk -v version="${version}" '
      !replaced && match($0, /"version"[[:space:]]*:[[:space:]]*"[^"]*"/) {
        $0 = substr($0, 1, RSTART - 1) "\"version\": \"" version "\"" substr($0, RSTART + RLENGTH)
        replaced = 1
      }
      { print }
      END { if (!replaced) exit 3 }
    ' "${file}" > "${file}.tmp" || fail "cannot find a version key in ${file}"
    mv "${file}.tmp" "${file}"
  done

  # Workspace member versions are recorded in the lockfile; keep it in step so a
  # release build does not rewrite it.
  (cd "${PROJECT_ROOT}" && cargo metadata --format-version 1 >/dev/null 2>&1) ||
    fail "cannot refresh Cargo.lock after changing the version"
}

case "${1:-}" in
  --check)
    version=$(check_versions)
    printf 'versions agree on %s\n' "${version}"
    ;;
  --print)
    check_versions
    ;;
  --set)
    [ $# -eq 2 ] || fail "--set needs exactly one version argument"
    target="$2"
    validate_semver "${target}"
    current=$(check_versions)
    if [ "${target}" = "${current}" ]; then
      printf 'already at %s\n' "${target}"
      exit 0
    fi
    is_newer "${target}" "${current}" ||
      fail "${target} is not newer than the current version ${current}"
    set_version "${target}"
    check_versions >/dev/null
    printf 'updated %s -> %s\n' "${current}" "${target}"
    ;;
  *)
    sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
