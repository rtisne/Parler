# Upstream synchronization ledger

Upstream: https://github.com/Melvynx/Parler
Last reviewed upstream release: v0.9.1
Strategy: selective behavioral ports; never merge upstream/main.

The upstream history has diverged and no longer provides a safe merge base.
Each relevant upstream change is re-implemented (or manually ported) behind its
own vertical PR, with TDD, explicit commit provenance, independent review and a
green CI. Do not run `git merge upstream/main`, `git rebase upstream/main`, or a
range cherry-pick across upstream tags.

## Status values

```
planned | adapted | cherry-picked | already-present | deferred | rejected
```

- `planned` — identified, not yet ported.
- `adapted` — behavior re-implemented for this fork (may differ from upstream).
- `cherry-picked` — imported with `git cherry-pick -x <sha>` after isolated test.
- `already-present` — the fork already provides an equivalent behavior.
- `deferred` — postponed to a later milestone.
- `rejected` — intentionally not imported (conflicts with fork-specific behavior).

## Ledger

| Upstream SHA | Capability                | Local PR | Local commit | Status   | Notes                                                                                                        |
| ------------ | ------------------------- | -------- | ------------ | -------- | ------------------------------------------------------------------------------------------------------------ |
| `5b651cc`    | Windows Thin LTO          | #22      | `964b79f`    | adapted  | One-line manual port; keeps `panic = "abort"`; PR merge build validated on Windows x64                       |
| `(local)`    | Windows OpenBLAS runtime  | pending  | pending      | adapted  | No Melvynx commit; uses pinned OpenBLAS 0.3.31 x64/WOA64 archives and Windows-only Cargo feature unification |
| `4239cd3`    | Secret log redaction      | #21      | `433c556`    | adapted  | Also covers the Gemini key; no serialized settings change                                                    |
| (fork)       | Updater fork isolation    | #21      | `70e62e0`    | adapted  | Melvynx feed removed; checks disabled until a signed rtisne feed is available                                |
| `f4fca4b`    | Selective hotkey blocking | #22      | `c3ac2b1`    | deferred | Port removed after review: `handy-keys 0.2.4` lacks safe Linux listener and transactional hook semantics     |

## Provenance trailer

For any adapted or cherry-picked change, add to the commit message:

```
Upstream-Commit: <sha>
Upstream-Repository: https://github.com/Melvynx/Parler
```
