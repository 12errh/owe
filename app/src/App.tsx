import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  assignWallpaper,
  asFailure,
  clearWallpaper,
  daemonStatus,
  libraryIndex,
  libraryScan,
  libraryThumbnail,
  listOutputs,
  type ApplyOutcome,
  type DaemonStatus,
  type IpcFailure,
  type LibraryItem,
  type LibraryPage,
  type OutputsView,
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
        await loadLibrary({ page: 1, scan: true });
      } else {
        setOutputs(null);
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
