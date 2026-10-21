import { useState } from "react";
import { api } from "../api";
import { UrQR } from "../components/AnimatedQR";
import { Icon } from "../components/Icon";
import { ScanButton } from "../components/Scanner";
import { errorText } from "../format";

/**
 * Jade QR PIN unlock. Jade shows its encrypted unlock request as a QR, we
 * forward it to Jade's PIN server (the blind oracle, which never sees the PIN
 * or keys) and show the answer for Jade to scan.
 */
export function JadeQrUnlock({ onDone }: { onDone?: () => void }) {
  const [reply, setReply] = useState<Uint8Array | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (reply) {
    return (
      <div>
        <p className="notice">On Jade, choose Scan Web QR and scan this. Jade unlocks.</p>
        <div className="qr-wrap">
          <UrQR type="jade-pin" cbor={reply} />
        </div>
        {onDone && (
          <button className="btn primary block" onClick={onDone}>
            Jade is unlocked
          </button>
        )}
      </div>
    );
  }

  return (
    <div>
      <p className="notice">On Jade, choose QR Mode › QR PIN Unlock, enter your PIN, and scan the code Jade shows.</p>
      <ScanButton
        className="btn primary block"
        label={busy ? "Unlocking…" : "Scan Jade's unlock code"}
        title="Scan Jade's code"
        hint="The code Jade shows after you enter your PIN."
        onResult={(r) => {
          if (r.kind !== "ur" || r.type !== "jade-pin") {
            setError("That isn't Jade's unlock code. Enter your PIN on Jade first, then scan the code it shows.");
            return;
          }
          setBusy(true);
          setError(null);
          api()
            .then((a) => a.jadeQrPin(new Uint8Array(r.cbor)))
            .then(setReply)
            .catch((e) => setError(errorText(e)))
            .finally(() => setBusy(false));
        }}
      />
      <p className="group-note" style={{ marginTop: 14 }}>This sends Jade's encrypted unlock request to Blockstream's PIN server. The server never sees your PIN or keys.</p>
      {error && (
        <p className="field-error" role="alert" style={{ marginTop: 14 }}>
          <Icon name="warn" size={14} /> {error}
        </p>
      )}
    </div>
  );
}
