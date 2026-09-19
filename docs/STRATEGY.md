# OWE — Strategy

**Status:** DRAFT v1. The *how we work and why it works* document: process, risk, honesty, and community strategies that keep the project alive after the initial momentum.
**Companions:** [PRD](./PRD.md) · [TRD](./TRD.md) · [ARCHITECTURE](./ARCHITECTURE.md) · [BACKEND-DESIGN](./BACKEND-DESIGN.md) · [IMPLEMENTATION-PLAN](./IMPLEMENTATION-PLAN.md) · [UI-DESIGN](./UI-DESIGN.md)

---

## 1. Product strategy — where OWE wins

**Positioning:** "the low-resource live-wallpaper engine that isn't feature-frozen."

- vs **swww/awww**: they are excellent and frozen (author explicitly refuses new features). OWE provides runtime-managed live content, a governor, and a GUI while matching their daemon discipline.
- vs **hyprpaper**: static-only, no GUI. OWE keeps IPC coexistence (TRD §4) and adds live content.
- vs **Wallpaper Engine (Steam/Wine)**: native, no Wine, no Steam dependency — but *honest* that Workshop scene-format support is a post-1.0 plugin, not a launch promise (PRD §6.4).
- vs **mpvpaper/Phonto/waywe-rs**: single-focus tools; OWE unifies image+video+shader under one governor with per-output control.
- **Sequencing logic** (from the raw chat, kept): prove the boring path first (static images end-to-end), then add each content type only after the previous phase's gate passes. The daemon is the product; the GUI and the redesign follow the core (ADR-010).

## 2. Engineering process strategy (the operating system of the project)

1. **Phase gates are law** — IMPLEMENTATION-PLAN's exit criteria block progression; the sign-off log records date+commit. Skipping a gate requires an ADR listing exactly what debt was taken.
2. **Pragmatic TDD (ADR-008):** `[T]` tasks are red-green-refactor from the first line; `[I]` tasks (graphics/plumbing) get integration/golden/HW tests within the same phase. Golden-image freezing requires one human-reviewed snapshot — never auto-frozen (this prevents "tested against its own bugs").
3. **ADRs for every irreversible or vetoable decision** (ARCHITECTURE §9). The user can veto any ADR; vetoes update the doc set, not just the code.
4. **Small PRs, trunk-based:** each task in the plan is one PR-sized unit; feature flags aren't needed because phases are small and gated.
5. **Definition of Done** for any requirement (TRD §5): Verify method green in CI/HW checklist + gate row updated + docs touched if behavior changed.
6. **CI as the enforcer:** build+test (nextest), clippy `-D warnings`, fmt, headless-Wayland smoke, nightly fuzz, coverage floor on logic crates (NFR-CODE-1). The headless job exists from P0 — it is the harness that makes compositor claims testable.

## 3. Dependency & supply-chain strategy

