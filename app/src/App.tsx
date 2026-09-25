import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  assignWallpaper,
  asFailure,
  clearWallpaper,
  daemonStatus,
  getStats,
  libraryIndex,
  libraryScan,
  libraryThumbnail,
  listOutputs,
  patchShell,
  playbackCommand,
  shellStatus,
  type ApplyOutcome,
  type DaemonStatus,
  type IpcFailure,
  type LibraryItem,
  type LibraryPage,
  type OutputsView,
  type PlaybackCommand,
  type PlaybackResult,
  type ShellStatus,
  type ShellPatch,
  type StatsRow,
  type StatsView,
  type TransitionRequest,
} from "./ipc";
import { t, tList } from "./i18n";

// GUI v1 (IMPLEMENTATION-PLAN Phase 2): a library grid with thumbnails, per-output
// assignment, a transition picker, and apply-all — still the "simple UI" contract of
// UI-DESIGN §3 (one column, real states, no styling framework; the visual redesign
// wave replaces this markup, not its data flow).
//
// The data-flow rules from P1 still hold, and they are what keep this honest:
//   * the daemon owns every piece of state — this component caches only what it has
//     already displayed (thumbnails), and closing the window loses nothing;
//   * no timers and no polling — work happens when the user asks for it, and
//     thumbnails are fetched once per item because the daemon generates them;
//   * every user-visible string goes through `t()`.

/** Thumbnail cache sentinel for "the daemon could not make one" (distinct from
 * "not asked yet", which is an absent key). */
const NO_THUMBNAIL = "";

