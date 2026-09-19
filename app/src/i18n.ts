// Every user-visible string goes through here from day one (TRD NRF-I18N-1 /
// UI-DESIGN §3): shipping a second language must not require touching a single
// component. Only English ships in P0 — that is a translation gap, not a design
// decision made by hardcoding strings.

export const strings = {
  appTitle: "OWE",
  appTagline: "Low-resource live wallpaper engine for Wayland",

  refresh: "Refresh",
  refreshing: "Checking…",

  daemonLabel: "Daemon",
  daemonConnected: "connected",
  daemonDisconnected: "not running",
  daemonUnknown: "not checked yet",

  versionLabel: "Version",
  schemaLabel: "Protocol",
  socketLabel: "Socket",
  shellBackendsLabel: "Shell backends",
  contentKindsLabel: "Content kinds",
  mediaBackendsLabel: "Decode backends",
  none: "none advertised",

  placeholderTitle: "Library and Outputs views arrive in Phase 1",
  placeholderBody:
    "Phase 0 ships the wiring only: this window proves the GUI can reach the daemon. Browsing, assigning, and per-output controls are built in Phase 1 against the same IPC.",
  disposableNote:
    "This window is disposable — closing it leaves no background work and frees its memory. The daemon keeps rendering.",

  statusHintDisconnected:
    "Start it with `owed` (or `systemctl --user start owed`) and press Refresh.",
} as const;

export type StringKey = keyof typeof strings;

/** Look up a user-visible string. */
export function t(key: StringKey): string {
  return strings[key];
}

/** Join a list, falling back to a placeholder when it is empty. */
export function tList(items: string[], whenEmpty: StringKey): string {
  return items.length > 0 ? items.join(", ") : t(whenEmpty);
}
