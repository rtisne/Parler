import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const temporaryDirectories: string[] = [];

afterEach(() => {
  for (const directory of temporaryDirectories.splice(0)) {
    rmSync(directory, { recursive: true, force: true });
  }
});

describe("production frontend bundle", () => {
  test("does not generate circular chunks that can break ESM initialization", async () => {
    const outputDirectory = mkdtempSync(join(tmpdir(), "parler-vite-build-"));
    temporaryDirectories.push(outputDirectory);

    const process = Bun.spawn(
      ["bunx", "vite", "build", "--outDir", outputDirectory, "--emptyOutDir"],
      {
        cwd: resolve(import.meta.dir, ".."),
        stdout: "pipe",
        stderr: "pipe",
      },
    );

    const [exitCode, stdout, stderr] = await Promise.all([
      process.exited,
      new Response(process.stdout).text(),
      new Response(process.stderr).text(),
    ]);
    const buildOutput = `${stdout}\n${stderr}`;

    expect(exitCode).toBe(0);
    expect(buildOutput).not.toContain("Circular chunk:");
  }, 30_000);
});
