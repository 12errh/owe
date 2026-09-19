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

  outputsTitle: "Outputs",
  outputsNone: "The daemon reports no outputs.",
  outputsUnavailable: "Could not read the outputs from the daemon.",
  focusedBadge: "focused",
  wallpaperLabel: "Wallpaper",
  wallpaperNone: "no wallpaper applied by OWE",
  recordedLabel: "Recorded",
  recordedHint: "not applied this run; restore lands in P2",
  stateLabel: "State",
  clear: "Clear",
  clearedLabel: "Cleared",
  clearNothing: "Nothing was set on that output.",
  governorPaused: "The governor is paused: nothing new will be drawn until it resumes.",

  libraryTitle: "Library",
  chooseFolder: "Choose folder…",
  libraryEmpty: "Pick a folder to see the wallpapers in it.",
  libraryNoneInFolder: "No images found in that folder.",
  scopeLabel: "Scanned",
  apply: "Apply",
  appliedLabel: "Applied",
  noOutputsMatched: "no outputs matched",

  unavailableTitle: "Not in this build",
  failureLabel: "The daemon refused",

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
