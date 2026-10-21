import { useState } from "react";
import { api } from "../api";
import type { Execution, FeePriority } from "../api/types";
import { CopyButton, QR, Sheet, useBusy } from "../components/ui";
import { useWallet } from "../state/store";

/** Hardware wallet / cosigner round-trip for a paused payment or move. */
export function SignStep({ paused, fee, onDone }: { paused: Execution; fee: FeePriority; onDone: () => void }) {
  const refresh = useWallet((s) => s.refresh);
  const [signed, setSigned] = useState("");
  const [busy, run] = useBusy();
  const ns = paused.awaitingSignature?.needsSignature;
  if (!ns) return null;

  const broadcast = () =>
    run(async () => {
      await (await api()).resume(paused, signed.trim(), fee);
      await refresh();
      onDone();
    });

  return (
    <Sheet title="Sign on your device">
      <p className="notice">
        Scan this with your hardware wallet
        {ns.signaturesNeeded > 1 ? ` and ${ns.signaturesNeeded - 1} more cosigner${ns.signaturesNeeded > 2 ? "s" : ""}` : ""}. Check the amount and
        address on its screen, then bring the signed transaction back here.
      </p>
      <div className="qr-wrap">
        <QR value={ns.psbtBase64} />
      </div>
      <div style={{ display: "flex", justifyContent: "center", marginBottom: 20 }}>
        <CopyButton text={ns.psbtBase64} label="Copy PSBT" />
      </div>
      <p className="group-label">Signed transaction</p>
      <div className="group">
        <label className="cell">
          <span className="visually-hidden">Signed PSBT</span>
          <textarea value={signed} onChange={(e) => setSigned(e.target.value)} placeholder="Paste the signed PSBT" spellCheck={false} />
        </label>
      </div>
      <button className="btn primary block" disabled={!signed.trim() || busy} onClick={broadcast}>
        {busy ? "Broadcasting…" : "Broadcast"}
      </button>
    </Sheet>
  );
}
