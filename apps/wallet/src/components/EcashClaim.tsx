import { useState } from "react";
import { api } from "../api";
import { errorText, formatSats } from "../format";
import { useWallet } from "../state/store";

/**
 * Ecash account: collect a payment the mint has but this device lost track
 * of (made on another device, or before a reinstall), by its quote ID.
 */
export function EcashClaim({ account }: { account: string }) {
  const notify = useWallet((s) => s.notify);
  const [open, setOpen] = useState(false);
  const [id, setId] = useState("");
  const [busy, setBusy] = useState(false);

  const claim = async () => {
    setBusy(true);
    try {
      const sats = await (await api()).claimEcashQuote(account, id.trim());
      notify("ok", `Collected ${formatSats(sats)} sats`);
      setId("");
      setOpen(false);
      void useWallet.getState().refresh();
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setBusy(false);
    }
  };

  if (!open) {
    return (
      <p style={{ textAlign: "center", margin: "4px 0 10px" }}>
        <button className="text-button subtle" onClick={() => setOpen(true)}>
          Collect a payment by quote ID
        </button>
      </p>
    );
  }
  return (
    <>
      <p className="group-label">Collect a payment</p>
      <div className="group">
        <label className="cell">
          <span className="visually-hidden">Quote ID</span>
          <input
            style={{ flex: 1 }}
            className="mono"
            value={id}
            onChange={(e) => setId(e.target.value)}
            placeholder="Quote ID from the mint"
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
          />
        </label>
      </div>
      <p className="group-note" style={{ marginTop: -8 }}>
        For a payment the mint received but this device never collected. It must have been made with this wallet's recovery phrase.
      </p>
      <button className="btn primary block" disabled={busy || !id.trim()} onClick={() => void claim()} style={{ marginBottom: 10 }}>
        {busy ? "Collecting…" : "Collect"}
      </button>
    </>
  );
}
