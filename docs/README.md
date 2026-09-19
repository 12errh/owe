# OWE — Documentation Index

**OWE (Open Wallpaper Engine)** — a low-resource live-wallpaper engine for Wayland (Hyprland + Caelestia Shell first), built as a Rust daemon + Tauri v2/React GUI. GPL-3.0-or-later. Decisions are logged as ADRs in ARCHITECTURE and are vetoable.

**Canonical repo:** `https://github.com/12errh/owe` (maintainer-created — already exists; all development, CI, and releases happen there, ADR-016).

| Doc | Purpose | Read when |
|---|---|---|
| [PRD](./PRD.md) | What we build, for whom, features with stable IDs, honest success criteria, non-goals, open questions | First |
| [TRD](./TRD.md) | Testable FR/NFR requirements with IDs, verification methods, phase mapping | Before implementing anything |
| [ARCHITECTURE](./ARCHITECTURE.md) | System design, components, data flows, tech choices, **ADR decision log** | Before changing design |
| [BACKEND-DESIGN](./BACKEND-DESIGN.md) | Daemon internals: crates, traits, IPC spec v1, config schema, governor, media pipelines | Before writing daemon code |
| [IMPLEMENTATION-PLAN](./IMPLEMENTATION-PLAN.md) | **The execution contract**: phase-by-phase TDD tasks + hard exit-criteria gates + sign-off log | Always, while developing |
| [UI-DESIGN](./UI-DESIGN.md) | Stub: simple-UI contract (P1–P2) + staged redesign plan (post-core) | At P1/P2 and again post-P6 |
| [STRATEGY](./STRATEGY.md) | Positioning, process, dependency risk, performance honesty, community, releases | When making process/scope calls |
| [REFERENCE-CODE-MAP](./REFERENCE-CODE-MAP.md) | **How reference code is fetched and used**: the clone→map→port workflow, plus the per-file port map for all six studied repos with PORT / READ-ONLY / AVOID verdicts | Before touching anything in `reference/` |
| [ATTRIBUTION](../ATTRIBUTION.md) | Provenance ledger: pinned commits, verified licenses, and every piece of code ported from a reference repo | Whenever code is ported |
| [PLAN](./PLAN.md) | *(Superseded)* original research: existing Rust wallpaper engines and what we borrow from each | Historical reference only |

## Ground rules

1. **Gates are law.** A phase's exit criteria block the next phase; exceptions require an ADR recording the debt (IMPLEMENTATION-PLAN §0).
2. **No silent drift.** Changing anything an ADR covers → new/updated ADR. Docs drift is treated as a bug at gate reviews.
3. **No unverified claims.** Performance targets are `UNVERIFIED` until the P6 benchmark publishes numbers (PRD §4, STRATEGY §4).
4. **Nothing is hardcoded.** Environment behavior lives behind `ShellBackend`; content types behind `ContentRenderer`; backends are registry entries, config-selected.
5. **Open questions are tracked, not assumed** — PRD §7 (OQ-1 name/trademark, OQ-2 Caelestia CLI pinning, OQ-3 shader licenses, OQ-4 benchmark silicon). Any of these can change a decision; they are surfaced the moment they block a phase.
6. The user may veto any ADR; vetoes update the doc set first, then the code.
7. **Reference code is provenance-tracked (ADR-013)** — studied repos live as pinned shallow clones in gitignored `reference/` (never built); every ported piece is logged in `ATTRIBUTION.md` (repo, commit, license) with original license headers preserved. The dev/benchmark platform is the maintainer's Dell Latitude E5440 on Zorin OS 18 (Ubuntu 24.04 base) daily-driving Hyprland + Caelestia (ADR-014/015).
