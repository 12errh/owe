// Typed wrappers over the Rust commands. Components never call `invoke` directly:
// keeping the raw command names and wire shapes in one file is what makes the
// daemon-facing surface reviewable (ARCHITECTURE §1: the GUI owns no state, it
// only asks the daemon).
//
// Field names are snake_case throughout because they mirror the Rust structs and,
// below them, the daemon's JSON — the same names `owectl --json` prints. Renaming
// them for JavaScript's sake would put a translation layer between the UI and the
// protocol, which is exactly where a "the GUI shows the wrong thing" bug lives.
//
// Nothing caches and nothing polls: the UI does work when the user asks for it,
// which is the whole point of a resource-first tool.

import { invoke } from "@tauri-apps/api/core";

/** Mirrors `DaemonStatus` in `app/src-tauri/src/main.rs`. */
export interface DaemonStatus {
  connected: boolean;
  socket_path: string;
  server_version: string | null;
  schema: string | null;
  shell_backends: string[];
  content_kinds: string[];
  media_backends: string[];
  /** Transitions the picker may offer (what the daemon renders *and* allows). */
  transitions: string[];
  /** Planned-but-missing features, as `"<id>: <why>"`. */
  unavailable: string[];
  error: string | null;
}

/** Mirrors `IpcError`: a protocol code, or `unreachable`/`protocol`/`cache`. */
export interface IpcFailure {
  code: string;
  message: string;
}

/** One registered shell backend and what it says about this session. */
export interface ShellBackendRow {
  id: string;
  /** `strong`, `weak` or `none`. */
  confidence: string;
  /** Why, in the backend's own words — the whole point of the card. */
  reason: string;
  selected: boolean;
  /** `daemon-drawn` or `shell-routed` for this backend under the live config. */
  mode: string;
}

/** The shell situation, as the settings/status card shows it. */
export interface ShellStatus {
  connected: boolean;
  backend: string | null;
  reason: string | null;
  mode: string;
  routed: boolean;
  patched: boolean;
  detect_order: string[];
  backends: ShellBackendRow[];
  /** The daemon's event-bus counters, or null when it publishes none. */
  events: {
    listening: boolean;
    published: number;
    dropped: number;
    reconnects: number;
    subscribers: number;
  } | null;
  /** Verbatim notices about wallpaper tools fighting over the same screens. */
  competing_tools: string[];
  /** The patch's own note — shown after a change, never invented by the UI. */
  runtime_note: string | null;
  error: string | null;
}

/** The `config.patch` shell subtree; unset keys are left alone by the daemon. */
export interface ShellPatch {
  backend?: string;
  caelestia_mode?: string;
  theme_hook?: boolean;
  detect_order?: string[];
}

/** One indexed wallpaper, as the grid renders it. */
export interface LibraryItem {
  id: number;
  path: string;
  name: string;
  kind: string;
  /** `library:<id>` — what an apply takes, so the UI never builds one itself. */
  reference: string;
  /** Cached thumbnail *path*, or null. Informational: the grid shows `data_url`. */
  thumb: string | null;
  bytes: number;
}

/** A page of the indexed library. */
export interface LibraryPage {
  items: LibraryItem[];
  total: number;
  page: number;
  pages: number;
  per_page: number;
  thumbnail_size: number;
  roots: string[];
}

/** The `library.list` parameters the UI sets; unset keys are omitted. */
export interface LibraryQuery {
  filter?: string;
  dir?: string;
  kind?: string;
  page?: number;
  per_page?: number;
  scan_if_empty?: boolean;
}

/** What a scan did. */
export interface ScanSummary {
  summary: string;
  roots: string[];
  added: number;
  updated: number;
  removed: number;
  unchanged: number;
  skipped_unsupported: number;
  rows_touched: number;
  files_seen: number;
  missing_roots: string[];
  duration_ms: number;
}

/** A materialised thumbnail. The PNG arrives inline; the webview opens no files. */
export interface Thumbnail {
  id: number;
  path: string;
  size: number;
  cached: boolean;
  source: string;
  data_url: string;
}

