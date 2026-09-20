// Every user-visible string goes through here from day one (TRD NRF-I18N-1 /
// UI-DESIGN §3): shipping a second language must not require touching a single
// component. Only English ships today — that is a translation gap, not a design
// decision taken by hardcoding strings in JSX.
//
// `StringKey` is derived from this object, so a typo in `t("...")` is a type error
// rather than a blank label.

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
  recordedHint: "not applied this run",
  stateLabel: "State",
  clear: "Clear",
  clearedLabel: "Cleared",
  clearNothing: "Nothing was set on that output.",
  goSlash: "Only",
  assignHere: "Apply selected",
  assignHereHint: "Apply the selected wallpaper to this output",
  governorPaused: "The governor is paused: nothing new will be drawn until it resumes.",

  libraryTitle: "Library",
  librarySearchPlaceholder: "Search wallpapers",
  libraryEmpty: "Nothing indexed yet. Scan your folders to fill the grid.",
  libraryNoneMatching: "No wallpaper matches that search.",
  libraryRootsLabel: "Indexed",
  libraryNoRoots: "no folders configured — set `library.paths` in the daemon config",
  scopeLabel: "Scanned",
  scan: "Rescan",
  scanning: "Scanning…",
  scanningLabel: "Scanned",
  scanSummaryLabel: "Scan",
  scanMissingRoots: "Unreachable folders (kept, not forgotten)",
  kindAll: "All kinds",
  thumbnailsPending: "Generating…",
  thumbnailUnavailable: "No preview",
  loadingMore: "Loading…",
  previousPage: "Previous",
  nextPage: "Next",
  pageLabel: "Page",
  totalsLabel: "Wallpapers",
  ofLabel: "of",

  selectionNone: "Select a wallpaper in the grid, then apply it.",
  selectionLabel: "Selected",
  applyAll: "Apply to all outputs",
  appliedLabel: "Applied",
  noOutputsMatched: "no outputs matched",
  transitionLabel: "Transition",
  transitionNone: "No transition",
  transitionDuration: "Duration (ms)",
  transitionUnavailable: "This daemon's config allows no transitions.",

  unavailableTitle: "Not in this build",
  failureLabel: "The daemon refused",

  shellTitle: "Shell",
  shellBackendLabel: "Backend",
  shellModeLabel: "Drawing",
  shellModeDaemonDrawn: "OWE draws the wallpaper",
  shellModeShellRouted: "the shell applies it (OWE stays out)",
  shellPatched: "runtime override — the config file still wins on restart",
  shellDetectOrder: "Detection order",
  shellNoneSelected: "no shell backend selected — OWE draws every wallpaper itself",
  shellRowHint: "Why each backend says what it says, straight from the backend",
  shellEventsLabel: "Shell events",
  shellEventsListening: "listening",
  shellEventsIdle: "not listening",
  shellEventsSeen: "seen",
  shellEventsDropped: "dropped",
  shellEventsReconnects: "reconnects",
  shellBackendOverride: "Backend override",
  shellBackendAuto: "auto-detect (recommended)",
  shellModeLabel2: "Draw mode",
  shellModeDaemonDrawnChoice: "daemon-drawn",
  shellModeShellRoutedChoice: "shell-routed",
  shellThemeHookLabel: "Let the shell theme-switch OWE wallpapers",
  shellThemeHookOn: "on",
  shellThemeHookOff: "off",
  shellDetectOrderHint: "comma-separated backend ids, most preferred first",
  shellApply: "Apply",
  shellApplying: "Applying…",
  shellApplied: "Applied.",
  shellPatchNone: "Nothing to change — pick a different value first.",
  shellCardUnreachable: "The daemon cannot be reached, so there is nothing to show about the shell.",
  shellCompetingTitle: "Competing wallpaper tools",
  shellCompetingHint: "Found running; may fight OWE for the same screens.",

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
