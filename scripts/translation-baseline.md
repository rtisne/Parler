# Translation baseline

`translation-baseline.json` records translation keys that were already missing when the baseline check was introduced. It does **not** provide fallback text or count as a translation; i18next continues to fall back to English at runtime.

The checker requires each locale to match the baseline exactly:

- a newly missing key fails as `New missing keys`;
- a translated baseline key fails as `Translated keys to remove from baseline` until the baseline is reduced in the same change;
- a key absent from the English reference fails as `Extra keys absent from English`;
- every locale must appear in exactly one baseline group.

`common` contains keys missing from every non-English locale. Each entry in `groups` adds keys missing only from those locales. If a common key is translated for one locale, remove it from `common` and add it to the appropriate groups for locales where it is still missing.

Run both checks after changing translations or the baseline:

```sh
bun run test:translations
bun run check:translations
```

Do not add a key to the baseline merely to make CI green. New UI text should normally be added to every locale in the same pull request, using a reviewed translation or an explicit product decision about fallback behavior.
