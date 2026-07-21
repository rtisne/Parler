import { afterEach, describe, expect, test } from "bun:test";
import {
  mkdtempSync,
  readFileSync,
  rmSync,
  mkdirSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const temporaryDirectories: string[] = [];

afterEach(() => {
  for (const directory of temporaryDirectories.splice(0)) {
    rmSync(directory, { recursive: true, force: true });
  }
});

function buildWorkflow(): string {
  return readFileSync(
    resolve(import.meta.dir, "../.github/workflows/build.yml"),
    "utf8",
  );
}

function updaterDisableCommand(): string {
  const command = buildWorkflow().match(
    /- name: Disable updater artifacts signing[\s\S]*?node -e '([^'\n]*)'/,
  )?.[1];

  if (!command) {
    throw new Error(
      "Updater configuration command not found in build workflow",
    );
  }

  return command;
}

describe("unsigned build updater configuration", () => {
  test("disables updater configuration when signing is disabled or the key is absent", () => {
    const workflow = buildWorkflow();
    const disableStepStart = workflow.indexOf(
      "- name: Disable updater artifacts signing (unsigned build)",
    );
    const buildStepStart = workflow.indexOf(
      "- name: Build with Tauri",
      disableStepStart,
    );
    const disableStep = workflow.slice(disableStepStart, buildStepStart);

    expect(disableStepStart).toBeGreaterThan(-1);
    expect(disableStep).toContain(
      "!inputs.sign-binaries || steps.signing-check.outputs.has-signing-key != 'true'",
    );
    expect(disableStep).toContain("PARLER_UPDATER_ENABLED=false");

    const rustApplication = readFileSync(
      resolve(import.meta.dir, "../src-tauri/src/lib.rs"),
      "utf8",
    );
    expect(rustApplication).toMatch(
      /if updater_enabled_from_build_flag\(option_env!\("PARLER_UPDATER_ENABLED"\)\)\s*\{\s*builder = builder\.plugin\(tauri_plugin_updater::Builder::new\(\)\.build\(\)\);\s*\}/s,
    );
  });

  test("removes the updater plugin configuration instead of leaving it without a pubkey", () => {
    const directory = mkdtempSync(join(tmpdir(), "parler-updater-config-"));
    temporaryDirectories.push(directory);
    const tauriDirectory = join(directory, "src-tauri");
    mkdirSync(tauriDirectory);
    const configPath = join(tauriDirectory, "tauri.conf.json");
    writeFileSync(
      configPath,
      JSON.stringify({
        bundle: { createUpdaterArtifacts: true },
        plugins: {
          updater: {
            pubkey: "test-public-key",
            endpoints: [],
          },
        },
      }),
    );

    const result = Bun.spawnSync(["node", "-e", updaterDisableCommand()], {
      cwd: directory,
      stdout: "pipe",
      stderr: "pipe",
    });
    expect(result.exitCode).toBe(0);

    const transformed = JSON.parse(readFileSync(configPath, "utf8"));
    expect(transformed.bundle.createUpdaterArtifacts).toBe(false);
    expect(transformed.plugins?.updater).toBeUndefined();
  });
});

describe("Windows startup smoke gate", () => {
  test("runs the packaged application after Tauri build and before artifact verification", () => {
    const workflow = buildWorkflow();

    // The Windows-only workflow keeps exactly one Tauri build step: the
    // `apple_signing` variant was removed, so there is no longer a guarded pair.
    expect(workflow.match(/- name: Build with Tauri/g)?.length).toBe(1);

    const buildStep = workflow.indexOf("- name: Build with Tauri");
    const smokeStep = workflow.indexOf(
      "- name: Smoke test Windows application startup",
    );
    const artifactVerificationStep = workflow.indexOf(
      "- name: Verify staged OpenBLAS runtime (Windows)",
    );

    expect(buildStep).toBeGreaterThan(-1);
    expect(smokeStep).toBeGreaterThan(buildStep);
    expect(artifactVerificationStep).toBeGreaterThan(smokeStep);
    const smokeSection = workflow.slice(smokeStep, artifactVerificationStep);
    expect(smokeSection).toContain(
      ".github/scripts/smoke-test-windows-startup.ps1",
    );
    expect(smokeSection).toContain("contains(inputs.build-args, '--target')");
    expect(smokeSection).toContain("src-tauri/target/{0}/{1}/parler.exe");
    expect(smokeSection).toContain("src-tauri/target/{0}/parler.exe");

    const smokeScript = readFileSync(
      resolve(
        import.meta.dir,
        "../.github/scripts/smoke-test-windows-startup.ps1",
      ),
      "utf8",
    );
    expect(smokeScript).not.toContain("$candidates");
  });
});

describe("Installer lifecycle gates", () => {
  test("runs MSI then NSIS lifecycle checks after OpenBLAS verification and before artifact upload", () => {
    const workflow = buildWorkflow();

    const verifyStep = workflow.indexOf(
      "- name: Verify staged OpenBLAS runtime (Windows)",
    );
    const msiStep = workflow.indexOf("- name: Installer lifecycle test (MSI)");
    const nsisStep = workflow.indexOf(
      "- name: Installer lifecycle test (NSIS)",
    );
    const uploadStep = workflow.indexOf("- name: Upload artifacts (Windows)");

    expect(verifyStep).toBeGreaterThan(-1);
    expect(msiStep).toBeGreaterThan(verifyStep);
    expect(nsisStep).toBeGreaterThan(msiStep);
    expect(uploadStep).toBeGreaterThan(nsisStep);

    const lifecycleSection = workflow.slice(msiStep, uploadStep);
    expect(lifecycleSection).toContain(
      ".github/scripts/installer-lifecycle.ps1",
    );
    expect(lifecycleSection).toContain("-InstallerType msi");
    expect(lifecycleSection).toContain("-InstallerType nsis");
  });

  test("uploads lifecycle diagnostics only when a lifecycle step fails", () => {
    const workflow = buildWorkflow();
    const diagnosticsStep = workflow.indexOf(
      "- name: Upload installer lifecycle diagnostics",
    );
    const nsisStep = workflow.indexOf(
      "- name: Installer lifecycle test (NSIS)",
    );

    expect(diagnosticsStep).toBeGreaterThan(nsisStep);
    const diagnosticsSection = workflow.slice(
      diagnosticsStep,
      workflow.indexOf("- name: Upload artifacts (Windows)"),
    );
    expect(diagnosticsSection).toContain("if: failure()");
    expect(diagnosticsSection).toContain("installer-lifecycle");
  });
});
