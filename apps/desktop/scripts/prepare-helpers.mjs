import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, mkdirSync, readdirSync, rmSync } from "node:fs";
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

function stage(triple) {
  const releaseDir = join(projectRoot, "target", triple, "release");
  for (const name of HELPERS) {
    const source = join(releaseDir, name);
    const destination = join(binariesDir, `${name}-${triple}`);
    copyFileSync(source, destination);
    chmodSync(destination, 0o700);
    console.log(`Prepared ${destination}`);
  }
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
