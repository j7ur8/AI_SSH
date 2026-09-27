import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// Stages the daemon and MCP helpers into `src-tauri/binaries`, which the bundle
// ships as a resource. The desktop app installs whichever staged helper matches
// the machine it runs on, so a universal app has to carry one per architecture
// rather than a single host-architecture pair.
//
// Target selection:
//   --target <triple>            build and stage that one triple
//   AISSH_HELPER_TARGET=<triple> the same, via the environment (used by CI)
//   universal-apple-darwin       build and stage both macOS architectures
//   (unset)                      the host triple, which is what local dev wants

const desktopDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const projectRoot = resolve(desktopDir, "../..");
const binariesDir = join(desktopDir, "src-tauri", "binaries");
const HELPERS = ["aisshd", "aissh-mcp"];
const UNIVERSAL = "universal-apple-darwin";
// Windows resolves a program by its extension, so the staged helper keeps the
// `.exe` cargo built it with and the desktop app installs it under that name.
const EXECUTABLE_SUFFIX = process.platform === "win32" ? ".exe" : "";
const WEBVIEW2_LOADER = "WebView2Loader.dll";

function hostTriple() {
  const rustcInfo = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
  const host = rustcInfo.match(/^host: (.+)$/m)?.[1];
  if (!host) {
    throw new Error("Could not determine the Rust host target triple");
  }
  return host;
}

function requestedTargets() {
  const flagIndex = process.argv.indexOf("--target");
  const requested =
    flagIndex === -1 ? process.env.AISSH_HELPER_TARGET : process.argv[flagIndex + 1];

  if (flagIndex !== -1 && !requested) {
    throw new Error("--target needs a target triple, for example universal-apple-darwin");
  }
  if (!requested) {
    return [hostTriple()];
  }
  if (requested === UNIVERSAL) {
    // Each architecture gets its own helper, because the helper that ends up
    // installed runs natively on the machine that installed it.
    return ["aarch64-apple-darwin", "x86_64-apple-darwin"];
  }
  return [requested];
}

function build(triple) {
  console.log(`Building helpers for ${triple}`);
  try {
    execFileSync(
      "cargo",
      ["build", "--release", "-p", "aisshd", "-p", "aissh-mcp", "--target", triple],
      { cwd: projectRoot, stdio: "inherit" },
    );
  } catch {
    // Cargo already reported the reason on stderr; add the one hint that is not
    // obvious from its output, then fail without a Node stack trace.
    if (triple !== hostTriple()) {
      console.error(
        `\nBuilding for ${triple} failed. If that target is not installed, run:\n` +
          `  rustup target add ${triple}\n`,
      );
    }
    process.exit(1);
  }
}

// `webview2-com-sys` links WebView2Loader statically under MSVC and dynamically
// under every other toolchain, so a GNU build needs the loader to sit beside the
// installed application. `tauri-build` already puts a copy in the profile
// directory next to the binary; this stages it where the bundler reads resources
// from, which is what gets it installed beside the executable.
function stageWebView2Loader(triple) {
  if (process.platform !== "win32") {
    return;
  }
  const candidates = [
    join(projectRoot, "target", triple, "release", WEBVIEW2_LOADER),
    join(projectRoot, "target", "release", WEBVIEW2_LOADER),
  ];
  const source = candidates.find((candidate) => existsSync(candidate));
  if (!source) {
    console.warn(
      `No ${WEBVIEW2_LOADER} to stage; looked in ${candidates.join(", ")}`,
    );
    return;
  }
  const destination = join(desktopDir, "src-tauri", WEBVIEW2_LOADER);
  copyFileSync(source, destination);
  console.log(`Prepared ${destination}`);
}

function stage(triple) {
  const releaseDir = join(projectRoot, "target", triple, "release");
  for (const name of HELPERS) {
    const source = join(releaseDir, `${name}${EXECUTABLE_SUFFIX}`);
    const destination = join(binariesDir, `${name}-${triple}${EXECUTABLE_SUFFIX}`);
    copyFileSync(source, destination);
    // The mode is what stops another account from replacing a staged helper.
    // Windows has no mode; the directory helpers are installed into carries an
    // owner-only DACL instead.
    if (EXECUTABLE_SUFFIX === "") {
      chmodSync(destination, 0o700);
    }
    console.log(`Prepared ${destination}`);
  }
  stageWebView2Loader(triple);
}

// Everything here is generated, and a helper staged for a previous target would
// otherwise be bundled into this run, so start from an empty directory.
function clearStaged() {
  rmSync(binariesDir, { recursive: true, force: true });
  mkdirSync(binariesDir, { recursive: true });
}

clearStaged();

if (process.argv.includes("--help") || process.argv.includes("-h")) {
  console.log(
    [
      "Stage the aisshd and aissh-mcp helpers for a Tauri build.",
      "",
      "  node scripts/prepare-helpers.mjs [--target <triple>]",
      "  AISSH_HELPER_TARGET=<triple> npm run prepare:helpers",
      "",
      "Triples: a host default, any installed Rust triple, or universal-apple-darwin",
      "to stage one helper per macOS architecture.",
    ].join("\n"),
  );
  process.exit(0);
}

const targets = requestedTargets();
for (const triple of targets) {
  build(triple);
  stage(triple);
}
console.log(
  `Staged ${targets.length} target(s) in ${binariesDir}: ${readdirSync(binariesDir).join(", ")}`,
);
