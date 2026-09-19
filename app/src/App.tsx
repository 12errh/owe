import { useCallback, useEffect, useState } from "react";

import { daemonStatus, type DaemonStatus } from "./ipc";
import { t, tList } from "./i18n";

// P0 GUI: the simple-UI contract's status bar (UI-DESIGN §3) and nothing more.
// Deliberately no timers and no polling: the window does work when the user asks
// it to, which is the whole point of a resource-first tool. Live updates arrive
// in P3 with the daemon's event bus.

export default function App() {
  const [status, setStatus] = useState<DaemonStatus | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    setBusy(true);
    try {
      setStatus(await daemonStatus());
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

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

      {status?.connected && (
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
      )}

      <section className="placeholder">
        <h2>{t("placeholderTitle")}</h2>
        <p>{t("placeholderBody")}</p>
        <p className="placeholder__note">{t("disposableNote")}</p>
      </section>
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
