import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const bindings = readFileSync("src/bindings.ts", "utf8");
const settings = readFileSync("src-tauri/src/settings.rs", "utf8");
const models = readFileSync("src-tauri/src/managers/model.rs", "utf8");

function rustEnumVariants(source: string, enumName: string): string[] {
  const body = source.match(
    new RegExp(`pub enum ${enumName}\\s*\\{([\\s\\S]*?)\\n\\}`),
  )?.[1];
  if (!body) throw new Error(`Rust enum ${enumName} not found`);
  return body
    .split("\n")
    .map((line) => line.trim().replace(/,$/, ""))
    .filter((line) => /^[A-Z][A-Za-z0-9_]*$/.test(line));
}

function typescriptUnion(typeName: string): string[] {
  const declaration = bindings.match(
    new RegExp(`export type ${typeName} = ([^\\n]+)`),
  )?.[1];
  if (!declaration) throw new Error(`TypeScript type ${typeName} not found`);
  return Array.from(declaration.matchAll(/"([^"]+)"/g), (match) => match[1]);
}

describe("generated binding contract", () => {
  test("EngineType mirrors every Rust variant", () => {
    expect(typescriptUnion("EngineType")).toEqual(
      rustEnumVariants(models, "EngineType"),
    );
  });

  test("cloud target migration fields are present on both sides", () => {
    for (const field of [
      "selected_transcription_target",
      "long_audio_target",
      "transcription_target_migration_version",
      "secret_store_migration_version",
    ]) {
      expect(settings).toContain(`pub ${field}:`);
      expect(bindings).toContain(`${field}?:`);
    }
  });

  test("new provider and retry commands remain exported", () => {
    expect(bindings).toContain("async getTranscriptionTargets()");
    expect(bindings).toContain("async retryCloudHistoryEntry(");
    expect(bindings).toContain("async testCloudProviderConnection(");
    expect(bindings).toContain("retention_text: string | null");
  });
});
