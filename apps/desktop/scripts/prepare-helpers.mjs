import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const desktopDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const projectRoot = resolve(desktopDir, "../..");
const binariesDir = join(desktopDir, "src-tauri", "binaries");

execFileSync(
  "cargo",
  ["build", "--release", "-p", "aisshd", "-p", "aissh-mcp"],
  { cwd: projectRoot, stdio: "inherit" },
);

const rustcInfo = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
const host = rustcInfo.match(/^host: (.+)$/m)?.[1];
if (!host) {
  throw new Error("Could not determine the Rust host target triple");
}

mkdirSync(binariesDir, { recursive: true });
for (const name of ["aisshd", "aissh-mcp"]) {
  const source = join(projectRoot, "target", "release", name);
  const destination = join(binariesDir, `${name}-${host}`);
  copyFileSync(source, destination);
  chmodSync(destination, 0o700);
  console.log(`Prepared ${destination}`);
}
