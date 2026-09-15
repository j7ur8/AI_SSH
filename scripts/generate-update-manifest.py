#!/usr/bin/env python3
"""Builds the updater manifest that a release publishes as `latest.json`.

The Tauri updater fetches this document from the endpoint configured in
`tauri.conf.json`, picks the entry for the platform it is running on, and
installs the archive that entry names only after checking a minisign signature
over those exact bytes. Both macOS platform keys point at the same universal
archive, because one download has to work on either architecture.

The field names and shapes are not ours to choose: they are parsed into
`tauri_plugin_updater::RemoteRelease`, and a manifest that does not match is
rejected at update time. `cargo test -p ai-ssh-desktop` validates the output of
this script against that type.
"""

import argparse
import datetime
import json
import pathlib
import sys

# The keys the updater looks up once it has resolved its own target triple.
PLATFORMS = ("darwin-aarch64", "darwin-x86_64")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="release version, without a leading v")
    parser.add_argument("--tag", required=True, help="release tag, for example v0.2.0")
    parser.add_argument("--repository", required=True, help="owner/name on GitHub")
    parser.add_argument(
        "--archive",
        required=True,
        help="release asset file name of the .app.tar.gz, as it will be uploaded",
    )
    parser.add_argument("--signature", required=True, help="path to the .sig produced by tauri signer")
    parser.add_argument("--output", required=True, help="path to write the manifest to")
    args = parser.parse_args()

    if args.version.startswith("v") or args.tag != f"v{args.version}":
        parser.error(f"tag {args.tag!r} does not match version {args.version!r}")

    signature_path = pathlib.Path(args.signature)
    if not signature_path.is_file():
        parser.error(f"no signature at {signature_path}")

    signature = signature_path.read_text().strip()
    if not signature:
        parser.error(f"signature at {signature_path} is empty")

    url = f"https://github.com/{args.repository}/releases/download/{args.tag}/{args.archive}"
    if " " in args.archive:
        # A space in an asset name is rewritten by GitHub in some download URLs,
        # which would make the manifest point somewhere that does not exist.
        parser.error(f"archive name {args.archive!r} must be URL safe; rename it before uploading")

    manifest = {
        "version": args.version,
        "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": {
            platform: {"url": url, "signature": signature} for platform in PLATFORMS
        },
    }

    output = pathlib.Path(args.output)
    output.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"wrote {output} for {args.version}")
    print(json.dumps(manifest, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
