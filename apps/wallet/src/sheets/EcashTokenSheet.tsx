import { useState } from "react";
import { api } from "../api";
import { AmountInput, CopyButton, QR, Sheet, useBusy } from "../components/ui";
import { useWallet } from "../state/store";

/** Turn ecash into a token you can text, email or print. */
export function EcashTokenSheet({ account }: { account: string }) {
  const { accounts, refresh } = useWallet();
  const a = accounts.find((x) => x.id === account);
  const [amount, setAmount] = useState<number | null>(null);
  const [memo, setMemo] = useState("");
  const [token, setToken] = useState<string | null>(null);
  const [busy, run] = useBusy();

  const create = () =>
    run(async () => {
      if (!amount) return;
      const p = await (await api()).createToken(account, amount, memo.trim() || null);
      await refresh();
      setToken(p.token ?? null);
    });

  if (token) {
    return (
      <Sheet title="Ecash ready">
        <div className="qr-wrap">
          <QR value={token} />
        </div>
        <p className="notice">Whoever has this token can claim the sats. Send it only to the person you mean to pay.</p>
        <div style={{ display: "flex", gap: 10, justifyContent: "center" }}>
          <CopyButton text={token} label="Copy token" />
          {"share" in navigator && (
            <button className="btn small" onClick={() => navigator.share({ text: token }).catch(() => {})}>
              Share
            </button>
          )}
        </div>
      </Sheet>
    );
  }

  return (
    <Sheet title="Send as ecash">
      <AmountInput value={amount} onChange={setAmount} autoFocus max={a?.balance.confirmed} />
      <div className="group">
        <label className="cell">
          <span className="grow">Note</span>
          <input value={memo} onChange={(e) => setMemo(e.target.value)} placeholder="Optional" style={{ textAlign: "right" }} maxLength={80} />
        </label>
      </div>
      <button className="btn primary block" disabled={!amount || busy} onClick={create}>
        {busy ? "Creating…" : "Create token"}
      </button>
    </Sheet>
  );
}
