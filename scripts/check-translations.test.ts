import { describe, expect, test } from "bun:test";
import {
  formatKeyPath,
  validateBaselineLocales,
  validateTranslationKeys,
} from "./check-translations-lib";

const baseline = {
  common: ["legacy.missing"],
  groups: [
    {
      locales: ["fr"],
      additional: ["fr.only.missing"],
    },
  ],
};

describe("translation baseline validation", () => {
  test("formats dotted and escaped key segments without collisions", () => {
    expect(formatKeyPath(["models", "parakeet-tdt-0.6b-v2", "name"])).toBe(
      "models.parakeet-tdt-0\\.6b-v2.name",
    );
    expect(formatKeyPath(["literal\\segment", "value"])).toBe(
      "literal\\\\segment.value",
    );
  });

  test("accepts exactly the documented translation debt", () => {
    const result = validateTranslationKeys(
      ["translated", "legacy.missing", "fr.only.missing"],
      ["translated"],
      "fr",
      baseline,
    );

    expect(result).toEqual({
      unexpectedMissing: [],
      resolvedBaseline: [],
      extra: [],
    });
  });

  test("rejects a new missing key even when the total count is unchanged", () => {
    const result = validateTranslationKeys(
      ["translated", "legacy.missing", "fr.only.missing"],
      ["fr.only.missing"],
      "fr",
      baseline,
    );

    expect(result.unexpectedMissing).toEqual(["translated"]);
    expect(result.resolvedBaseline).toEqual(["fr.only.missing"]);
  });

  test("reports baseline entries that have been translated", () => {
    const result = validateTranslationKeys(
      ["translated", "legacy.missing", "fr.only.missing"],
      ["translated", "fr.only.missing"],
      "fr",
      baseline,
    );

    expect(result.resolvedBaseline).toEqual(["fr.only.missing"]);
  });

  test("rejects keys absent from the English reference", () => {
    const result = validateTranslationKeys(
      ["translated", "legacy.missing", "fr.only.missing"],
      ["translated", "unexpected.extra"],
      "fr",
      baseline,
    );

    expect(result.extra).toEqual(["unexpected.extra"]);
  });

  test("rejects missing, duplicated, and unknown baseline locales", () => {
    const result = validateBaselineLocales(["de", "fr"], {
      common: [],
      groups: [
        {
          locales: ["fr", "fr", "it"],
          additional: [],
        },
      ],
    });

    expect(result).toEqual({
      missing: ["de"],
      duplicated: ["fr"],
      unknown: ["it"],
    });
  });
});
