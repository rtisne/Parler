import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import {
  collectAllViolations,
  collectMatrixEntries,
  collectRunsOn,
  collectStepViolations,
  formatViolations,
} from "./windows-only-ci-lib";

const workflowsDir = resolve(import.meta.dir, "../.github/workflows");

function workflowFiles(): string[] {
  return readdirSync(workflowsDir)
    .filter((name) => name.endsWith(".yml") || name.endsWith(".yaml"))
    .sort();
}

function parseWorkflow(name: string): unknown {
  return Bun.YAML.parse(
    readFileSync(resolve(workflowsDir, name), "utf8"),
  ) as unknown;
}

describe("windows-only CI structure", () => {
  const files = workflowFiles();

  test("discovers the workflow files", () => {
    expect(files.length).toBeGreaterThan(0);
  });

  for (const name of files) {
    test(`${name} contains no non-Windows runners, targets, or steps`, () => {
      const violations = collectAllViolations(name, parseWorkflow(name));
      expect(
        violations.length,
        `Non-Windows CI violations in ${name}:\n${formatViolations(violations)}`,
      ).toBe(0);
    });
  }
});

describe("windows-only CI collectors (self-test)", () => {
  test("collectors flag a planted non-Windows fixture", () => {
    const fixture = {
      on: true,
      jobs: {
        bad: {
          "runs-on": "ubuntu-latest",
          strategy: {
            matrix: {
              include: [
                {
                  platform: "macos-latest",
                  target: "aarch64-apple-darwin",
                  args: "--bundles appimage",
                },
              ],
            },
          },
          steps: [{ name: "install", run: "sudo apt-get install -y libfuse2" }],
        },
      },
    };

    expect(collectRunsOn("fixture", fixture).length).toBeGreaterThan(0);
    expect(collectMatrixEntries("fixture", fixture).length).toBeGreaterThan(0);
    expect(collectStepViolations("fixture", fixture).length).toBeGreaterThan(0);
  });

  test("collectors accept a valid Windows fixture", () => {
    const fixture = {
      on: true,
      jobs: {
        good: {
          "runs-on": "windows-latest",
          strategy: {
            matrix: {
              include: [
                {
                  platform: "windows-11-arm",
                  target: "aarch64-pc-windows-msvc",
                  args: "--target aarch64-pc-windows-msvc",
                },
              ],
            },
          },
          steps: [{ name: "build", run: "bun install --frozen-lockfile" }],
        },
        reusable: {
          uses: "./.github/workflows/build.yml",
          with: {
            platform: "${{ matrix.platform }}",
            target: "${{ matrix.target }}",
          },
        },
      },
    };

    expect(collectAllViolations("fixture", fixture)).toEqual([]);
  });
});