/** One output. */
export interface OutputView {
  name: string;
  description: string;
  width: number;
  height: number;
  focused: boolean;
  /** What this daemon run has actually presented. */
  wallpaper: string | null;
  kind: string | null;
  state: string;
  /** Recorded in the session file but not applied this run. */
  recorded: string | null;
  error: string | null;
}

export interface OutputsView {
  outputs: OutputView[];
  paused: boolean;
}

export interface ApplyOutcome {
  reference: string;
  kind: string;
  outputs: string[];
  notes: string[];
}

/** A transition request: exactly the daemon's `{name, duration_ms, fps}` table. */
export interface TransitionRequest {
  name: string;
  duration_ms: number;
  fps: number;
}

function disconnected(error: string, socketPath = ""): DaemonStatus {
  return {
    connected: false,
    socket_path: socketPath,
    server_version: null,
    schema: null,
    shell_backends: [],
    content_kinds: [],
    media_backends: [],
    transitions: [],
    unavailable: [],
    error,
  };
}

/** Ask the daemon for its status (a real `hello` handshake over the IPC socket).
 * Never throws: an unreachable daemon is a normal, displayable state.
 */
export async function daemonStatus(): Promise<DaemonStatus> {
  try {
    return await invoke<DaemonStatus>("daemon_status");
  } catch (error) {
    return disconnected(String(error));
  }
}

/**
 * Ask the daemon which shell backend is live and why (`shell.status`).
 * Never throws, for the same reason `daemonStatus` does not: "cannot reach the
 * daemon" is a state the card displays, not an error dialog.
 */
export async function shellStatus(): Promise<ShellStatus> {
  try {
    return await invoke<ShellStatus>("shell_status");
  } catch (error) {
    return {
      connected: false,
      backend: null,
      reason: null,
      mode: "daemon-drawn",
      routed: false,
      patched: false,
      detect_order: [],
      backends: [],
      events: null,
      competing_tools: [],
      runtime_note: null,
      error: String(error),
    };
  }
}

/** Change the shell backend, draw mode or theme hook at runtime. */
export function patchShell(patch: ShellPatch): Promise<ShellStatus> {
  return invoke<ShellStatus>("patch_shell", { patch });
}

/** Normalise anything thrown by `invoke` into an `IpcFailure`. */
export function asFailure(error: unknown): IpcFailure {
  if (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    "message" in error
  ) {
    const failure = error as IpcFailure;
    return { code: String(failure.code), message: String(failure.message) };
  }
  return { code: "unknown", message: String(error) };
}

/** Ask the daemon for a page of its wallpaper index. */
export function libraryIndex(query: LibraryQuery): Promise<LibraryPage> {
  return invoke<LibraryPage>("library_index", { query });
}

/** Ask the daemon to rescan its configured roots (or the given ones). */
export function libraryScan(paths: string[] | null = null): Promise<ScanSummary> {
  return invoke<ScanSummary>("library_scan", { paths });
}

/** Fetch one thumbnail as a data URL (the daemon generates it on first ask). */
export function libraryThumbnail(id: number): Promise<Thumbnail> {
  return invoke<Thumbnail>("library_thumbnail", { id });
}

/** Ask the daemon about its outputs. */
export function listOutputs(): Promise<OutputsView> {
  return invoke<OutputsView>("list_outputs");
}

/**
 * Apply a wallpaper reference to specific outputs, or to every output when
 * `outputs` is empty.
 */
export function assignWallpaper(
  reference: string,
  outputs: string[] = [],
  transition: TransitionRequest | null = null,
): Promise<ApplyOutcome> {
  return invoke<ApplyOutcome>("assign_wallpaper", {
    reference,
    outputs,
    transition,
  });
}

/** Remove the wallpaper from one output, or from every output. */
export function clearWallpaper(output: string | null = null): Promise<string[]> {
  return invoke<string[]>("clear_wallpaper", { output });
}

export { disconnected };
