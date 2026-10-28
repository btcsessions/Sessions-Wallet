import { useEffect, useState } from "react";
import { api } from "../api";
import { isBiometricError, type BiometricStatus } from "../api/types";
import { errorText } from "../format";
import { useWallet } from "../state/store";

const LABEL: Record<BiometricStatus["kind"], string> = { fingerprint: "fingerprint", face: "face", biometric: "fingerprint or face" };

/** Settings: turn fingerprint / face unlock on or off. */
export function BiometricSetting() {
  const notify = useWallet((s) => s.notify);
  const [status, setStatus] = useState<BiometricStatus | null>(null);
  const [asking, setAsking] = useState(false);
  const [passcode, setPasscode] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api()
      .then((a) => a.biometricStatus())
      .then(setStatus)
      .catch(() => {});
  }, []);

  if (!status) return null;
  const what = LABEL[status.kind];

  const turnOn = async () => {
    setBusy(true);
    try {
      setStatus(await (await api()).enableBiometric(passcode));
      setAsking(false);
      setPasscode("");
      notify("ok", `Unlock with ${what} is on`);
    } catch (e) {
      if (!isBiometricError(e, "cancelled")) notify("error", errorText(e));
      if (isBiometricError(e, "wrong_passcode")) setPasscode("");
    } finally {
      setBusy(false);
    }
  };

  const turnOff = async () => {
    setBusy(true);
    try {
      setStatus(await (await api()).disableBiometric());
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <p className="group-label">Security</p>
      <div className="group">
        <label className="cell">
          <span className="grow">Unlock with {what}</span>
          <input
            type="checkbox"
            role="switch"
            checked={status.enabled || asking}
            disabled={!status.available || busy}
            onChange={(e) => (e.target.checked ? setAsking(true) : asking ? setAsking(false) : void turnOff())}
            style={{ flex: "none", width: 22, height: 22, accentColor: "var(--accent)" }}
          />
        </label>
        {asking && (
          <label className="cell">
            <span className="grow">Passcode</span>
            <input
              type="password"
              inputMode="numeric"
              autoComplete="off" data-1p-ignore data-lpignore="true"
              autoFocus
              value={passcode}
              onChange={(e) => setPasscode(e.target.value)}
              style={{ textAlign: "right" }}
            />
          </label>
        )}
      </div>
      {asking ? (
        <button className="btn primary block" style={{ marginBottom: 22 }} disabled={passcode.length < 6 || busy} onClick={() => void turnOn()}>
          {busy ? `Confirm with your ${what}…` : "Turn on"}
        </button>
      ) : (
        <p className="group-note">
          {status.available ? `Skip typing your passcode. It still works, and it's needed if your ${what} changes.` : (status.reason ?? "This device doesn't have a fingerprint or face sensor that apps can use.")}
        </p>
      )}
    </>
  );
}
