import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import type { BackupStatus } from "../api/types";
import { errorText } from "../format";
import { useWallet } from "../state/store";
import { Icon } from "./Icon";

function ago(ts: number): string {
  const s = Math.max(0, Math.floor(Date.now() / 1000) - ts);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return new Date(ts * 1000).toLocaleDateString();
}

/** Backup status, "back up now", and (Advanced) adding a passkey backup. */
export function BackupSection({ onStatus }: { onStatus?: (s: BackupStatus) => void }) {
  const advanced = useWallet((s) => s.layout?.advanced ?? false);
  const notify = useWallet((s) => s.notify);
  const [status, setStatus] = useState<BackupStatus | null>(null);
  const [passkeys, setPasskeys] = useState(false);
  const [busy, setBusy] = useState<"now" | "passkey" | null>(null);

  const update = useCallback(
    (s: BackupStatus) => {
      setStatus(s);
      onStatus?.(s);
    },
    [onStatus],
  );

  useEffect(() => {
    let off: (() => void) | undefined;
    let alive = true;
    void api().then(async (a) => {
      a.backupStatus().then((s) => alive && update(s)).catch(() => {});
      a.passkeySupported().then((v) => alive && setPasskeys(v)).catch(() => {});
      const f = await a.onBackupStatus((s) => alive && update(s));
      if (alive) off = f;
      else f();
    });
    return () => {
      alive = false;
      off?.();
    };
  }, [update]);

  const act = async (kind: "now" | "passkey") => {
    setBusy(kind);
    try {
      const a = await api();
      update(kind === "now" ? await a.backupNow() : await a.enablePasskeyBackup());
      notify("ok", kind === "now" ? "Backed up" : "Passkey backup added");
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setBusy(null);
    }
  };

  if (!status) return null;
  const methods = [status.passkey && "passkey", status.phrase && "recovery phrase"].filter(Boolean).join(" and ");
  return (
    <>
      <p className="group-label">Backup</p>
      <div className="group">
        <div className="cell">
          <span className="grow">Backed up with</span>
          <span className="value">{methods || "Nothing yet"}</span>
        </div>
        <div className="cell">
          <span className="grow">Accounts, labels and layout</span>
          <span className="value" style={{ color: status.lastError ? "var(--red, #ff453a)" : undefined }}>
            {status.pending ? "Saving…" : status.lastError ? "Not saved" : status.lastBackupAt ? `Saved ${ago(status.lastBackupAt)}` : "Not saved yet"}
          </span>
        </div>
      </div>
      <p className="group-note">
        {status.lastError
          ? `Last attempt failed: ${status.lastError}. It retries on its own.`
          : `Encrypted so only your ${methods || "phrase or passkey"} can open it, and stored on ${status.relays.length} independent servers. Saved automatically when anything changes.`}
      </p>
      {!status.phrase && status.seedOrigin === "phrase" && (
        <p className="notice">
          <Icon name="warn" size={16} /> You haven't confirmed writing down your recovery phrase.
          {passkeys && !status.passkey ? " Add a passkey backup, or show the phrase below and write it down." : " Show it below and write it down."}
        </p>
      )}
      <div style={{ display: "grid", gap: 10, marginBottom: 22 }}>
        <button className="btn block" disabled={!!busy} onClick={() => void act("now")}>
          {busy === "now" ? "Backing up…" : "Back up now"}
        </button>
        {advanced && passkeys && !status.passkey && (
          <button className="btn block" disabled={!!busy} onClick={() => void act("passkey")}>
            {busy === "passkey" ? "Create your passkey when asked…" : "Add a passkey backup"}
          </button>
        )}
      </div>
    </>
  );
}
