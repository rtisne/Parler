export interface TranslationBaseline {
  common: string[];
  groups: Array<{
    locales: string[];
    additional: string[];
  }>;
}

export interface TranslationValidationResult {
  unexpectedMissing: string[];
  resolvedBaseline: string[];
  extra: string[];
}

export interface BaselineLocaleValidationResult {
  missing: string[];
  duplicated: string[];
  unknown: string[];
}

export function formatKeyPath(segments: string[]): string {
  return segments
    .map((segment) => segment.replaceAll("\\", "\\\\").replaceAll(".", "\\."))
    .join(".");
}

export function validateBaselineLocales(
  locales: string[],
  baseline: TranslationBaseline,
): BaselineLocaleValidationResult {
  const expected = new Set(locales);
  const configured = baseline.groups.flatMap((group) => group.locales);
  const counts = new Map<string, number>();

  for (const locale of configured) {
    counts.set(locale, (counts.get(locale) ?? 0) + 1);
  }

  return {
    missing: locales.filter((locale) => !counts.has(locale)).sort(),
    duplicated: configured
      .filter((locale) => (counts.get(locale) ?? 0) > 1)
      .filter((locale, index, all) => all.indexOf(locale) === index)
      .sort(),
    unknown: configured
      .filter((locale) => !expected.has(locale))
      .filter((locale, index, all) => all.indexOf(locale) === index)
      .sort(),
  };
}

function sortedDifference(left: Set<string>, right: Set<string>): string[] {
  return [...left].filter((key) => !right.has(key)).sort();
}

export function validateTranslationKeys(
  referenceKeys: string[],
  localeKeys: string[],
  locale: string,
  baseline: TranslationBaseline,
): TranslationValidationResult {
  const reference = new Set(referenceKeys);
  const translated = new Set(localeKeys);
  const missing = new Set(sortedDifference(reference, translated));
  const localeBaseline = baseline.groups
    .filter((group) => group.locales.includes(locale))
    .flatMap((group) => group.additional);
  const expectedMissing = new Set([...baseline.common, ...localeBaseline]);

  return {
    unexpectedMissing: sortedDifference(missing, expectedMissing),
    resolvedBaseline: sortedDifference(expectedMissing, missing),
    extra: sortedDifference(translated, reference),
  };
}
