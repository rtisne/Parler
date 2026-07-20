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

| Upstream SHA | Capability             | Local PR                           | Local commit | Status  | Notes                                                                      |
| ------------ | ---------------------- | ---------------------------------- | ------------ | ------- | -------------------------------------------------------------------------- |
| 5b651cc      | Windows thin LTO       | fix/upstream-sync-windows-keyboard | TBD          | planned | Manual one-line port                                                       |
| 4239cd3      | Secret log redaction   | fix/upstream-sync-security-updater | TBD          | adapted | Also covers Gemini key; no serialization change                            |
| (fork)       | Updater fork isolation | fix/upstream-sync-security-updater | TBD          | adapted | Remove Melvynx feed; disable auto checks until a signed rtisne feed exists |

## Provenance trailer

For any adapted or cherry-picked change, add to the commit message:

```
Upstream-Commit: <sha>
Upstream-Repository: https://github.com/Melvynx/Parler
```