- **Few deps by design** (ADR-004's no-tokio is one instance): every dependency is a liability in a daemon that runs 24/7. Rule of thumb: a crate enters only with a one-line justification in the PR description.
- **Cargo workspace hygiene:** `cargo-deny` (licenses + advisories + duplicates) from P0; `cargo-audit` in the nightly job; MSRV pinned and bumped only at major releases.
- **Risky dependencies, known honestly:**
  - *WebKitGTK* (Tauri's Linux webview) varies across distros — mitigated by pinning Tauri minor versions and keeping the GUI thin (it may crash; the wallpaper doesn't care — NFR-REL-2 world).
  - *GStreamer plugin availability* differs per distro — mitigated by the FFmpeg fallback behind `MediaDecoder` (ADR-006) and distro-specific install docs (Phonto's lists as the template, P7).
  - *wgpu/Mesa driver bugs* — mitigated by GL fallback and the lavapipe CI path; driver-specific issues go into a known-issues table, not the marketing README.
  - *Legacy VA-API (`i965`) on the pinned Reference machine (ADR-014)* — upstream-archived Haswell driver; the P4 capability probe plus the software-decode fallback (ADR-006) keep video working if hardware decode misbehaves, and the measured reality lands in the P6 report.
- **No vendored blobs, no bundled browsers, no telemetry. Ever.** (Differentiators, stated in the README.)

## 4. Performance strategy — measure, then speak

- **Honesty rule (PRD §4):** no performance claim exists until `owe-bench` produced it on the Reference Profile (pinned to the maintainer's Dell Latitude E5440, ADR-014 — old hardware on purpose: the weakest realistic case, so claims survive scrutiny); until then targets are labeled `UNVERIFIED`. This rule exists because machine-specific numbers lie.
- **Relative deltas are the primary metric:** OWE vs awww vs hyprpaper vs mpvpaper on identical workloads, same machine, same session — absolute numbers second. Both are published with raw data (NFR-PERF-5).
- **Regression watch:** CI perf smoke after P4 flags >10% CPU or RSS drift between runs of the same workload; the bench harness is reusable locally (`owe bench`) so contributors reproduce numbers.
- **The governor is the biggest lever, not micro-optimization:** pausing on fullscreen/battery saves more watts than any code-level tweak — hence P6 before P7 polish.

## 5. Compatibility strategy — the support matrix is a contract

- **Primary targets are CI-tested (Hyprland, Caelestia — both daily-driven on the maintainer's Zorin OS 18 machine, so real-HW gates are directly signable); generic layer-shell is smoke-tested.** We claim exactly what CI/HW verifies, nothing more (PRD §5).
- **Coexistence, not conquest:** detect-and-warn for hyprpaper/swww; never kill another tool's process by default; Caelestia shell-routed mode defers to Caelestia's theming (TRD §4) — OWE integrates into rices instead of replacing them.
- **No hardcoding rule:** every environment-specific behavior lives behind `ShellBackend` (BACKEND-DESIGN §2); adding niri/KDE later is a new crate + registry entry, which is the acceptance test for the abstraction itself (FR-SHELL-1).
- **Upstream drift:** Caelestia moves fast — OQ-2's pinned fixtures are re-verified each release; if the CLI breaks, the generic backend still works and the release notes say so.

## 6. Open-source & community strategy

- **License GPL-3.0-or-later (ADR-001):** compatible with borrowing from swww/awww + wpaperd; standard for this ecosystem. Every bundled shader pack ships its own license file (OQ-3).
- **Reference-code policy (ADR-013):** studied repos are pinned shallow clones in gitignored `reference/` via `scripts/fetch-reference.sh`; the file-level port map with PORT/READ-ONLY/AVOID verdicts is [REFERENCE-CODE-MAP](./REFERENCE-CODE-MAP.md); every ported piece is logged in `ATTRIBUTION.md` (repo, commit, license, files); GPL/MIT headers are preserved on ported code; reference code is never compiled into our build. Proprietary Wallpaper Engine assets/code are never copied — compatible formats are implemented clean-room, post-1.0. License reality at pin (verified 2026-09-19): swww/wpaperd/phonto GPL-3, wallr/waywe-rs MIT, **we-layerd unlicensed → READ-ONLY**.
- **Name check before launch (OQ-1):** "Open Wallpaper Engine" must clear the Wallpaper Engine trademark question before any public announcement — if unresolved, rename early (cheap now, expensive later).
- **Contribution surface ranked by stability:** day one, `owectl` + config + shader packs are stable-ish surfaces for contributors; the IPC protocol joins after v1.0 (versioned); renderer internals stay unstable until P6.
- **Docs as onboarding:** user guide, backend-authoring guide, and distro troubleshooting are P7 deliverables (PRD-F roadmap) — a "no hardcoding" architecture only pays off if outsiders can write the next backend.
- **Issue hygiene:** bug reports for rendering issues require the support-matrix template (compositor, GPU, driver, OWE version, `stats.get` output) — this feeds the matrix instead of stale guessing.

## 7. Release strategy

- **0.x cadence:** each phase gate ≈ one tagged release (v0.1…v0.6) so users can ride phases safely; breaking config/IPC changes allowed within 0.x with migration notes.
- **1.0 = P7 complete:** packaging + docs + benchmark report + name-check resolved. 1.0 promises: support matrix as CI-tested, all PRD v1.0 features, no UNVERIFIED claims left in the README.
- **Post-1.0:** semver for IPC + config schema; plugin ABI only after it survives its own ADR + security review (NFR-SEC-3).
- **Release notes rule:** every claim links to a gate row or benchmark artifact — same honesty contract as PRD §4.

## 8. Scope-control strategy (how this project avoids dying of ambition)

1. The non-goals list (PRD §6) is load-bearing: web wallpapers, X11, GNOME, Workshop-at-launch are rejections, not backlogs.
2. New ideas enter the post-1.0 backlog; they reach a phase only through the PRD's ADR process with a gate row added — never by PR creep.
3. The plugin ABI (PRD-F-34) is the pressure valve: community demand for exotic content formats lands there instead of in core.
4. If a phase slips two gate reviews in a row, its scope is cut, not its quality bar — documented in the sign-off log.

## 9. Documentation strategy

- This doc set is the source of truth; `docs/PLAN.md` is archived research (superseded note at its top).
- Every doc carries **Status**, **companions**, and one-line purpose; cross-links are relative; the README index (docs/README.md) is the only entry point newcomers need.
- Doc drift is a bug: gate reviews include "docs still true?" as a checklist item; ADRs are the only legal way to change architecture-level statements.
