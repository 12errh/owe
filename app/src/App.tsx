import { useCallback, useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";

import {
  applyWallpaper,
  asFailure,
  clearWallpaper,
  daemonStatus,
  listOutputs,
  listWallpapers,
  type DaemonStatus,
  type IpcFailure,
  type LibraryListing,
  type OutputsView,
} from "./ipc";
import { t, tList } from "./i18n";

// P1 GUI: status bar, outputs, folder picker, wallpaper list, apply. The "simple
// UI" contract of UI-DESIGN §3 — one column, real states, no styling framework
// (the visual design pass replaces this file's markup, not its data flow).
//
// Deliberately no timers and no polling: the window does work when the user asks
// it to. Live updates arrive with the daemon's event bus.

export default function App() {
  const [status, setStatus] = useState<DaemonStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [library, setLibrary] = useState<LibraryListing | null>(null);
  const [outputs, setOutputs] = useState<OutputsView | null>(null);
  const [failure, setFailure] = useState<IpcFailure | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setBusy(true);
    try {
      const next = await daemonStatus();
      setStatus(next);
      if (next.connected) {
        const [outputsView, listing] = await Promise.all([
          listOutputs().catch((error) => {
            setFailure(asFailure(error));
            return null;
          }),
          Promise.resolve(null),
        ]);
        setOutputs(outputsView);
        if (listing) setLibrary(listing);
      } else {
        setOutputs(null);
      }
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const run = useCallback(
    async (action: () => Promise<string>) => {
      setBusy(true);
      setFailure(null);
      setNotice(null);
      try {
        setNotice(await action());
        setOutputs(await listOutputs().catch(() => null));
      } catch (error) {
        setFailure(asFailure(error));
      } finally {
        setBusy(false);
      }
    },
    [],
  );

  const chooseFolder = useCallback(async () => {
    const picked = await open({ directory: true, multiple: false });
    if (typeof picked !== "string") return;
    setBusy(true);
    setFailure(null);
    setNotice(null);
    try {
      setLibrary(await listWallpapers(picked));
    } catch (error) {
      setFailure(asFailure(error));
    } finally {
      setBusy(false);
    }
  }, []);

  return (
    <main className="app">
      <header className="app__header">
        <h1 className="app__title">{t("appTitle")}</h1>
        <p className="app__tagline">{t("appTagline")}</p>
      </header>

      <section className="status" aria-live="polite">
        <StatusLine status={status} busy={busy} />
        {status && !status.connected && (
          <p className="status__hint">{t("statusHintDisconnected")}</p>
        )}
        <button
          type="button"
          className="status__refresh"
          onClick={() => void refresh()}
          disabled={busy}
        >
          {busy ? t("refreshing") : t("refresh")}
        </button>
      </section>

      {failure && (
        <p className="alert alert--error" role="alert">
          {t("failureLabel")}: <code>{failure.code}</code> — {failure.message}
        </p>
      )}
      {notice && <p className="alert alert--ok">{notice}</p>}

      {status?.connected && (
        <>
          <section className="panel">
            <h2 className="panel__title">{t("outputsTitle")}</h2>
            {outputs === null ? (
              <p className="panel__empty">{t("outputsUnavailable")}</p>
            ) : outputs.outputs.length === 0 ? (
              <p className="panel__empty">{t("outputsNone")}</p>
            ) : (
              <ul className="list">
                {outputs.outputs.map((output) => (
                  <li key={output.name} className="list__row">
                    <div className="list__main">
                      <span className="list__name">
                        {output.name} · {output.width}×{output.height}
                        {output.focused ? ` · ${t("focusedBadge")}` : ""}
                      </span>
                      <span className="list__sub">
                        {output.wallpaper
                          ? `${t("wallpaperLabel")}: ${output.wallpaper}`
                          : t("wallpaperNone")}
                      </span>
                      {output.recorded && (
                        <span className="list__sub">
                          {t("recordedLabel")}: {output.recorded} — {t("recordedHint")}
                        </span>
                      )}
                      <span className="list__sub">
                        {t("stateLabel")}: {output.state}
                      </span>
                      {output.error && (
                        <span className="list__sub list__sub--error">
                          {output.error}
                        </span>
                      )}
                    </div>
                    <button
                      type="button"
                      className="button"
                      disabled={busy || !output.wallpaper}
                      onClick={() =>
                        void run(async () => {
                          const cleared = await clearWallpaper(output.name);
                          return cleared.length > 0
                            ? `${t("clearedLabel")}: ${cleared.join(", ")}`
                            : t("clearNothing");
                        })
                      }
                    >
                      {t("clear")}
                    </button>
                  </li>
                ))}
              </ul>
            )}
            {outputs && outputs.paused && (
              <p className="panel__note">{t("governorPaused")}</p>
            )}
          </section>

          <section className="panel">
            <h2 className="panel__title">{t("libraryTitle")}</h2>
            <div className="panel__actions">
              <button
                type="button"
                className="button button--primary"
                onClick={() => void chooseFolder()}
                disabled={busy}
              >
                {t("chooseFolder")}
              </button>
              {library && (
                <span className="panel__note">
                  {t("scopeLabel")}: {library.scope}
                </span>
              )}
            </div>

            {library === null ? (
              <p className="panel__empty">{t("libraryEmpty")}</p>
            ) : library.entries.length === 0 ? (
              <p className="panel__empty">{t("libraryNoneInFolder")}</p>
            ) : (
              <>
                <p className="panel__path">{library.dir}</p>
                <ul className="list">
                  {library.entries.map((entry) => (
                    <li key={entry.path} className="list__row">
                      <div className="list__main">
                        <span className="list__name">{entry.name}</span>
                        <span className="list__sub list__sub--mono">
                          {entry.path}
                        </span>
                      </div>
                      <button
                        type="button"
                        className="button button--primary"
                        disabled={busy}
                        onClick={() =>
                          void run(async () => {
                            const applied = await applyWallpaper(entry.path);
                            const where =
                              applied.outputs.length > 0
                                ? applied.outputs.join(", ")
                                : t("noOutputsMatched");
                            const notes =
                              applied.notes.length > 0
                                ? ` (${applied.notes.join("; ")})`
                                : "";
                            return `${t("appliedLabel")}: ${applied.reference} → ${where}${notes}`;
                          })
                        }
                      >
                        {t("apply")}
                      </button>
                    </li>
                  ))}
                </ul>
              </>
            )}
          </section>

          <section className="details">
            <Detail label={t("versionLabel")} value={status.server_version ?? "—"} />
            <Detail label={t("schemaLabel")} value={status.schema ?? "—"} />
            <Detail label={t("socketLabel")} value={status.socket_path} mono />
            <Detail
              label={t("shellBackendsLabel")}
              value={tList(status.shell_backends, "none")}
            />
            <Detail
              label={t("contentKindsLabel")}
              value={tList(status.content_kinds, "none")}
            />
            <Detail
              label={t("mediaBackendsLabel")}
              value={tList(status.media_backends, "none")}
            />
          </section>

          {status.unavailable.length > 0 && (
            <section className="panel panel--quiet">
              <h2 className="panel__title">{t("unavailableTitle")}</h2>
              <ul className="list list--plain">
                {status.unavailable.map((entry) => (
                  <li key={entry} className="list__sub">
                    {entry}
                  </li>
                ))}
              </ul>
            </section>
          )}
        </>
      )}

      <p className="placeholder__note">{t("disposableNote")}</p>
    </main>
  );
}

function StatusLine({ status, busy }: { status: DaemonStatus | null; busy: boolean }) {
  const state = busy
    ? "unknown"
    : status === null
      ? "unknown"
      : status.connected
        ? "connected"
        : "disconnected";

  const text =
    state === "connected"
      ? `${t("daemonLabel")}: ${t("daemonConnected")}`
      : state === "disconnected"
        ? `${t("daemonLabel")}: ${t("daemonDisconnected")}`
        : `${t("daemonLabel")}: ${t("daemonUnknown")}`;

  return (
    <p className={`status__line status__line--${state}`}>
      <span className="status__dot" aria-hidden="true" />
      <span>{text}</span>
      {status?.error && <span className="status__error">— {status.error}</span>}
    </p>
  );
}

function Detail({
  label,
  value,
  mono = false,
}: {
  label: string;
  value: string;
  mono?: boolean;
}) {
  return (
    <div className="detail">
      <span className="detail__label">{label}</span>
      <span className={mono ? "detail__value detail__value--mono" : "detail__value"}>
        {value}
      </span>
    </div>
  );
}
