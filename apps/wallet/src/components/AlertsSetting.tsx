import { useEffect, useState } from "react";
import { api } from "../api";
import type { AlertsStatus } from "../api/types";
import { useWallet } from "../state/store";

const SWITCH = { flex: "none", width: 22, height: 22, accentColor: "var(--accent)" } as const;

/** Settings: payment notifications, and watching with the app closed. */
export function AlertsSetting() {
  const alerts = useWallet((s) => s.synced.alerts);
  const setSynced = useWallet((s) => s.setSynced);
  const [status, setStatus] = useState<AlertsStatus | null>(null);
  const [busy, setBusy] = useState(false);

  const reload = () =>
    api()
      .then((a) => a.alertsStatus())
      .then(setStatus)
      .catch(() => {});

  useEffect(() => {
    void reload();
    // Back from the system settings page: see what changed.
    const onVis = () => !document.hidden && void reload();
    document.addEventListener("visibilitychange", onVis);
    return () => document.removeEventListener("visibilitychange", onVis);
  }, []);

  if (!status?.supported) return null;
  const allowed = status.notifications === "granted";

  const ask = async () => {
    setBusy(true);
    try {
      setStatus(await (await api()).alertsRequestNotifications());
    } finally {
      setBusy(false);
    }
  };

  const setNotify = async (on: boolean) => {
    setSynced("alerts", { ...alerts, notify: on });
    if (on && !allowed) await ask();
  };

  const setBackground = async (on: boolean) => {
    setSynced("alerts", { ...alerts, notify: on || alerts.notify, background: on });
    if (on && !allowed) await ask();
    // The service starts once the wallet has saved the setting.
    setTimeout(() => void reload(), 2500);
  };

  const unrestrict = async () => {
    setBusy(true);
    try {
      setStatus(await (await api()).alertsRequestUnrestricted());
    } finally {
      setBusy(false);
    }
  };

  const on = alerts.notify && allowed;
  return (
    <>
      <p className="group-label">Payment alerts</p>
      <div className="group">
        <label className="cell">
          <span className="grow">Notify me when payments arrive</span>
          <input type="checkbox" role="switch" checked={on} disabled={busy} onChange={(e) => void setNotify(e.target.checked)} style={SWITCH} />
        </label>
        <label className="cell">
          <span className="grow">Keep watching when the app is closed</span>
          <input type="checkbox" role="switch" checked={alerts.background} disabled={busy} onChange={(e) => void setBackground(e.target.checked)} style={SWITCH} />
        </label>
        {alerts.background && !status.unrestricted && (
          <button className="cell" onClick={() => void unrestrict()} disabled={busy}>
            <span className="grow" style={{ textAlign: "left" }}>
              Allow alerts with the screen off
            </span>
            <span className="value" style={{ color: "var(--accent-ink)" }}>
              Allow
            </span>
          </button>
        )}
      </div>
      <p className="group-note">
        {alerts.notify && status.notifications === "denied"
          ? "Notifications are switched off for Sessions in your phone's settings. Turn the first switch on to open them."
          : alerts.background
            ? status.unrestricted
              ? "The wallet stays connected after you close it and shows a small “Watching for payments” notification. Restarting your phone stops it until you open the app."
              : "The wallet stays connected after you close it. Android may pause it while the screen is off unless you allow it to run unrestricted."
            : "You'll get alerts while the app is open or was used recently. Turn on watching to get them after you close it."}
      </p>
    </>
  );
}
