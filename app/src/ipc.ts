// Typed wrappers over the Rust commands. Components never call `invoke` directly:
// keeping the raw command names and wire shapes in one file is what makes the
// daemon-facing surface reviewable (ARCHITECTURE §1: the GUI owns no state, it
// only asks the daemon).
//
// Every function here returns a plain value or throws an `IpcFailure`. Nothing
// caches and nothing polls — the UI does work when the user asks for it, which is
// the whole point of a resource-first tool.

import { invoke } from "@tauri-apps/api/core";

/** Mirrors `DaemonStatus` in `app/src-tauri/src/main.rs` (serde snake_case). */
export interface DaemonStatus {
  connected: boolean;
  socket_path: string;
  server_version: string | null;
  schema: string | null;
  shell_backends: string[];
  content_kinds: string[];
  media_backends: string[];
  /** Planned-but-missing features, as `"<id>: <why>"`. */
  unavailable: string[];
  error: string | null;
}

/** Mirrors `IpcError`: a protocol code, or `unreachable`/`protocol`. */
export interface IpcFailure {
  code: string;
  message: string;
}

/** One wallpaper file. */
export interface WallpaperEntry {
  path: string;
  name: string;
  bytes: number;
}

/** A directory listing, with the scope the daemon actually used. */
export interface LibraryListing {
  dir: string;
  entries: WallpaperEntry[];
  scope: string;
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
  /** Recorded in the session file but not applied this run (restore is P2). */
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

function disconnected(error: string, socketPath = ""): DaemonStatus {
  return {
    connected: false,
    socket_path: socketPath,
    server_version: null,
    schema: null,
    shell_backends: [],
    content_kinds: [],
    media_backends: [],
    unavailable: [],
    error,
  };
}

/**
 * Ask the daemon for its status (a real `hello` handshake over the IPC socket).
 * Never throws: an unreachable daemon is a normal, displayable state.
 */
export async function daemonStatus(): Promise<DaemonStatus> {
  try {
    return await invoke<DaemonStatus>("daemon_status");
  } catch (error) {
    return disconnected(String(error));
  }
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

/** List the image files the daemon can see in `dir`. */
export function listWallpapers(dir: string): Promise<LibraryListing> {
  return invoke<LibraryListing>("list_wallpapers", { dir });
}

/** Ask the daemon about its outputs. */
export function listOutputs(): Promise<OutputsView> {
  return invoke<OutputsView>("list_outputs");
}

/** Apply a wallpaper to one output, or to every output when `output` is null. */
export function applyWallpaper(
  path: string,
  output: string | null = null,
): Promise<ApplyOutcome> {
  return invoke<ApplyOutcome>("apply_wallpaper", { path, output });
}

/** Remove the wallpaper from one output, or from every output. */
export function clearWallpaper(output: string | null = null): Promise<string[]> {
  return invoke<string[]>("clear_wallpaper", { output });
}

export { disconnected };
