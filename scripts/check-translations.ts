import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";
import {
  type TranslationBaseline,
  formatKeyPath,
  validateBaselineLocales,
  validateTranslationKeys,
} from "./check-translations-lib";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const LOCALES_DIR = path.join(__dirname, "..", "src", "i18n", "locales");
const BASELINE_FILE = path.join(__dirname, "translation-baseline.json");
const REFERENCE_LANG = "en";
const MAX_REPORTED_KEYS = 10;

type TranslationData = Record<string, unknown>;

const colors: Record<string, string> = {
  reset: "\x1b[0m",
  red: "\x1b[31m",
  green: "\x1b[32m",
  yellow: "\x1b[33m",
  blue: "\x1b[34m",
};

function colorize(text: string, color: string): string {
  return `${colors[color]}${text}${colors.reset}`;
}

function getLanguages(): string[] {
  return fs
    .readdirSync(LOCALES_DIR, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && entry.name !== REFERENCE_LANG)
    .map((entry) => entry.name)
    .sort();
}

function getAllKeyPaths(obj: TranslationData, prefix: string[] = []): string[] {
  const paths: string[] = [];

  for (const key in obj) {
    if (!Object.prototype.hasOwnProperty.call(obj, key)) continue;

    const currentPath = [...prefix, key];
    const value = obj[key];

    if (typeof value === "object" && value !== null && !Array.isArray(value)) {
      paths.push(...getAllKeyPaths(value as TranslationData, currentPath));
    } else {
      paths.push(formatKeyPath(currentPath));
    }
  }

  return paths.sort();
}

function loadJsonFile<T>(filePath: string, label: string): T | null {
  try {
    return JSON.parse(fs.readFileSync(filePath, "utf8")) as T;
  } catch (error) {
    console.error(colorize(`✗ Error loading ${label}:`, "red"));
    console.error(`  ${(error as Error).message}`);
    return null;
  }
}

function loadTranslation(lang: string): TranslationData | null {
  return loadJsonFile<TranslationData>(
    path.join(LOCALES_DIR, lang, "translation.json"),
    `${lang}/translation.json`,
  );
}

function reportKeys(label: string, keys: string[]): void {
  if (keys.length === 0) return;

  console.log(colorize(`  ${label} (${keys.length}):`, "yellow"));
  keys.slice(0, MAX_REPORTED_KEYS).forEach((key) => {
    console.log(`    - ${key}`);
  });
  if (keys.length > MAX_REPORTED_KEYS) {
    console.log(
      colorize(`    ... and ${keys.length - MAX_REPORTED_KEYS} more`, "yellow"),
    );
  }
}

function validateTranslations(): void {
  console.log(colorize("\n🌍 Translation Baseline Check\n", "blue"));

  const languages = getLanguages();
  const referenceData = loadTranslation(REFERENCE_LANG);
  const baseline = loadJsonFile<TranslationBaseline>(
    BASELINE_FILE,
    "translation-baseline.json",
  );

  if (!referenceData || !baseline) {
    process.exit(1);
  }

  const baselineLocales = validateBaselineLocales(languages, baseline);
  if (
    baselineLocales.missing.length > 0 ||
    baselineLocales.duplicated.length > 0 ||
    baselineLocales.unknown.length > 0
  ) {
    console.error(colorize("✗ Invalid baseline locale coverage", "red"));
    reportKeys("Missing locales", baselineLocales.missing);
    reportKeys("Duplicated locales", baselineLocales.duplicated);
    reportKeys("Unknown locales", baselineLocales.unknown);
    process.exit(1);
  }

  const referenceKeys = getAllKeyPaths(referenceData);
  let hasErrors = false;
  let documentedDebt = 0;

  console.log(`Reference has ${referenceKeys.length} keys`);
  console.log("─".repeat(60));

  for (const lang of languages) {
    const langData = loadTranslation(lang);
    if (!langData) {
      hasErrors = true;
      continue;
    }

    const result = validateTranslationKeys(
      referenceKeys,
      getAllKeyPaths(langData),
      lang,
      baseline,
    );
    const expectedMissing =
      baseline.common.length +
      baseline.groups
        .filter((group) => group.locales.includes(lang))
        .reduce((count, group) => count + group.additional.length, 0);
    documentedDebt += expectedMissing;

    const valid =
      result.unexpectedMissing.length === 0 &&
      result.resolvedBaseline.length === 0 &&
      result.extra.length === 0;

    if (valid) {
      console.log(
        colorize(
          `✓ ${lang.toUpperCase()}: matches baseline (${expectedMissing} documented missing keys)`,
          "green",
        ),
      );
      continue;
    }

    hasErrors = true;
    console.log(colorize(`✗ ${lang.toUpperCase()}: baseline drift`, "red"));
    reportKeys("New missing keys", result.unexpectedMissing);
    reportKeys(
      "Translated keys to remove from baseline",
      result.resolvedBaseline,
    );
    reportKeys("Extra keys absent from English", result.extra);
  }

  console.log("─".repeat(60));

  if (hasErrors) {
    console.error(
      colorize(
        "\n✗ Translation baseline drift detected. Update translations and the baseline together.",
        "red",
      ),
    );
    process.exit(1);
  }

  console.log(
    colorize(
      `\n✓ All ${languages.length} locales match the documented baseline (${documentedDebt} missing translations).`,
      "green",
    ),
  );
}

validateTranslations();
