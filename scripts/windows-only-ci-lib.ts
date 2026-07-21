// Pure structural validators for the "Windows-only CI" invariant.
//
// These functions operate on already-parsed workflow objects (via
// `Bun.YAML.parse`) rather than raw file text, so comments can never trigger a
// violation. Each collector returns the list of violations it found; the caller
// (windows-only-ci.test.ts) asserts that list is empty.

export interface WorkflowViolation {
  workflow: string;
  location: string;
  value: string;
  reason: string;
}

// A parsed workflow is an arbitrary YAML object graph. We narrow lazily.
type Json = unknown;

const PLATFORM_EXPRESSION = "${{ inputs.platform }}";
const WINDOWS_RUNNER = /^windows-/;
const WINDOWS_TARGET = /-pc-windows-msvc$/;
const FORBIDDEN_ARGS = /deb|appimage|rpm|dmg|apple|darwin|linux/i;

// Tokens that must never appear in a parsed step value (run/if/env/with).
const FORBIDDEN_STEP_TOKENS = [
  "apt-get",
  "ubuntu",
  "macos",
  "APPLE_CERTIFICATE",
  "apple-darwin",
  "unknown-linux-gnu",
  "appimage",
  ".deb",
  ".rpm",
  ".dmg",
  "fuse",
  "setup-ubuntu",
  "setup-macos",
  "docker://",
  "dnf install",
  "apk add",
  "brew install",
];

function workflowBasename(workflow: string): string {
  return workflow.replaceAll("\\", "/").split("/").at(-1) ?? workflow;
}

function isRecord(value: Json): value is Record<string, Json> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isExpression(value: string): boolean {
  return value.includes("${{");
}

// Only ever walk `jobs`; the top-level `on:` key can parse to a non-object and
// carries no runner information.
function jobsOf(doc: Json): Record<string, Json> {
  if (isRecord(doc) && isRecord(doc.jobs)) {
    return doc.jobs;
  }
  return {};
}

function jobEntries(doc: Json): Array<[string, Record<string, Json>]> {
  return Object.entries(jobsOf(doc)).filter(
    (entry): entry is [string, Record<string, Json>] => isRecord(entry[1]),
  );
}

/**
 * `jobs.*['runs-on']` must be a Windows runner, or the single sanctioned
 * expression `${{ inputs.platform }}` (only build.yml uses it — its callers'
 * matrices are validated separately). Arrays are violations unless every
 * element is a Windows runner.
 */
export function collectRunsOn(
  workflow: string,
  doc: Json,
): WorkflowViolation[] {
  const violations: WorkflowViolation[] = [];
  for (const [jobName, job] of jobEntries(doc)) {
    if (!("runs-on" in job)) {
      continue;
    }
    const runsOn = job["runs-on"];
    const location = `jobs.${jobName}.runs-on`;
    const values = Array.isArray(runsOn) ? runsOn : [runsOn];
    for (const value of values) {
      if (typeof value !== "string") {
        violations.push({
          workflow,
          location,
          value: JSON.stringify(value),
          reason: "runs-on must be a Windows runner string",
        });
        continue;
      }
      if (
        WINDOWS_RUNNER.test(value) ||
        (value === PLATFORM_EXPRESSION &&
          workflowBasename(workflow) === "build.yml")
      ) {
        continue;
      }
      violations.push({
        workflow,
        location,
        value,
        reason: "runs-on is not a Windows runner",
      });
    }
  }
  return violations;
}

/**
 * `jobs.*.strategy.matrix.include[]`: `platform` must be a Windows runner,
 * `target` (if present) must be a Windows MSVC triple, and `args` must not
 * reference any non-Windows bundle/target.
 */
export function collectMatrixEntries(
  workflow: string,
  doc: Json,
): WorkflowViolation[] {
  const violations: WorkflowViolation[] = [];
  for (const [jobName, job] of jobEntries(doc)) {
    const strategy = job.strategy;
    if (!isRecord(strategy)) continue;
    const matrix = strategy.matrix;
    if (!isRecord(matrix)) continue;
    for (const [axis, validator, reason] of [
      ["platform", WINDOWS_RUNNER, "matrix platform is not a Windows runner"],
      ["target", WINDOWS_TARGET, "matrix target is not a Windows MSVC triple"],
    ] as const) {
      const values = matrix[axis];
      if (values === undefined) continue;
      if (!Array.isArray(values) || values.length === 0) {
        violations.push({
          workflow,
          location: `jobs.${jobName}.strategy.matrix.${axis}`,
          value: JSON.stringify(values),
          reason: `matrix ${axis} axis must be a non-empty array`,
        });
        continue;
      }
      values.forEach((value, index) => {
        if (typeof value !== "string" || !validator.test(value)) {
          violations.push({
            workflow,
            location: `jobs.${jobName}.strategy.matrix.${axis}[${index}]`,
            value: String(value),
            reason,
          });
        }
      });
    }

    const argsAxis = matrix.args;
    if (Array.isArray(argsAxis)) {
      argsAxis.forEach((value, index) => {
        if (typeof value !== "string" || FORBIDDEN_ARGS.test(value)) {
          violations.push({
            workflow,
            location: `jobs.${jobName}.strategy.matrix.args[${index}]`,
            value: String(value),
            reason: "matrix args reference a non-Windows bundle/target",
          });
        }
      });
    }

    const include = matrix.include;
    if (!Array.isArray(include)) continue;

    include.forEach((entry, index) => {
      if (!isRecord(entry)) return;
      const base = `jobs.${jobName}.strategy.matrix.include[${index}]`;

      const platform = entry.platform;
      if (typeof platform === "string" && !WINDOWS_RUNNER.test(platform)) {
        violations.push({
          workflow,
          location: `${base}.platform`,
          value: platform,
          reason: "matrix platform is not a Windows runner",
        });
      }

      const target = entry.target;
      if (typeof target === "string" && !WINDOWS_TARGET.test(target)) {
        violations.push({
          workflow,
          location: `${base}.target`,
          value: target,
          reason: "matrix target is not a Windows MSVC triple",
        });
      }

      const args = entry.args;
      if (typeof args === "string" && FORBIDDEN_ARGS.test(args)) {
        violations.push({
          workflow,
          location: `${base}.args`,
          value: args,
          reason: "matrix args reference a non-Windows bundle/target",
        });
      }
    });
  }
  return violations;
}

