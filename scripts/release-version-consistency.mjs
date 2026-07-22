import fs from "node:fs";
import { fileURLToPath } from "node:url";

const normalizeNewlines = (value) => value.replace(/\r\n?/g, "\n");

export function extractReleaseVersions({
  packageJson,
  tauriConfig,
  cargoToml,
  cargoLock,
}) {
  const normalizedCargo = normalizeNewlines(cargoToml);
  const normalizedLock = normalizeNewlines(cargoLock);

  return {
    packageVersion: JSON.parse(packageJson).version,
    tauriVersion: JSON.parse(tauriConfig).version,
    cargoVersion: normalizedCargo.match(/^version = "([^"]+)"$/m)?.[1],
    lockVersion: normalizedLock.match(
      /\[\[package\]\]\nname = "parler"\nversion = "([^"]+)"/,
    )?.[1],
  };
}

export function verifyReleaseVersions(input, expectedVersion) {
  const versions = extractReleaseVersions(input);
  const expected = expectedVersion ?? versions.tauriVersion;

  if (
    !expected ||
    versions.tauriVersion !== expected ||
    versions.packageVersion !== expected ||
    versions.cargoVersion !== expected ||
    versions.lockVersion !== expected
  ) {
    throw new Error(
      `Version mismatch: tauri=${versions.tauriVersion}, package=${versions.packageVersion}, cargo=${versions.cargoVersion}, lock=${versions.lockVersion}, expected=${expected}`,
    );
  }

  return expected;
}

function main() {
  const version = verifyReleaseVersions(
    {
      packageJson: fs.readFileSync("package.json", "utf8"),
      tauriConfig: fs.readFileSync("src-tauri/tauri.conf.json", "utf8"),
      cargoToml: fs.readFileSync("src-tauri/Cargo.toml", "utf8"),
      cargoLock: fs.readFileSync("src-tauri/Cargo.lock", "utf8"),
    },
    process.env.EXPECTED_VERSION,
  );
  console.log(`Version consistency verified: ${version}`);
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  main();
}
