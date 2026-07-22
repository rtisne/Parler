import { describe, expect, it } from "bun:test";
import {
  extractReleaseVersions,
  verifyReleaseVersions,
} from "./release-version-consistency.mjs";

const fixtures = (newline: "\n" | "\r\n") => ({
  packageJson: JSON.stringify({ version: "0.7.22" }, null, 2).replaceAll(
    "\n",
    newline,
  ),
  tauriConfig: JSON.stringify({ version: "0.7.22" }, null, 2).replaceAll(
    "\n",
    newline,
  ),
  cargoToml: ["[package]", 'name = "parler"', 'version = "0.7.22"'].join(
    newline,
  ),
  cargoLock: [
    "[[package]]",
    'name = "another-package"',
    'version = "1.0.0"',
    "",
    "[[package]]",
    'name = "parler"',
    'version = "0.7.22"',
  ].join(newline),
});

describe("release version consistency", () => {
  it.each(["\n", "\r\n"] as const)(
    "extracts the Parler version with %s line endings",
    (newline) => {
      expect(extractReleaseVersions(fixtures(newline))).toEqual({
        packageVersion: "0.7.22",
        tauriVersion: "0.7.22",
        cargoVersion: "0.7.22",
        lockVersion: "0.7.22",
      });
    },
  );

  it("rejects a mismatch", () => {
    const input = fixtures("\r\n");
    input.cargoLock = input.cargoLock.replace("0.7.22", "0.7.21");
    expect(() => verifyReleaseVersions(input)).toThrow("Version mismatch");
  });
});
