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
          steps: [{ name: "build", run: "bun install --frozen-lockfile" }],
        },
        reusable: {
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

  test("collectors reject non-Windows matrix axes feeding reusable inputs", () => {
    const fixture = {
      jobs: {
        reusable: {
          strategy: {
            matrix: {
              platform: ["ubuntu-latest"],
              target: ["x86_64-unknown-linux-gnu"],
            },
          },
          uses: "./.github/workflows/build.yml",
          with: {
            platform: "${{ matrix.platform }}",
            target: "${{ matrix.target }}",
          },
        },
      },
    };

    expect(collectAllViolations("fixture", fixture).length).toBeGreaterThan(0);
  });

  test("only build.yml may resolve runs-on from inputs.platform", () => {
    const fixture = {
      jobs: { build: { "runs-on": "${{ inputs.platform }}" } },
    };

    expect(collectRunsOn("other.yml", fixture).length).toBeGreaterThan(0);
    expect(collectRunsOn("build.yml", fixture)).toEqual([]);
  });

  test("rejects unaudited job-level reusable workflow calls", () => {
    const fixture = {
      jobs: {
        remote: {
          uses: "acme/project/.github/workflows/linux.yml@v1",
        },
      },
    };

    expect(collectAllViolations("fixture", fixture).length).toBeGreaterThan(0);
  });
});

describe("required Windows CI topology", () => {
  const expectedMatrices: Record<string, Array<[string, string, string]>> = {
    "build-test.yml": [
      ["windows-latest", "x86_64-pc-windows-msvc", ""],
      [
        "windows-11-arm",
        "aarch64-pc-windows-msvc",
        "--target aarch64-pc-windows-msvc",
      ],
    ],
    "pr-test-build.yml": [
      ["windows-latest", "x86_64-pc-windows-msvc", ""],
      [
        "windows-11-arm",
        "aarch64-pc-windows-msvc",
        "--target aarch64-pc-windows-msvc",
      ],
    ],
    "build-windows.yml": [
      ["windows-latest", "x86_64-pc-windows-msvc", "--bundles nsis,msi"],
      [
        "windows-11-arm",
        "aarch64-pc-windows-msvc",
        "--target aarch64-pc-windows-msvc --bundles nsis,msi",
      ],
    ],
    "release.yml": [
      ["windows-latest", "x86_64-pc-windows-msvc", "--bundles nsis,msi"],
      ["windows-11-arm", "aarch64-pc-windows-msvc", "--bundles nsis,msi"],
    ],
  };

  test("discovers exactly the audited build.yml callers", () => {
    const callers = workflowFiles()
      .filter((name) => {
        const workflow = parseWorkflow(name) as any;
        return Object.values(workflow.jobs ?? {}).some(
          (job: any) => job.uses === "./.github/workflows/build.yml",
        );
      })
      .sort();

    expect(callers).toEqual(Object.keys(expectedMatrices).sort());
  });

  for (const [name, expected] of Object.entries(expectedMatrices)) {
    test(`${name} keeps exactly the native x64 and ARM64 matrix`, () => {
      const workflow = parseWorkflow(name) as any;
      const reusableJobs = Object.values(workflow.jobs).filter(
        (job: any) => job.uses === "./.github/workflows/build.yml",
      ) as any[];
      expect(reusableJobs).toHaveLength(1);
      const matrix = reusableJobs[0].strategy.matrix;
      expect(Object.keys(matrix).sort()).toEqual(["include"]);
      const include = matrix.include;
      expect(
        include.map((entry: any) => [entry.platform, entry.target, entry.args]),
      ).toEqual(expected);
    });
  }

  test("lint enforces the structural CI test", () => {
    const lint = parseWorkflow("lint.yml") as any;
    const commands = lint.jobs.lint.steps.map((step: any) => step.run ?? "");
    expect(
      commands.some((command: string) =>
        command.includes("bun run test:ci-structure"),
      ),
    ).toBe(true);
  });
});
