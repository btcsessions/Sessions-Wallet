import { useEffect, useState } from "react";
import { api } from "../api";
import type { SyncDirection, SyncStatus } from "../api/types";
import { Icon } from "../components/Icon";
import { ScanButton } from "../components/Scanner";
import { Sheet, useBusy, useSecureScreen } from "../components/ui";
import { errorText, relativeTime } from "../format";
import { useWallet } from "../state/store";

/** Settings › Linked devices: the phones and computers on this wallet, linking another, and resync. */
export function DevicesSheet() {
  const [status, setStatus] = useState<SyncStatus | null>(null);
  const [linking, setLinking] = useState(false);
  const [busy, run] = useBusy();
  const [error, setError] = useState<string | null>(null);
  const { notify, refresh, loadPrefs } = useWallet();

  useEffect(() => {
    if (linking) return;
    void (async () => {
      try {
        setStatus(await (await api()).syncStatus());
      } catch (e) {
        setError(errorText(e));
      }
    })();
  }, [linking]);

  const sync = (direction: SyncDirection) =>
    run(async () => {
      const ask =
        direction === "sendMine"
          ? "Use this device's setup on all your linked devices? Their accounts, labels and settings are replaced with this one's. Home screens stay as they are on each device."
          : direction === "takeTheirs"
            ? "Replace this device's setup with the latest from your other device? Accounts, labels and settings here are replaced. Your home screen stays as it is."
            : null;
      if (ask && !window.confirm(ask)) return;
      setError(null);
      try {
        setStatus(await (await api()).syncNow(direction));
        await refresh();
        await loadPrefs();
        notify("ok", direction === "sendMine" ? "Sent. Your other devices pick it up within a few minutes." : direction === "takeTheirs" ? "Done. This device now matches the other." : "Up to date");
      } catch (e) {
        setError(errorText(e));
      }
    });

  if (linking) return <LinkDevice onDone={() => setLinking(false)} />;

  const others = status?.devices.filter((d) => !d.thisDevice) ?? [];
  return (
    <Sheet title="Linked devices">
      <p className="notice">Use this wallet on your phone and computer. Accounts, labels and settings stay in step; each device keeps its own home screen.</p>
      <div className="group">
        {(status?.devices ?? []).map((d) => (
          <div className="cell" key={d.id}>
            <Icon name={d.kind === "desktop" ? "desktop" : "phone"} size={18} />
            <span className="grow">
              {d.name}
              {d.thisDevice ? " (this device)" : ""}
            </span>
            {!d.thisDevice && d.seenAt && <small className="muted">{relativeTime(d.seenAt)}</small>}
          </div>
        ))}
      </div>
      <button className="btn primary block" style={{ margin: "6px 0 22px" }} onClick={() => setLinking(true)}>
        Link a device
      </button>

      {others.length > 0 && (
        <>
          <p className="group-label">Sync</p>
          <div className="group">
            <div className="cell">
              <span className="grow">Last synced</span>
              <span className="value">{status?.lastSyncAt ? relativeTime(status.lastSyncAt) : "Not yet"}</span>
            </div>
            <button className="cell" disabled={busy} onClick={() => sync("both")}>
              <span className="grow">Sync now</span>
              <Icon name="chevron" size={16} />
            </button>
          </div>
          <p className="group-note">Changes go across by themselves within a couple of minutes. If both devices change the same thing, the newest change wins.</p>

          <p className="group-label">Resync</p>
          <div className="group">
            <button className="cell" disabled={busy} onClick={() => sync("sendMine")}>
              <span className="grow">
                Use this device's setup everywhere
                <small className="muted" style={{ display: "block" }}>
                  This device → your other devices
                </small>
              </span>
              <Icon name="chevron" size={16} />
            </button>
            <button className="cell" disabled={busy} onClick={() => sync("takeTheirs")}>
              <span className="grow">
                Use the other device's setup here
                <small className="muted" style={{ display: "block" }}>
                  Your other device → this device
                </small>
              </span>
              <Icon name="chevron" size={16} />
            </button>
          </div>
          <p className="group-note">For when the two have drifted apart and you want one to match the other exactly. Funds aren't touched.</p>
        </>
      )}
      {error && (
        <p className="field-error" role="alert">
          <Icon name="warn" size={14} /> {error}
        </p>
      )}
    </Sheet>
  );
}

