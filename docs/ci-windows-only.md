# Windows-only CI

This fork ships **Windows x64 and ARM64 installers only**. Every GitHub Actions
job therefore runs on a Windows runner, and the installers CI produces are now
actually installed, launched, and uninstalled before they can reach anyone.

## Why

1. The macOS/Linux build matrix produced artifacts this fork never distributes,
   burning the majority of CI minutes.
2. No job ever installed the MSI/NSIS artifacts — a broken installer (bad
   bundling, a missing DLL in the installed layout, a broken uninstaller) could
   publish successfully. Only the raw `target/**/parler.exe` was smoke-tested.
3. Nothing stopped a future PR from silently reintroducing non-Windows jobs.

## Before → after job matrix

| Pipeline              | Before (jobs)                                          | After (jobs)                                    |
| --------------------- | ------------------------------------------------------ | ----------------------------------------------- |
| PR checks             | 4 — lint, prettier, playwright, rust-tests (all Linux) | 5 — same four + `powershell-tests`, all Windows |
| Build Test (dispatch) | 7 build matrix (2 macOS + 3 Linux + 2 Windows)         | 2 build matrix (Windows x64 + ARM64)            |
| PR Test Build         | 8 (7 builds + ubuntu comment)                          | 3 (2 Windows builds + Windows comment)          |
| Release               | 4 (2 ubuntu + 2 Windows)                               | 4 (all Windows)                                 |
| Build Windows         | 2 (1 Windows build + ubuntu release)                   | 2 (all Windows)                                 |
| **Total**             | **25 jobs — 18 non-Windows (72%)**                     | **16 jobs — 0 non-Windows**                     |

The heavyweight build matrix drops from **7 → 2** entries per dispatch of Build
Test / PR Test Build (−71%).

## Invariants

### 1. Structural regression test — `bun run test:ci-structure`

`scripts/windows-only-ci.test.ts` parses every `.github/workflows/*.yml` with
`Bun.YAML.parse` (structural, comment-insensitive) and fails if any workflow
reintroduces:

- a `runs-on` that is not a `windows-*` runner (or the single sanctioned
  `${{ inputs.platform }}` expression used by the reusable `build.yml`);
- a `strategy.matrix.include[]` whose `platform`/`target`/`args` reference
  macOS or Linux;
- a literal reusable-call `with.platform`/`with.target` that is not Windows;
- a step whose `run`/`if`/`env`/`with` **value** contains a macOS/Linux token
  (`apt-get`, `ubuntu`, `macos`, `APPLE_CERTIFICATE`, `apple-darwin`,
  `unknown-linux-gnu`, `appimage`, `.deb`, `.rpm`, `.dmg`, `fuse`).

This test runs in the `lint` job, so the invariant is enforced on every PR.

### 2. Installer lifecycle gate

`.github/scripts/installer-lifecycle.ps1` runs inside the reusable `build.yml`
right after the OpenBLAS verification — on the **same runner that just built the
bundles**, so there is no rebuild and no artifact download. For both MSI and
NSIS it performs:

1. **Resolve** the single installer under `bundle/msi/*.msi` or
   `bundle/nsis/*-setup.exe` (ambiguity → fail).
2. **Silent install** — `msiexec /i … /qn` (MSI, per-machine → HKLM) or the
   uppercase `/S` NSIS switch (per-user → `%LOCALAPPDATA%\Parler`, HKCU).
3. **Resolve the installed exe** from the uninstall-registry metadata
   (`DisplayName -eq 'Parler'` across all four HKLM/HKCU + WOW6432Node hives),
   via `InstallLocation` with a `DisplayIcon` fallback.
4. **Bounded launch survival** — launch `parler.exe --no-tray`, keep it alive
   for a fixed window, scan stderr for startup panics, then force-kill and
   confirm the process is gone.
5. **Real silent uninstall** — `msiexec /x {ProductCode}` or the NSIS
   `QuietUninstallString`/`UninstallString` (with `/S`). Because an NSIS
   uninstaller detaches a `%TEMP%` copy and returns early, completion is
   confirmed with a bounded poll (up to 120 s) until both the registry entry and
   the executable are gone.
6. **Residue verification** — fail if any `parler.exe` or `*.dll` remains.

On failure, MSI logs and the app's stdout/stderr are uploaded as the
`installer-lifecycle-logs-<target>` artifact.

The helper's pure logic (file resolution, entry selection, command
construction, exe resolution, residue assertion) is covered by
`.github/scripts/installer-lifecycle.Tests.ps1` (Pester 5), run by the
`powershell-tests` job.

## Why publication is gated

The lifecycle steps live **inside the `build` job** of the reusable
`build.yml`, so a failure fails that job. In `release.yml`, `publish-release`
requires `needs.publish-tauri.result == 'success'`, and `publish-tauri` is the
matrix that calls `build.yml`. A lifecycle failure on either architecture
therefore leaves the release a **draft** — a broken installer can never be
published. For PR Test Build, the same failure means no artifacts are produced
for humans to download.
