# OWE — UI Design

**Status:** 🚧 **INTENTIONAL STUB** (per decision ADR-010). This document defines only: (1) the *simple UI* contract that P1–P2 must meet, and (2) the *structured plan* for the full redesign, which starts **after the core (P0–P6) ships**. Do not treat anything here as the final look & feel.
**Companions:** [PRD](./PRD.md) · [TRD](./TRD.md) · [ARCHITECTURE](./ARCHITECTURE.md) · [BACKEND-DESIGN](./BACKEND-DESIGN.md) · [IMPLEMENTATION-PLAN](./IMPLEMENTATION-PLAN.md) · [STRATEGY](./STRATEGY.md)

---

## 1. Why the UI is deliberately deferred

The product's risk lives in the daemon (rendering, resources, compositor integration) — not in the GUI. Polishing pixels before the daemon is proven would spend effort on the least risky part. So: build the core with a functional, unpretentious UI; redesign it once, deliberately, against a real information architecture. This mirrors the raw-chat decision that the daemon is the product and the GUI its best friend.

## 2. Design inputs (what any redesign must serve)

From PRD personas + IPC capabilities:

1. **Primary loop:** browse library → preview → assign to monitor(s) → apply. Must be < 3 clicks for the common case.
2. **Per-output state is first-class:** current wallpaper, policy, FPS, decode path per output — never hidden behind tabs.
3. **Governor visibility:** users must see *why* a wallpaper is paused (fullscreen/battery/manual) — trust comes from explainability.
4. **Power users:** everything the GUI does must also exist in `owectl`/config; GUI adds no unique state (TRD FR-LIB-6).
5. **Resource honesty:** the GUI itself reports its own cost lightly (it is disposable; closing it frees its RAM — say so in the UI).

## 3. Simple-UI contract (P1–P2, acceptance criteria for the gates)

Functional, not pretty. Explicitly allowed to look rough.

- [ ] Single window, two views: **Library** (grid of thumbnails, folder picker, search box) and **Outputs** (card per monitor: current wallpaper name, apply button, dropdown of library items, FPS cap number input from P2).
- [ ] Daemon status bar: connected/reconnecting, backend id, version (from `hello`).
- [ ] Apply = one click; per-output apply = one dropdown + one click. No multi-step wizards.
- [ ] All strings externalized (NRF-I18N-1) even though only English ships.
- [ ] Keyboard: Tab order sane, Enter applies focused item, Esc closes dialogs. (Full a11y is the redesign wave's job — NFR-A11Y-1 exempts P2.)
- [ ] Visual spec: default Tauri window, Tailwind default palette, dark background, one accent color. No custom icon set, no animations, no branding. Estimated: ~10 components, ~1,500 LoC TS — deliberately small.
- [ ] Excluded on purpose: shader param editors (arrive P5 functional), governor dashboard (P6 functional), settings pages (config.toml is the settings UI until the redesign), tray (P7).

**Exit criteria for this contract** live in the P1/P2 gates (IMPLEMENTATION-PLAN). If a redesign wish sneaks in early, it goes to §5, not into the code.

## 4. Visual/content inventory to produce during the redesign wave

Deliverables of the wave, in `docs/ui/`: information-architecture map, user flows (primary loop + governor explainability), low-fi wireframes, high-fi mockups (dark first), component inventory with states, motion spec (if any — must respect resource thesis: no always-running CSS animations), icon set decision, theme tokens (matching Caelestia/Hyprland aesthetics where sensible without coupling to them), and an axe/pa11y accessibility audit report.

## 5. Redesign plan (staged, executed post-core — the "stecw plan")

Each stage ends with its own mini-gate before the next begins:

| Stage | Work | Gate |
|---|---|---|
| R1 — Research & IA | Usage review of the simple UI (own use + issue feedback), competitor screenshots (Lutris-style library grids, wayer/waypaper), IA map, flows for §2.1–2.3 | IA + flows reviewed and signed by maintainer |
| R2 — Wireframes | Low-fi for: Library, Outputs, Governor dashboard, Settings, First-run (backend detection), Onboarding-lite | Clickable paper-prototype pass through all primary flows |
| R3 — Visual system | Tokens (color/type/spacing), dark-first theme, component inventory + states, icon decision | Token sheet + storybook-style component gallery reviewed |
| R4 — Implementation | Rebuild screens on the existing IPC client (no protocol changes needed — that's the point of the design), progressive: Library → Outputs → Governor → Settings | Feature-parity checklist vs simple UI = 100%; perf budget: GUI idle < 60 MiB RSS, no render loop when idle |
| R5 — A11y & polish | Keyboard-complete, axe/pa11y audits, contrast ≥ 4.5:1, light theme, i18n hooks exercised with a second language | NFR-A11Y-1 passes; zero axe criticals |
| R6 — Feedback loop | Ship behind the same release cadence; issues → R1 loop | — |

Rules for the wave: no daemon/IPC changes requested by UI alone (UI adapts to the daemon, not vice versa); every redesign PR keeps the GUI-disposable property (close = zero background work); budget one release cycle total — if it slips, remaining stages re-gate.

## 6. Out of scope for this document

Final visual design (that is §4/§5's job, later), branding/logo (deferred with OQ-1 name check), marketing site.