export default function App() {
  const [status, setStatus] = useState<DaemonStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<IpcFailure | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const [outputs, setOutputs] = useState<OutputsView | null>(null);
  const [stats, setStats] = useState<StatsView | null>(null);
  const [shell, setShell] = useState<ShellStatus | null>(null);

  const [page, setPage] = useState<LibraryPage | null>(null);
  const [pageNumber, setPageNumber] = useState(1);
  const [filterInput, setFilterInput] = useState("");
  const [filter, setFilter] = useState("");
  const [kind, setKind] = useState("");
  const [loadingLibrary, setLoadingLibrary] = useState(false);

  const [selected, setSelected] = useState<LibraryItem | null>(null);
  const [transitionName, setTransitionName] = useState("");
  const [transitionDuration, setTransitionDuration] = useState(300);
  const [thumbnails, setThumbnails] = useState<Record<number, string>>({});

  // Bumped whenever a new scan/filter should replace the in-flight request, so a
  // slow reply from an older query can never overwrite a newer one.
  const request = useRef(0);

  const loadLibrary = useCallback(
    async (next: { page?: number; filter?: string; kind?: string; scan?: boolean }) => {
      const ticket = ++request.current;
      setLoadingLibrary(true);
      try {
        const query: Parameters<typeof libraryIndex>[0] = {
          page: next.page ?? (pageNumber || 1),
          per_page: 60,
          // The first listing scans once when the index is empty, so a fresh install
          // shows a library instead of an empty grid.
          scan_if_empty: next.scan ?? (page === null ? true : undefined),
        };
        const nextFilter = next.filter ?? filter;
        if (nextFilter) query.filter = nextFilter;
        const nextKind = next.kind ?? kind;
        if (nextKind) query.kind = nextKind;

        const loaded = await libraryIndex(query);
        if (ticket !== request.current) return;
        setPage(loaded);
        setPageNumber(loaded.page);
      } catch (error) {
        if (ticket !== request.current) return;
        setFailure(asFailure(error));
      } finally {
        if (ticket === request.current) setLoadingLibrary(false);
      }
    },
    [filter, kind, page, pageNumber],
  );

  const refresh = useCallback(async () => {
    setBusy(true);
    try {
      const next = await daemonStatus();
      setStatus(next);
      if (next.connected) {
        setOutputs(await listOutputs().catch(() => null));
        setStats(await getStats().catch(() => null));
        setShell(await shellStatus().catch(() => null));
        await loadLibrary({ page: 1, scan: true });
      } else {
        setOutputs(null);
        setStats(null);
        setShell(null);
        setPage(null);
      }
    } finally {
      setBusy(false);
    }
  }, [loadLibrary]);

  useEffect(() => {
    void refresh();
    // Once, on mount: the window does work when it opens, then not again until the
    // user asks. `refresh` is stable enough (it only depends on the query helpers).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Adopt the daemon's transition catalogue instead of hardcoding one, and only
  // once — the user's choice must not be overwritten by a later status refresh.
  useEffect(() => {
    if (!status) return;
    setTransitionName((current) => {
      if (current && status.transitions.includes(current)) return current;
      if (status.transitions.includes("fade")) return "fade";
      return status.transitions[0] ?? "";
    });
  }, [status]);

  // Fetch thumbnails for the items in view that have none yet. Sequential on
  // purpose: the daemon decodes on demand, and a dozen parallel decodes is how a
  // low-resource tool stops being one.
  useEffect(() => {
    if (!page) return;
    const missing = page.items.filter((item) => thumbnails[item.id] === undefined);
    if (missing.length === 0) return;

    let cancelled = false;
    void (async () => {
      for (const item of missing) {
        try {
          const thumbnail = await libraryThumbnail(item.id);
          if (cancelled) return;
          setThumbnails((current) => ({ ...current, [item.id]: thumbnail.data_url }));
        } catch {
          // A file that cannot be decoded is shown as "no preview", never as an
          // error dialog: one broken image must not blank the grid.
          if (cancelled) return;
          setThumbnails((current) => ({ ...current, [item.id]: NO_THUMBNAIL }));
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [page, thumbnails]);

  const transition = useMemo<TransitionRequest | null>(() => {
    if (!transitionName) return null;
    return { name: transitionName, duration_ms: transitionDuration, fps: 60 };
  }, [transitionName, transitionDuration]);

  const run = useCallback(
    async (action: () => Promise<string>) => {
      setBusy(true);
      setFailure(null);
      setNotice(null);
      try {
        setNotice(await action());
        setOutputs(await listOutputs().catch(() => null));
        setStats(await getStats().catch(() => null));
      } catch (error) {
        setFailure(asFailure(error));
      } finally {
        setBusy(false);
      }
    },
    [],
  );

  const applySelected = useCallback(
    (targets: string[]) =>
      run(async () => {
        if (!selected) return t("selectionNone");
        const applied: ApplyOutcome = await assignWallpaper(
          selected.reference,
          targets,
          transition,
        );
        const where =
          applied.outputs.length > 0
            ? applied.outputs.join(", ")
            : t("noOutputsMatched");
        const notes =
          applied.notes.length > 0 ? ` (${applied.notes.join("; ")})` : "";
        return `${t("appliedLabel")}: ${selected.name} → ${where}${notes}`;
      }),
    [run, selected, transition],
  );

  const rescan = useCallback(
    () =>
      run(async () => {
        const summary = await libraryScan(null);
        const missing =
          summary.missing_roots.length > 0
            ? ` — ${t("scanMissingRoots")}: ${summary.missing_roots.join(", ")}`
            : "";
        // A scan changes the page contents, so reload from the top.
        await loadLibrary({ page: 1 });
        return `${t("scanSummaryLabel")}: ${summary.summary}${missing}`;
      }),
    [run, loadLibrary],
  );

  const applyShellPatch = useCallback(
    (patch: ShellPatch) =>
      run(async () => {
        const next = await patchShell(patch);
        setShell(next);
        return next.runtime_note ?? t("shellApplied");
      }),
    [run],
  );

  const refreshStats = useCallback(
    () =>
      run(async () => {
        const next = await getStats();
        setStats(next);
        return t("statsRefreshed");
      }),
    [run],
  );

  const sendPlayback = useCallback(
    (output: string, command: PlaybackCommand) =>
      run(async () => {
        const result: PlaybackResult = await playbackCommand(output, command);
        const state = result.states[0];
        return state
          ? `${t("playbackUpdated")} ${state.output} · ${state.decode}`
          : t("statsNoRows");
      }),
    [run],
  );

  const canAssign = selected !== null && !busy;

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

      {/* `status && status.connected` rather than a `connected` boolean: the direct
          check is what lets TypeScript narrow `status` for everything below. */}
      {status && status.connected && (
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
                          {t("recordedLabel")}: {output.recorded} —{" "}
                          {t("recordedHint")}
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
                    <div className="list__actions">
                      <button
                        type="button"
                        className="button button--primary"
                        title={t("assignHereHint")}
                        disabled={!canAssign}
                        onClick={() => void applySelected([output.name])}
                      >
                        {t("assignHere")}
                      </button>
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
                    </div>
                  </li>
                ))}
              </ul>
            )}
            {outputs && outputs.paused && (
              <p className="panel__note">{t("governorPaused")}</p>
            )}
          </section>

          <StatsPanel
            stats={stats}
            busy={busy}
            onRefresh={() => void refreshStats()}
            onCommand={(output, command) => void sendPlayback(output, command)}
          />

          <ShellCard
            shell={shell}
            busy={busy}
            onPatch={(patch) => void applyShellPatch(patch)}
          />

          <section className="panel">
            <h2 className="panel__title">{t("libraryTitle")}</h2>

            <div className="panel__actions">
              <form
                className="search"
                onSubmit={(event) => {
                  event.preventDefault();
                  setFilter(filterInput);
                  void loadLibrary({ page: 1, filter: filterInput });
                }}
              >
                <input
                  className="search__input"
                  type="search"
                  value={filterInput}
                  placeholder={t("librarySearchPlaceholder")}
                  onChange={(event) => setFilterInput(event.target.value)}
                />
                <button type="submit" className="button" disabled={loadingLibrary}>
                  {t("librarySearchPlaceholder")}
                </button>
              </form>

              <label className="field">
                <span className="field__label">{t("contentKindsLabel")}</span>
                <select
                  className="field__control"
                  value={kind}
                  onChange={(event) => {
                    setKind(event.target.value);
                    void loadLibrary({ page: 1, kind: event.target.value });
                  }}
                >
                  <option value="">{t("kindAll")}</option>
                  {(status?.content_kinds ?? []).map((value) => (
                    <option key={value} value={value}>
                      {value}
                    </option>
                  ))}
                </select>
              </label>

              <label className="field">
                <span className="field__label">{t("transitionLabel")}</span>
                <select
                  className="field__control"
                  value={transitionName}
                  disabled={(status?.transitions.length ?? 0) === 0}
                  onChange={(event) => setTransitionName(event.target.value)}
                >
                  {(status?.transitions ?? []).map((value) => (
                    <option key={value} value={value}>
                      {value}
                    </option>
                  ))}
                </select>
              </label>

              <label className="field">
                <span className="field__label">{t("transitionDuration")}</span>
                <input
                  className="field__control field__control--number"
                  type="number"
                  min={0}
                  max={5000}
                  step={50}
                  value={transitionDuration}
                  onChange={(event) =>
                    setTransitionDuration(Number(event.target.value) || 0)
                  }
                />
              </label>

              <button
                type="button"
                className="button"
                disabled={busy || loadingLibrary}
                onClick={() => void rescan()}
              >
                {loadingLibrary ? t("scanning") : t("scan")}
              </button>
            </div>

            {(status?.transitions.length ?? 0) === 0 && (
              <p className="panel__note">{t("transitionUnavailable")}</p>
            )}

            <p className="panel__note">
              {t("libraryRootsLabel")}:{" "}
              {page && page.roots.length > 0
                ? page.roots.join(", ")
                : t("libraryNoRoots")}
            </p>

            <div className="selection">
              <span className="selection__text">
                {selected
                  ? `${t("selectionLabel")}: ${selected.name}`
                  : t("selectionNone")}
              </span>
              <button
                type="button"
                className="button button--primary"
                disabled={!canAssign}
                onClick={() => void applySelected([])}
              >
                {t("applyAll")}
              </button>
            </div>

            {page === null ? (
              <p className="panel__empty">{t("libraryEmpty")}</p>
            ) : page.items.length === 0 ? (
              <p className="panel__empty">{t("libraryNoneMatching")}</p>
            ) : (
              <>
                <ul className="grid">
                  {page.items.map((item) => {
                    const thumbnail = thumbnails[item.id];
                    return (
                      <li key={item.id}>
                        <button
                          type="button"
                          className={
                            selected?.id === item.id
                              ? "card card--selected"
                              : "card"
                          }
                          aria-pressed={selected?.id === item.id}
                          onClick={() => setSelected(item)}
                        >
                          <span className="card__thumb">
                            {thumbnail === undefined ? (
                              <span className="card__placeholder">
                                {t("thumbnailsPending")}
                              </span>
                            ) : thumbnail === NO_THUMBNAIL ? (
                              <span className="card__placeholder">
                                {t("thumbnailUnavailable")}
                              </span>
                            ) : (
                              <img
                                className="card__image"
                                src={thumbnail}
                                alt=""
                                loading="lazy"
                              />
                            )}
                          </span>
                          <span className="card__name" title={item.path}>
                            {item.name}
                          </span>
                          <span className="card__meta">
                            {Math.max(1, Math.round(item.bytes / 1024))} KiB ·{" "}
                            {item.kind}
                          </span>
                        </button>
                      </li>
                    );
                  })}
                </ul>

                <div className="pager">
                  <button
                    type="button"
                    className="button"
                    disabled={loadingLibrary || page.page <= 1}
                    onClick={() => void loadLibrary({ page: page.page - 1 })}
                  >
                    {t("previousPage")}
                  </button>
                  <span className="pager__label">
                    {t("pageLabel")} {page.page} {t("ofLabel")}{" "}
                    {Math.max(page.pages, 1)} · {t("totalsLabel")}: {page.total}
                  </span>
                  <button
                    type="button"
                    className="button"
                    disabled={loadingLibrary || Number(page.page) >= Number(page.pages)}
                    onClick={() => void loadLibrary({ page: page.page + 1 })}
                  >
                    {t("nextPage")}
                  </button>
                </div>
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
            <Detail
              label={t("ipcEventsLabel")}
              value={status.events.length > 0 ? status.events.join(", ") : t("none")}
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

function StatsPanel({
  stats,
  busy,
  onRefresh,
  onCommand,
}: {
  stats: StatsView | null;
  busy: boolean;
  onRefresh: () => void;
  onCommand: (output: string, command: PlaybackCommand) => void;
}) {
  return (
    <section className="panel">
      <div className="panel__heading">
        <h2 className="panel__title">{t("statsTitle")}</h2>
        <button
          type="button"
          className="button"
          disabled={busy}
          onClick={onRefresh}
        >
          {busy ? t("statsRefreshing") : t("statsRefresh")}
        </button>
      </div>
      {stats === null ? (
        <p className="panel__empty">{t("statsUnavailable")}</p>
      ) : (
        <>
          <div className="stats__summary">
            <Detail
              label={t("statsRss")}
              value={stats.rss_bytes === null ? t("statsRssUnavailable") : formatBytes(stats.rss_bytes)}
            />
            <Detail
              label={t("statsGovernor")}
              value={stats.paused ? t("statsGovernorPaused") : t("statsGovernorRunning")}
            />
          </div>
          {stats.stats.length === 0 ? (
            <p className="panel__empty">{t("statsNoRows")}</p>
          ) : (
            <ul className="list">
              {stats.stats.map((row) => (
                <PlaybackRow
                  key={row.output}
                  row={row}
                  busy={busy}
                  onCommand={onCommand}
                />
              ))}
            </ul>
          )}
        </>
      )}
    </section>
  );
}

function PlaybackRow({
  row,
  busy,
  onCommand,
}: {
  row: StatsRow;
  busy: boolean;
  onCommand: (output: string, command: PlaybackCommand) => void;
}) {
  const [seek, setSeek] = useState(0);
  const [loopStart, setLoopStart] = useState(0);
  const [loopEnd, setLoopEnd] = useState(1);
  const controllable = row.mode !== "" && row.failed === null;
  const decode =
    row.decode === "hardware"
      ? t("statsDecodeHardware")
      : row.decode === "software"
        ? t("statsDecodeSoftware")
        : t("statsDecodeUnknown");

  return (
    <li className="stats-row">
      <div className="stats-row__main">
        <div className="stats-row__title">
          <strong>{row.output}</strong>
          <span className={`badge badge--${row.decode || "unknown"}`}>{decode}</span>
          <span className="stats-row__state">
            {row.playing ? "▶" : row.held ? "Ⅱ" : "·"}
          </span>
        </div>
        <div className="stats-row__meta">
          {t("statsDecode")}: {decode} · {t("statsFps")}: {row.fps > 0 ? row.fps.toFixed(2) : "—"} · {t("statsPosition")}:{" "}
          {row.position_ms > 0 ? `${(row.position_ms / 1000).toFixed(2)} s` : t("playbackPositionUnavailable")} ·{" "}
          {t("statsBuffers")}: {row.buffers} ({formatBytes(row.buffer_bytes)}) · {t("statsFrames")}:{" "}
          {row.frames_presented}
        </div>
        {row.mode !== "" && (
          <div className="stats-row__meta">
            {t("statsMode")}: {row.mode} · {row.decoder || "—"}
          </div>
        )}
        {row.failed && (
          <div className="list__sub list__sub--error">
            {t("statsFailed")}: {row.failed}
          </div>
        )}
        {!controllable && <div className="list__sub">{t("playbackUnavailable")}</div>}
      </div>
      <div className="playback-controls">
        <button
          type="button"
          className="button"
          disabled={busy || !controllable || row.playing}
          onClick={() => onCommand(row.output, "play")}
        >
          {t("playbackPlay")}
        </button>
        <button
          type="button"
          className="button"
          disabled={busy || !controllable || !row.playing}
          onClick={() => onCommand(row.output, "pause")}
        >
          {t("playbackPause")}
        </button>
        <label className="playback-controls__field">
          <span>{t("playbackSeek")} ({t("playbackSeekSeconds")})</span>
          <input
            type="number"
            min="0"
            step="0.1"
            value={seek}
            onChange={(event) => setSeek(Number(event.target.value) || 0)}
          />
        </label>
        <button
          type="button"
          className="button"
          disabled={busy || !controllable}
          onClick={() => onCommand(row.output, { seek })}
        >
          {t("playbackSeek")}
        </button>
        <label className="playback-controls__field">
          <span>{t("playbackLoopStart")}</span>
          <input
            type="number"
            min="0"
            step="0.1"
            value={loopStart}
            onChange={(event) => setLoopStart(Number(event.target.value) || 0)}
          />
        </label>
        <label className="playback-controls__field">
          <span>{t("playbackLoopEnd")}</span>
          <input
            type="number"
            min="0"
            step="0.1"
            value={loopEnd}
            onChange={(event) => setLoopEnd(Number(event.target.value) || 0)}
          />
        </label>
        <button
          type="button"
          className="button"
          disabled={busy || !controllable || loopEnd <= loopStart}
          onClick={() => onCommand(row.output, { loop: { a: loopStart, b: loopEnd } })}
        >
          {t("playbackApplyLoop")}
        </button>
      </div>
    </li>
  );
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1_048_576) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1_048_576).toFixed(1)} MiB`;
}

/// The shell card (Phase 3, FR-SHELL-3): which backend is live, why, and the
/// runtime switches. Every value shown comes from `shell.status`; the only string
/// the UI invents is the placeholder for an unreachable daemon.
function ShellCard({
  shell,
  busy,
  onPatch,
}: {
  shell: ShellStatus | null;
  busy: boolean;
  onPatch: (patch: ShellPatch) => void;
}) {
  const [backendOverride, setBackendOverride] = useState("");
  const [modeChoice, setModeChoice] = useState("");
  const [themeHook, setThemeHook] = useState("");
  const [orderInput, setOrderInput] = useState("");

  if (shell === null) {
    return (
      <section className="panel">
        <h2 className="panel__title">{t("shellTitle")}</h2>
        <p className="panel__empty">{t("shellCardUnreachable")}</p>
      </section>
    );
  }

  if (!shell.connected) {
    return (
      <section className="panel">
        <h2 className="panel__title">{t("shellTitle")}</h2>
        <p className="panel__empty">{shell.error ?? t("shellCardUnreachable")}</p>
      </section>
    );
  }

  const patch: ShellPatch = {};
  if (backendOverride !== "") patch.backend = backendOverride;
  if (modeChoice !== "") patch.caelestia_mode = modeChoice;
  if (themeHook === "on") patch.theme_hook = true;
  if (themeHook === "off") patch.theme_hook = false;
  const order = orderInput
    .split(",")
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);
  if (order.length > 0) patch.detect_order = order;
  const patchEmpty = Object.keys(patch).length === 0;

  const modeText =
    shell.mode === "shell-routed" ? t("shellModeShellRouted") : t("shellModeDaemonDrawn");

  return (
    <section className="panel">
      <h2 className="panel__title">{t("shellTitle")}</h2>

      <div className="details details--tight">
        <Detail
          label={t("shellBackendLabel")}
          value={shell.backend ?? t("shellNoneSelected")}
        />
        <Detail label={t("shellModeLabel")} value={modeText} />
        {shell.patched && <Detail label=" " value={t("shellPatched")} />}
        {shell.detect_order.length > 0 && (
          <Detail label={t("shellDetectOrder")} value={shell.detect_order.join(" → ")} mono />
        )}
        {shell.events && (
          <Detail
            label={t("shellEventsLabel")}
            value={`${
              shell.events.listening ? t("shellEventsListening") : t("shellEventsIdle")
            } — ${shell.events.published} ${t("shellEventsSeen")}, ${
              shell.events.dropped
            } ${t("shellEventsDropped")}, ${shell.events.reconnects} ${t(
              "shellEventsReconnects",
            )}`}
          />
        )}
      </div>

      <ul className="list list--plain" title={t("shellRowHint")}>
        {shell.backends.map((row) => (
          <li key={row.id} className="list__sub">
            <span aria-hidden="true">{row.selected ? "●" : "○"} </span>
            <code>{row.id}</code> · {row.confidence} · {row.mode} — {row.reason}
          </li>
        ))}
      </ul>

      {shell.competing_tools.length > 0 && (
        <div className="panel__note">
          <strong>{t("shellCompetingTitle")}</strong> {t("shellCompetingHint")}
          <ul className="list list--plain">
            {shell.competing_tools.map((notice) => (
              <li key={notice} className="list__sub">
                {notice}
              </li>
            ))}
          </ul>
        </div>
      )}

      <form
        className="panel__actions"
        onSubmit={(event) => {
          event.preventDefault();
          if (!patchEmpty) onPatch(patch);
        }}
      >
        <label className="field">
          <span className="field__label">{t("shellBackendOverride")}</span>
          <select
            className="field__control"
            value={backendOverride}
            onChange={(event) => setBackendOverride(event.target.value)}
          >
            <option value="">{t("shellBackendAuto")}</option>
            {shell.detect_order.map((id) => (
              <option key={id} value={id}>
                {id}
              </option>
            ))}
          </select>
        </label>

        <label className="field">
          <span className="field__label">{t("shellModeLabel2")}</span>
          <select
            className="field__control"
            value={modeChoice}
            onChange={(event) => setModeChoice(event.target.value)}
          >
            <option value="">{modeText}</option>
            <option value="daemon-drawn">{t("shellModeDaemonDrawnChoice")}</option>
            <option value="shell-routed">{t("shellModeShellRoutedChoice")}</option>
          </select>
        </label>

        <label className="field">
          <span className="field__label">{t("shellThemeHookLabel")}</span>
          <select
            className="field__control"
            value={themeHook}
            onChange={(event) => setThemeHook(event.target.value)}
          >
            <option value="">—</option>
            <option value="on">{t("shellThemeHookOn")}</option>
            <option value="off">{t("shellThemeHookOff")}</option>
          </select>
        </label>

        <label className="field field--wide">
          <span className="field__label">{t("shellDetectOrder")}</span>
          <input
            className="field__control"
            type="text"
            value={orderInput}
            placeholder={shell.detect_order.join(", ") || t("shellDetectOrderHint")}
            onChange={(event) => setOrderInput(event.target.value)}
          />
        </label>

        <button
          type="submit"
          className="button button--primary"
          disabled={busy || patchEmpty}
          title={patchEmpty ? t("shellPatchNone") : undefined}
        >
          {busy ? t("shellApplying") : t("shellApply")}
        </button>
      </form>
    </section>
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