/**
 * For jobs that call the reusable `build.yml`, any *literal* `with.platform` /
 * `with.target` must satisfy the Windows rules. Expression values such as
 * `${{ matrix.platform }}` are exempt — the matrix feeding them is checked by
 * `collectMatrixEntries`.
 */
export function collectReusableCallInputs(
  workflow: string,
  doc: Json,
): WorkflowViolation[] {
  const violations: WorkflowViolation[] = [];
  for (const [jobName, job] of jobEntries(doc)) {
    const uses = job.uses;
    if (typeof uses !== "string" || !uses.endsWith("build.yml")) continue;
    const withInputs = job.with;
    if (!isRecord(withInputs)) continue;

    const strategy = isRecord(job.strategy) ? job.strategy : {};
    const matrix = isRecord(strategy.matrix) ? strategy.matrix : {};
    const include = Array.isArray(matrix.include) ? matrix.include : [];
    const matrixProvides = (key: string): boolean => {
      const axis = matrix[key];
      if (Array.isArray(axis) && axis.length > 0) return true;
      return (
        include.length > 0 &&
        include.every(
          (entry) => isRecord(entry) && typeof entry[key] === "string",
        )
      );
    };

    const platform = withInputs.platform;
    if (
      typeof platform === "string" &&
      ((!isExpression(platform) && !WINDOWS_RUNNER.test(platform)) ||
        (isExpression(platform) &&
          (platform !== "${{ matrix.platform }}" ||
            !matrixProvides("platform"))))
    ) {
      violations.push({
        workflow,
        location: `jobs.${jobName}.with.platform`,
        value: platform,
        reason: "reusable-call platform is not a Windows runner",
      });
    }

    const target = withInputs.target;
    if (
      typeof target === "string" &&
      ((!isExpression(target) && !WINDOWS_TARGET.test(target)) ||
        (isExpression(target) &&
          (target !== "${{ matrix.target }}" || !matrixProvides("target"))))
    ) {
      violations.push({
        workflow,
        location: `jobs.${jobName}.with.target`,
        value: target,
        reason: "reusable-call target is not a Windows MSVC triple",
      });
    }
  }
  return violations;
}

function collectStringLeaves(value: Json, out: string[]): void {
  if (typeof value === "string") {
    out.push(value);
  } else if (Array.isArray(value)) {
    for (const item of value) collectStringLeaves(item, out);
  } else if (isRecord(value)) {
    for (const item of Object.values(value)) collectStringLeaves(item, out);
  }
}

/**
 * Scan every step's `run`, `if`, `env` and `with` *values* (never raw file
 * text) for tokens that betray a macOS/Linux code path.
 */
export function collectStepViolations(
  workflow: string,
  doc: Json,
): WorkflowViolation[] {
  const violations: WorkflowViolation[] = [];
  for (const [jobName, job] of jobEntries(doc)) {
    const steps = job.steps;
    if (!Array.isArray(steps)) continue;

    steps.forEach((step, index) => {
      if (!isRecord(step)) return;
      const base = `jobs.${jobName}.steps[${index}]`;
      const scanned: Array<[string, Json]> = [
        ["uses", step.uses],
        ["run", step.run],
        ["if", step.if],
        ["env", step.env],
        ["with", step.with],
      ];
      for (const [field, fieldValue] of scanned) {
        if (fieldValue === undefined) continue;
        const leaves: string[] = [];
        collectStringLeaves(fieldValue, leaves);
        for (const leaf of leaves) {
          const scannedLeaf =
            field === "run"
              ? leaf
                  .split(/\r?\n/)
                  .filter((line) => !line.trimStart().startsWith("#"))
                  .join("\n")
              : leaf;
          const lower = scannedLeaf.toLowerCase();
          for (const token of FORBIDDEN_STEP_TOKENS) {
            if (lower.includes(token.toLowerCase())) {
              violations.push({
                workflow,
                location: `${base}.${field}`,
                value: leaf,
                reason: `step value contains forbidden token "${token}"`,
              });
            }
          }
        }
      }
    });
  }
  return violations;
}

/** Run every collector and concatenate the violations. */
export function collectAllViolations(
  workflow: string,
  doc: Json,
): WorkflowViolation[] {
  return [
    ...collectRunsOn(workflow, doc),
    ...collectMatrixEntries(workflow, doc),
    ...collectReusableCallInputs(workflow, doc),
    ...collectStepViolations(workflow, doc),
  ];
}

export function formatViolations(violations: WorkflowViolation[]): string {
  return violations
    .map((v) => `  ${v.workflow} ${v.location}: ${v.reason} → ${v.value}`)
    .join("\n");
}
