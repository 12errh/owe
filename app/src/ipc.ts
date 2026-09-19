// Typed wrappers over the Rust commands. Components never call `invoke` directly:
// keeping the raw command names and wire shapes in one file is what makes the
// daemon-facing surface reviewable (ARCHITECTURE §1: the GUI owns no state, it
// only asks the daemon).

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
  error: string | null;
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

export { disconnected };
