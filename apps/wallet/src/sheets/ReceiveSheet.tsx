import { useEffect, useMemo, useState } from "react";
import { api } from "../api";
import type { AccountInfo, Method, ReceiveTarget } from "../api/types";
import { AmountInput, CopyButton, QR, Sheet } from "../components/ui";
import { METHOD_LABEL, accountColor, errorText, formatSats } from "../format";
import { useWallet } from "../state/store";

/** Methods to offer for an account, best first. Liquid comes via swaps. */
function methodsFor(a: AccountInfo): Method[] {
  const order: Method[] = ["bolt11", "sparkAddress", "bitcoinAddress"];
  const out = order.filter((m) => a.receives.includes(m));
  if (a.receives.includes("bolt11")) out.push("liquidAddress");
  return out;
}

const NEEDS_AMOUNT: Method[] = ["bolt11", "liquidAddress"];

const EXPLAIN: Partial<Record<Method, string>> = {
  bolt11: "Any Lightning wallet can pay this. It expires, so make a fresh one each time.",
  sparkAddress: "Free and instant from other Spark wallets. Reusable.",
  bitcoinAddress: "Works with every bitcoin wallet. Takes about 10 to 60 minutes to confirm.",
  liquidAddress: "For someone paying from a Liquid wallet. It's swapped into your bitcoin automatically.",
};

export function ReceiveSheet({ account: initial }: { account?: string }) {
  const accounts = useWallet((s) => s.accounts);
  const [preferId, preferMethod] = (initial ?? "").split("#");
  const receivable = accounts.filter((a) => a.receives.length > 0);
  const [accountId, setAccountId] = useState(preferId || receivable[0]?.id || "");
  const account = accounts.find((a) => a.id === accountId);
  const methods = useMemo(() => (account ? methodsFor(account) : []), [account]);
  const [method, setMethod] = useState<Method>(preferMethod === "liquid" ? "liquidAddress" : methods[0] ?? "bitcoinAddress");
  const [amount, setAmount] = useState<number | null>(null);
  const [target, setTarget] = useState<ReceiveTarget | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editingAmount, setEditingAmount] = useState(false);

  useEffect(() => {
    if (account && !methods.includes(method)) setMethod(methods[0]);
  }, [account, methods, method]);

  const needsAmount = NEEDS_AMOUNT.includes(method) && !amount;

  useEffect(() => {
    if (!account || needsAmount) {
      setTarget(null);
      return;
    }
    let alive = true;
    setError(null);
    setTarget(null);
    api()
      .then((a) => a.receive(account.id, method, amount, null))
      .then((t) => alive && setTarget(t))
      .catch((e) => alive && setError(errorText(e)));
    return () => {
      alive = false;
    };
  }, [account, method, amount, needsAmount]);

  if (editingAmount || (needsAmount && account)) {
    return (
      <Sheet
        title="Request amount"
        left={
          editingAmount ? (
            <button className="text-button" onClick={() => setEditingAmount(false)}>
              Back
            </button>
          ) : undefined
        }
      >
        <AmountInput value={amount} onChange={setAmount} autoFocus />
        {needsAmount && <p className="group-note" style={{ margin: "0 0 18px", textAlign: "center" }}>{METHOD_LABEL[method]} requests need an amount.</p>}
        <button className="btn primary block" disabled={!amount} onClick={() => setEditingAmount(false)}>
          Show request
        </button>
        {needsAmount && methods.length > 1 && (
          <button className="btn block" style={{ marginTop: 10 }} onClick={() => setMethod(methods.find((m) => !NEEDS_AMOUNT.includes(m)) ?? methods[0])}>
            Use an address instead
          </button>
        )}
      </Sheet>
    );
  }

  return (
    <Sheet title="Receive">
      {receivable.length > 1 && (
        <div className="chips">
          {receivable.map((a) => (
            <button key={a.id} className="chip" aria-pressed={a.id === accountId} onClick={() => setAccountId(a.id)}>
              <span style={{ display: "inline-block", width: 8, height: 8, borderRadius: 2, background: accountColor(a), marginRight: 6 }} />
              {a.name}
            </button>
          ))}
        </div>
      )}

      {methods.length > 1 && (
        <div className="segmented">
          {methods.map((m) => (
            <button key={m} aria-pressed={m === method} onClick={() => setMethod(m)}>
              {METHOD_LABEL[m]}
            </button>
          ))}
        </div>
      )}

      {error ? (
        <p className="notice">{error}</p>
      ) : target ? (
        <>
          <div className="qr-wrap">
            <QR value={target.request} />
          </div>
          <p className="request-text">{target.request}</p>
          <div style={{ display: "flex", gap: 10, justifyContent: "center", flexWrap: "wrap", marginBottom: 18 }}>
            <CopyButton text={target.request} />
            {"share" in navigator && (
              <button className="btn small" onClick={() => navigator.share({ text: target.request }).catch(() => {})}>
                Share
              </button>
            )}
            <button className="btn small" onClick={() => setEditingAmount(true)}>
              {amount ? `${formatSats(amount)} sats` : "Add amount"}
            </button>
          </div>
          <p className="group-note" style={{ margin: "0 0 8px", textAlign: "center" }}>
            {EXPLAIN[method]}
            {target.feeSat ? ` Swap fee about ${formatSats(target.feeSat)} sats.` : ""}
          </p>
        </>
      ) : (
        <p className="empty">Getting a fresh request…</p>
      )}
    </Sheet>
  );
}
