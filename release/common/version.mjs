// Read the release version from [workspace.package] in the root Cargo.toml.
// Running this script directly prints the version.
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");

export function workspaceVersion(directory = root) {
  const manifest = readFileSync(join(directory, "Cargo.toml"), "utf8");
  const workspacePackage = manifest.split(/^[ \t]*(?=\[)/m)
    .find((section) => /^\[workspace\.package\][ \t]*(?:#[^\r\n]*)?\r?\n/.test(section));
  const version = workspacePackage?.match(/^[ \t]*version[ \t]*=[ \t]*(["'])([^"'\r\n]+)\1[ \t]*(?:#[^\r\n]*)?$/m)?.[2];
  if (!version || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error("Cargo.toml must define a semantic version in [workspace.package]");
  }
  return version;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try { console.log(workspaceVersion()); } catch (error) { console.error(error.message); process.exitCode = 1; }
}