/** The wallet device's side: scan the new device, compare codes, send. */
function LinkDevice({ onDone }: { onDone: () => void }) {
  const [code, setCode] = useState<string | null>(null);
  const [typed, setTyped] = useState("");
  const [passcode, setPasscode] = useState("");
  const [sent, setSent] = useState(false);
  useSecureScreen(!!code && !sent);
  const [error, setError] = useState<string | null>(null);
  const [busy, run] = useBusy();

  useEffect(
    () => () => {
      void api().then((a) => a.linkCancel());
    },
    [],
  );

  const scanned = (text: string) =>
    run(async () => {
      setError(null);
      try {
        setCode((await (await api()).linkScan(text.trim())).code);
      } catch (e) {
        setError(errorText(e));
      }
    });

  const send = () =>
    run(async () => {
      setError(null);
      try {
        await (await api()).linkSend(passcode);
        setPasscode("");
        setSent(true);
      } catch (e) {
        setError(errorText(e));
      }
    });

  const back = (
    <button className="text-button" onClick={onDone}>
      {sent ? "Done" : "Cancel"}
    </button>
  );
  const errorLine = error && (
    <p className="field-error" role="alert" style={{ marginTop: 10 }}>
      <Icon name="warn" size={14} /> {error}
    </p>
  );

  if (sent) {
    return (
      <Sheet title="Wallet sent" left={back}>
        <div className="hw-hero">
          <Icon name="check" size={48} />
        </div>
        <p className="notice">Finish on the other device: choose a passcode there. Your accounts and settings follow within a minute.</p>
        <button className="btn primary block" onClick={onDone}>
          Done
        </button>
      </Sheet>
    );
  }

  if (code) {
    return (
      <Sheet title="Link a device" left={back}>
        <p className="notice">Check the other device shows this same code.</p>
        <div className="join-code mono" aria-label="Check code" style={{ marginBottom: 12 }}>
          {code}
        </div>
        <p className="group-note">If it's different, cancel: something else scanned or changed the code.</p>
        <p className="group-label">Your passcode</p>
        <div className="group">
          <label className="cell">
            <span className="visually-hidden">Passcode</span>
            <input type="password" inputMode="numeric" autoComplete="off" data-1p-ignore data-lpignore="true" value={passcode} onChange={(e) => setPasscode(e.target.value)} placeholder="To send your wallet" />
          </label>
        </div>
        <button className="btn primary block" style={{ marginBottom: 14 }} disabled={busy || passcode.length < 6} onClick={send}>
          {busy ? "Sending…" : "Codes match · send wallet"}
        </button>
        <p className="group-note">This sends your recovery phrase to that device, encrypted so only it can read it. Only link devices you own.</p>
        {errorLine}
      </Sheet>
    );
  }

  return (
    <Sheet title="Link a device" left={back}>
      <p className="notice">On the new phone or computer, install Sessions Wallet and choose "Link to my wallet on another device". Then scan the code it shows.</p>
      <ScanButton className="btn primary block" label="Scan its code" title="Scan link code" hint="Scan the QR on the new device's screen." onResult={(r) => (r.kind === "text" ? scanned(r.text) : setError("That isn't a link code"))} />
      <p className="group-label">Or paste its code</p>
      <div className="group">
        <label className="cell">
          <span className="visually-hidden">Link code</span>
          <input value={typed} onChange={(e) => setTyped(e.target.value)} placeholder="sessionswallet://link/…" spellCheck={false} autoCapitalize="off" autoCorrect="off" className="mono" />
        </label>
      </div>
      <button className="btn block" disabled={busy || !typed.trim()} onClick={() => scanned(typed)}>
        {busy ? "Connecting…" : "Continue"}
      </button>
      {errorLine}
    </Sheet>
  );
}
