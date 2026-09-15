//! Validates the release artifacts an update actually depends on.
//!
//! A release can look complete and still be un-updatable: the manifest can use
//! the wrong field names, or the archive can be signed with a key that does not
//! match the public key baked into the app. Both failures only surface in the
//! field, on a user's machine, at update time. These tests check the same
//! contracts the updater plugin checks, using its own deserializer and the same
//! `minisign-verify` crate, so the release workflow can catch either one before
//! publishing.
//!
//! Two inputs come from the environment, because they only exist after a build:
//!
//!   AISSH_UPDATE_MANIFEST   path to a generated latest.json
//!   AISSH_UPDATE_ARCHIVE    path to the built .app.tar.gz
//!   AISSH_UPDATE_SIGNATURE  path to its .sig
//!
//! Without them the manifest test falls back to the committed fixture, which
//! still pins the schema, and the signature test reports that it was skipped.

use std::{fs, path::Path, path::PathBuf};

/// Must match the `platforms` keys the generator writes.
const MACOS_PLATFORMS: [&str; 2] = ["darwin-aarch64", "darwin-x86_64"];

fn repository_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/apps/desktop/src-tauri, so the repository root
    // is three levels up.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("repository root")
        .to_path_buf()
}

fn manifest_path() -> PathBuf {
    env_path("AISSH_UPDATE_MANIFEST")
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/latest.json"))
}

/// Reads a path from the environment, resolving a relative one against the
/// repository root.
///
/// `cargo test` runs with the package directory as the working directory, so a
/// repo-relative path such as `target/.../AI SSH.app.tar.gz` would otherwise not
/// be found.
fn env_path(key: &str) -> Option<PathBuf> {
    let value = std::env::var_os(key)?;
    let path = PathBuf::from(value);
    Some(if path.is_absolute() {
        path
    } else {
        repository_root().join(path)
    })
}

/// The public key the app will verify updates with, read from the config so the
/// test cannot pass against a key that is not the one actually shipped.
fn configured_public_key() -> String {
    let config = repository_root().join("apps/desktop/src-tauri/tauri.conf.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config).expect("read tauri.conf.json"))
            .expect("tauri.conf.json is valid JSON");
    parsed["plugins"]["updater"]["pubkey"]
        .as_str()
        .expect("plugins.updater.pubkey is configured")
        .to_owned()
}

/// The plugin parses this section when it initializes, and a section it rejects
/// stops the app from starting at all, so it is worth checking without launching
/// the app.
#[test]
fn the_configured_updater_section_is_accepted_by_the_plugin() {
    let config = repository_root().join("apps/desktop/src-tauri/tauri.conf.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config).expect("read tauri.conf.json"))
            .expect("tauri.conf.json is valid JSON");
    let section = &parsed["plugins"]["updater"];
    assert!(
        !section.is_null(),
        "plugins.updater is missing from tauri.conf.json"
    );

    let updater: tauri_plugin_updater::Config = serde_json::from_value(section.clone())
        .unwrap_or_else(|error| panic!("the plugin would reject this configuration: {error}"));

    assert!(
        !updater.endpoints.is_empty(),
        "the plugin fails to build without an endpoint"
    );
    for endpoint in &updater.endpoints {
        assert_eq!(
            endpoint.scheme(),
            "https",
            "a release build refuses a non-https endpoint and the app would not start"
        );
    }
    assert!(
        !updater.pubkey.is_empty(),
        "without a public key every update download is refused"
    );
}

#[test]
fn the_manifest_parses_into_the_updaters_own_schema() {
    let path = manifest_path();
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));

    // Deserializing with the plugin's type is the point: field names, the
    // semver version, the RFC3339 pub_date and the platform map all have to
    // match what the updater expects, or it rejects the manifest at update time.
    let release: tauri_plugin_updater::RemoteRelease =
        serde_json::from_str(&raw).unwrap_or_else(|error| {
            panic!(
                "{} is not a valid updater manifest: {error}",
                path.display()
            )
        });

    assert!(
        !release.version.to_string().is_empty(),
        "the manifest must name the version it publishes"
    );

    for platform in MACOS_PLATFORMS {
        let url = release
            .download_url(platform)
            .unwrap_or_else(|error| panic!("{platform} is missing from the manifest: {error}"));
        assert_eq!(url.scheme(), "https", "{platform} must download over https");
        assert!(
            url.path().ends_with(".app.tar.gz"),
            "{platform} must point at the updater archive, got {url}"
        );

        let signature = release
            .signature(platform)
            .unwrap_or_else(|error| panic!("{platform} has no signature: {error}"));
        assert!(
            !signature.is_empty(),
            "{platform} needs a signature or the download is refused"
        );
    }

    // Both architectures have to be present, because one universal download
    // serves either of them.
    assert_eq!(
        release.signature(MACOS_PLATFORMS[0]).unwrap().to_owned(),
        release.signature(MACOS_PLATFORMS[1]).unwrap().to_owned(),
        "a universal build signs one archive, so both platforms share its signature"
    );
}

#[test]
fn the_archive_signature_matches_the_configured_public_key() {
    let (Some(archive), Some(signature)) = (
        env_path("AISSH_UPDATE_ARCHIVE"),
        env_path("AISSH_UPDATE_SIGNATURE"),
    ) else {
        eprintln!(
            "skipping: set AISSH_UPDATE_ARCHIVE and AISSH_UPDATE_SIGNATURE to verify a built release"
        );
        return;
    };

    let bytes =
        fs::read(&archive).unwrap_or_else(|e| panic!("cannot read {}: {e}", archive.display()));
    let signature_text = fs::read_to_string(&signature)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", signature.display()));
    let public_key = configured_public_key();

    // Mirrors tauri-plugin-updater's own verification step exactly: the config
    // value and the .sig file are both base64 around a minisign document.
    let decoded_key = decode_base64(&public_key, "plugins.updater.pubkey");
    let key =
        minisign_verify::PublicKey::decode(&decoded_key).expect("configured public key parses");
    let decoded_signature = decode_base64(signature_text.trim(), "the .sig file");
    let parsed_signature =
        minisign_verify::Signature::decode(&decoded_signature).expect("the signature parses");

    key.verify(&bytes, &parsed_signature, true).expect(
        "the updater must accept this archive: signature does not match the configured key",
    );
}

#[test]
fn a_tampered_archive_is_rejected() {
    let (Some(archive), Some(signature)) = (
        env_path("AISSH_UPDATE_ARCHIVE"),
        env_path("AISSH_UPDATE_SIGNATURE"),
    ) else {
        eprintln!(
            "skipping: set AISSH_UPDATE_ARCHIVE and AISSH_UPDATE_SIGNATURE to verify a built release"
        );
        return;
    };

    // Without this, the test above would pass even if verification were not
    // actually happening.
    let mut bytes = fs::read(&archive).expect("read the archive");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;

    let public_key = configured_public_key();
    let key = minisign_verify::PublicKey::decode(&decode_base64(&public_key, "pubkey"))
        .expect("public key parses");
    let parsed_signature = minisign_verify::Signature::decode(&decode_base64(
        fs::read_to_string(&signature)
            .expect("read the signature")
            .trim(),
        "signature",
    ))
    .expect("signature parses");

    assert!(
        key.verify(&bytes, &parsed_signature, true).is_err(),
        "a modified archive must not verify"
    );
}

fn decode_base64(value: &str, what: &str) -> String {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value)
        .unwrap_or_else(|error| panic!("{what} is not valid base64: {error}"));
    String::from_utf8(decoded).unwrap_or_else(|error| panic!("{what} is not UTF-8: {error}"))
}
