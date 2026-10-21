import { useEffect, useMemo, useState } from "react";
import { api } from "../api";
import type { AccountInfo, LightningAddress, Method, ReceiveTarget } from "../api/types";
import { Icon } from "../components/Icon";
import { AmountInput, CopyButton, QR, Sheet } from "../components/ui";
import { METHOD_LABEL, accountColor, errorText, formatSats } from "../format";
import { useWallet } from "../state/store";

/** Methods to offer for an account, best first. Liquid comes via swaps. */
function methodsFor(a: AccountInfo): Method[] {
  // Spark addresses only work between Spark wallets, so they aren't offered.
  const order: Method[] = ["bolt11", "bitcoinAddress"];
  const out = order.filter((m) => a.receives.includes(m));
  if (a.receives.includes("bolt11")) out.push("liquidAddress");
  return out;
}

const NEEDS_AMOUNT: Method[] = ["bolt11", "liquidAddress"];

/** Short labels so five options fit on a phone. */
const SHORT: Partial<Record<Method, string>> = {
  lightningAddress: "Address",
  bolt11: "Invoice",
  sparkAddress: "Spark",
  bitcoinAddress: "On-chain",
  liquidAddress: "Liquid",
};

const EXPLAIN: Partial<Record<Method, string>> = {
  bolt11: "Any Lightning wallet can pay this. It expires, so make a fresh one each time.",
  sparkAddress: "Free and instant from other Spark wallets. Reusable.",
  lightningAddress: "Your permanent address. Works with any Lightning wallet, any amount, any time.",
  bitcoinAddress: "Works with every bitcoin wallet. Takes about 10 to 60 minutes to confirm.",
  liquidAddress: "For someone paying from a Liquid wallet. It's swapped into your bitcoin automatically.",
};

export function ReceiveSheet({ account: initial }: { account?: string }) {
  const { accounts, activity, open } = useWallet();
  const [openedAt] = useState(() => Math.floor(Date.now() / 1000) - 5);
  const [lnAddress, setLnAddress] = useState<LightningAddress | null>(null);
  const [preferId, preferMethod] = (initial ?? "").split("#");
  const receivable = accounts.filter((a) => a.receives.length > 0);
  // Default to Spending: everything can arrive there, including on-chain.
  const [accountId, setAccountId] = useState(preferId || receivable.find((a) => a.rail === "spark")?.id || receivable[0]?.id || "");
  const account = accounts.find((a) => a.id === accountId);
  const isSpark = account?.rail === "spark";
  const methods = useMemo(() => {
    const m = account ? methodsFor(account) : [];
    return lnAddress && isSpark ? (["lightningAddress", ...m] as Method[]) : m;
  }, [account?.id, lnAddress, isSpark]); // eslint-disable-line react-hooks/exhaustive-deps
  const [method, setMethod] = useState<Method>(preferMethod === "liquid" ? "liquidAddress" : methods[0] ?? "bitcoinAddress");
  const [amount, setAmount] = useState<number | null>(null);
  const [target, setTarget] = useState<ReceiveTarget | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editingAmount, setEditingAmount] = useState(false);

  // A claimed Lightning address is the simplest thing to share, so lead with it.
  useEffect(() => {
    setLnAddress(null);
    if (!isSpark || !account) return;
    let alive = true;
    api()
      .then((a) => a.lightningAddress(account.id))
      .then((l) => {
        if (alive && l) {
          setLnAddress(l);
          if (preferMethod !== "liquid") setMethod("lightningAddress");
        }
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [account?.id, isSpark]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (account && !methods.includes(method)) setMethod(methods[0]);
  }, [account?.id, methods, method]); // eslint-disable-line react-hooks/exhaustive-deps

  // Watch for the payment landing while this sheet is open.
  const received = activity.find((p) => p.account === accountId && p.direction === "incoming" && p.timestamp >= openedAt);

  const needsAmount = NEEDS_AMOUNT.includes(method) && !amount;

  useEffect(() => {
    if (!account || needsAmount) {
      setTarget(null);
      return;
    }
    if (method === "lightningAddress") {
      setError(null);
      setTarget(lnAddress ? { method, request: lnAddress.address, amountSat: null } : null);
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
    // Keyed on the account id, not the object: refreshes must not replace an invoice on screen.
  }, [account?.id, method, amount, needsAmount, lnAddress]); // eslint-disable-line react-hooks/exhaustive-deps

  if (received) {
    return (
      <Sheet title="Received">
        <div style={{ textAlign: "center", padding: "32px 0 24px" }}>
          <div style={{ color: "var(--green)", marginBottom: 12, display: "inline-block" }}>
            <Icon name="check" size={56} stroke={2.5} />
          </div>
          <div className="big-stat num" style={{ fontSize: 44 }}>
            +{formatSats(received.amountSat)} <span className="balance-unit">sats</span>
          </div>
          <p className="balance-sub">in {account?.name}</p>
        </div>
        <button className="btn primary block" onClick={() => useWallet.getState().close()}>
          Done
        </button>
      </Sheet>
    );
  }

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
            <button key={m} aria-pressed={m === method} onClick={() => setMethod(m)} aria-label={METHOD_LABEL[m]}>
              {SHORT[m] ?? METHOD_LABEL[m]}
            </button>
          ))}
        </div>
      )}

      {error ? (
        <p className="notice">{error}</p>
      ) : target ? (
        <>
          <div className="qr-wrap">
            {/* LNURL in the QR scans in every Lightning wallet; the address shows as text. */}
            <QR value={method === "lightningAddress" && lnAddress ? `lightning:${lnAddress.lnurl.toUpperCase()}` : target.request} />
          </div>
          <p className="request-text">{target.request}</p>
          <div style={{ display: "flex", gap: 10, justifyContent: "center", flexWrap: "wrap", marginBottom: 18 }}>
            <CopyButton text={target.request} />
            {"share" in navigator && (
              <button className="btn small" onClick={() => navigator.share({ text: target.request }).catch(() => {})}>
                Share
              </button>
            )}
            {method !== "lightningAddress" && (
              <button className="btn small" onClick={() => setEditingAmount(true)}>
                {amount ? `${formatSats(amount)} sats` : "Add amount"}
              </button>
            )}
          </div>
          <p className="group-note" style={{ margin: "0 0 8px", textAlign: "center" }}>
            {method === "bitcoinAddress" && isSpark
              ? "Send from any bitcoin wallet. It lands in Spending after 3 confirmations, ready to spend over Lightning."
              : EXPLAIN[method]}
            {target.feeSat ? ` Swap fee about ${formatSats(target.feeSat)} sats.` : ""}
          </p>
          {isSpark && !lnAddress && (
            <p style={{ textAlign: "center", margin: "12px 0 0" }}>
              <button className="text-button" onClick={() => open({ name: "account", id: accountId })}>
                Get a Lightning address
              </button>
            </p>
          )}
        </>
      ) : (
        <p className="empty">Getting a fresh request…</p>
      )}
    </Sheet>
  );
}
