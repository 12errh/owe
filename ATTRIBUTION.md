# ATTRIBUTION — Reference Repos & Ported Code

**Policy (ADR-013):** reference repos are pinned shallow clones in `reference/` (gitignored, never compiled). **Reading** reference code is free. **Porting** code into OWE requires: (1) the original license/copyright header preserved in the ported file, (2) a row in the Ported-Code Log below, (3) the port marked in `docs/REFERENCE-CODE-MAP.md`. Gate reviews verify this ledger stays accurate.

## 1. Reference repos (cloned & pinned 2026-09-19)

| Repo | Upstream | Pinned commit | License (verified from the repo at pin) |
|---|---|---|---|
| swww (awww) | https://github.com/LGFae/swww | `8a3b1bf9f3363683097aa5467d3058cc2278da1a` | GPL-3.0 (LICENSE). Project renamed awww, upstream moved to Codeberg: https://codeberg.org/LGFae/awww |
| wpaperd | https://github.com/danyspin97/wpaperd | `483225a9c6ad4f9bbf9104f958eafee9c47d4612` | GPL-3.0 (LICENSE.md) |
| wallr | https://github.com/programmersd21/wallr | `2ad4c14a3e8c44807fc98baf8698bd7462e83e27` | MIT ("Copyright (c) 2026 Wallr Contributors", LICENSE) |
| phonto | https://github.com/museslabs/phonto | `9eaa162135626d22617bca1bdbedb52567d76074` | **GPL-3.0** (LICENSE) — corrects an earlier research note that guessed "permissive" |
| waywe-rs | https://github.com/hack3rmann/waywe-rs | `e86fbbbf6a6d4e09b660c0a9240989b01d0301f7` | MIT ("Copyright (c) 2025 hack3rmann", LICENSE) |
| we-layerd | https://github.com/Aromatic05/we-layerd | `82f9a0695c0c1fdf01eb429fb9675750a9dde55c` | ⚠️ **NO LICENSE FILE at pin** → all rights reserved. **READ-ONLY: never copy any code from it.** See REFERENCE-CODE-MAP §3.6. Revisit only if upstream adds a license (via ADR). |

Re-pin policy: pins are frozen for the current development cycle; refreshing a pin requires updating this table, `scripts/fetch-reference.sh`, and re-checking each repo's verdicts in `docs/REFERENCE-CODE-MAP.md` in the same change.

## 2. Ported-code log

Every piece of code copied from a reference repo lands here. **No entries yet** — the table exists so the first port has nowhere to hide.

| Date | Source (repo @ commit) | Ported files / symbols | Into (OWE crate/module) | License header preserved | PR / commit |
|---|---|---|---|---|---|
| — | — | — | — | — | — |

## 3. Non-code borrowings

Designs, config shapes, and ideas learned from reference repos (no code copied) are credited here as a courtesy, tracked per repo in `docs/REFERENCE-CODE-MAP.md` (READ-ONLY entries). Examples: swww's transition-effect concept, wpaperd's per-output TOML sections, we-layerd's governor rule ideas, wallr's benchmark-harness pattern.
