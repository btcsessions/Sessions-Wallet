import { useEffect, useState } from "react";
import { api } from "../api";
import type { BumpAction, Execution, SendQuote, TxDetails } from "../api/types";
import { Icon } from "../components/Icon";
import { CopyButton, HoldButton, Sheet, useBusy } from "../components/ui";
import { METHOD_LABEL, errorText, formatFiat, formatSats, signed } from "../format";
import { useWallet } from "../state/store";
import { SignStep } from "./SignStep";

const TXID = /^[0-9a-f]{64}$/i;

async function openExternal(url: string) {
  if ("__TAURI_INTERNALS__" in window) {
    const { openUrl } = await import("@tauri-apps/plugin-opener");
    await openUrl(url);
  } else {
    window.open(url, "_blank", "noopener");
  }
}

/** One transaction: what happened, the on-chain facts, and speed up / cancel while it's pending. */
export function TxSheet({ id, back }: { id: string; back?: string }) {
  const { activity, accounts, price, currency, open, refresh, notify, close } = useWallet();
  const advanced = useWallet((s) => s.layout?.advanced ?? false);
  const p = activity.find((x) => x.id === id);
  const account = accounts.find((a) => a.id === p?.account);
  const [details, setDetails] = useState<TxDetails | null>(null);
  const [action, setAction] = useState<BumpAction | null>(null);
  const [quote, setQuote] = useState<SendQuote | null>(null);
  const [paused, setPaused] = useState<Execution | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, run] = useBusy();

  useEffect(() => {
    if (!p || account?.rail !== "onchain") return;
    let alive = true;
    api()
      .then((a) => a.txDetails(p.account, p.id))
      .then((d) => alive && setDetails(d))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [p?.id, p?.status, account?.rail]); // eslint-disable-line react-hooks/exhaustive-deps

  const backButton = back ? (
    <button className="text-button" onClick={() => open({ name: "account", id: back })}>
      Back
    </button>
  ) : undefined;

  if (!p) {
    return (
      <Sheet title="Transaction" left={backButton}>
        <p className="empty">This transaction isn't in your activity any more.</p>
      </Sheet>
    );
  }

  const done = (text: string) => {
    notify("ok", text);
    void refresh();
    close();
  };

  if (paused) {
    return <SignStep paused={paused} fee="fast" onDone={() => done(action === "cancel" ? "Payment cancelled" : "Sped up")} />;
  }

  const start = (a: BumpAction) =>
    run(async () => {
      setError(null);
      setAction(a);
      setQuote(null);
      try {
        setQuote(await (await api()).prepareBump(p.account, details?.txid ?? p.id, a, "fast"));
      } catch (e) {
        setError(errorText(e));
        setAction(null);
      }
    });

  const confirm = () =>
    run(async () => {
      if (!quote) return;
      try {
        if (quote.needsSignature) {
          setPaused({ payments: [], awaitingSignature: quote, remaining: [], amountSat: 0 });
          return;
        }
        await (await api()).confirmQuote(quote);
        done(action === "cancel" ? "Payment cancelled" : "Sped up");
      } catch (e) {
        setError(errorText(e));
      }
    });

  const txid = details?.txid ?? (TXID.test(p.id) ? p.id : null);
  const amount = p.direction === "outgoing" ? p.amountSat + p.feeSat : p.amountSat;
  const title =
    p.description ||
    (p.direction === "incoming" ? `Received via ${METHOD_LABEL[p.method]}` : p.direction === "internal" ? "Transfer between accounts" : `Sent via ${METHOD_LABEL[p.method]}`);
  const pending = p.status === "pending";
  const status = p.status === "failed" ? "Failed" : pending ? "Waiting for confirmation" : details && details.confirmations > 0 ? `Confirmed · ${details.confirmations.toLocaleString()} confirmation${details.confirmations === 1 ? "" : "s"}` : "Complete";

  // Confirming a speed-up or cancel.
  if (action && quote) {
    return (
      <Sheet
        title={action === "cancel" ? "Cancel payment" : "Speed up"}
        left={
          <button className="text-button" onClick={() => (setAction(null), setQuote(null))}>
            Back
          </button>
        }
      >
        <div style={{ textAlign: "center", padding: "12px 0 20px" }}>
          <div className="big-stat num" style={{ fontSize: 40 }}>
            {formatSats(quote.feeSat)} <span className="balance-unit">sats</span>
          </div>
          <p className="balance-sub">fee, about {formatFiat(quote.feeSat, price, currency)}</p>
        </div>
        <p className="notice">
          {action === "cancel"
            ? `The ${formatSats(p.amountSat)} sats stay in ${account?.name ?? "this account"}. The payment won't go through.`
            : "This payment gets a higher fee so it confirms sooner, usually in the next block or two."}
        </p>
        {error && <p className="field-error">{error}</p>}
        <HoldButton disabled={busy} onConfirm={confirm}>
          {busy ? "Working…" : action === "cancel" ? "Hold to cancel payment" : "Hold to speed up"}
        </HoldButton>
      </Sheet>
    );
  }

  return (
    <Sheet title="Transaction" left={backButton}>
      <div style={{ textAlign: "center", padding: "8px 0 18px" }}>
        <div className={`big-stat num ${p.direction === "incoming" ? "in" : ""}`} style={{ fontSize: 44 }}>
          {signed(amount, p.direction)} <span className="balance-unit">sats</span>
        </div>
        <p className="balance-sub" style={{ margin: "4px 0 10px" }}>
          {formatFiat(amount, price, currency)}
        </p>
        <span className={`status-chip ${p.status}`}>
          {pending && <Icon name="clock" size={13} stroke={2.2} />}
          {status}
        </span>
      </div>

      <div className="group">
        <Row label="What" value={title} />
        <Row label="Account" value={account?.name ?? p.account} />
        <Row label="When" value={new Date(p.timestamp * 1000).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" })} />
        <Row label="Network" value={METHOD_LABEL[p.method]} />
        {p.direction !== "incoming" && <Row label="Fee" value={`${formatSats(details?.feeSat ?? p.feeSat)} sats${advanced && details?.feeRate ? ` · ${details.feeRate.toFixed(1)} sat/vB` : ""}`} />}
        {details?.blockHeight != null && <Row label="Block" value={details.blockHeight.toLocaleString()} />}
      </div>

      {txid && (
        <>
          {advanced && (
            <>
              <p className="group-label">Transaction ID</p>
              <div className="group">
                <div className="cell detail-row">
                  <span className="detail-value mono">{txid}</span>
                  <CopyButton text={txid} label="" />
                </div>
              </div>
            </>
          )}
          <button className="btn block" style={{ marginBottom: 18 }} onClick={() => void openExternal(`https://mempool.space/tx/${txid}`)}>
            <Icon name="link" size={16} />
            View on mempool.space
          </button>
        </>
      )}

      {advanced && account?.rail === "onchain" && txid && <TxLabel account={account.id} txid={txid} initial={p.description ?? ""} />}

      {pending && details && (details.canSpeedUp || details.canCancel) && (
        <div style={{ display: "grid", gap: 10, gridTemplateColumns: details.canSpeedUp && details.canCancel ? "1fr 1fr" : "1fr" }}>
          {details.canSpeedUp && (
            <button className="btn" disabled={busy} onClick={() => start("speedUp")}>
              <Icon name="bolt" size={16} />
              Speed up
            </button>
          )}
          {details.canCancel && (
            <button className="btn" disabled={busy} onClick={() => start("cancel")}>
              <Icon name="close" size={16} />
              Cancel payment
            </button>
          )}
        </div>
      )}
      {busy && !quote && <p className="group-note" style={{ textAlign: "center", marginTop: 12 }}>Working out the fee…</p>}
      {error && <p className="field-error" style={{ marginTop: 12 }}>{error}</p>}
    </Sheet>
  );
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="cell">
      <span className="grow" style={{ color: "var(--label-2)" }}>
        {label}
      </span>
      <span className="value" style={{ color: "var(--label)", textAlign: "right" }}>
        {value}
      </span>
    </div>
  );
}

/** Advanced: label the transaction; its change and coins inherit it. */
function TxLabel({ account, txid, initial }: { account: string; txid: string; initial: string }) {
  const { notify, refresh } = useWallet();
  const [label, setLabel] = useState(initial);
  const save = async () => {
    const v = label.trim();
    if (v === initial.trim()) return;
    try {
      await (await api()).setLabel(account, "tx", txid, v || null);
      notify("ok", "Label saved");
      void refresh();
    } catch (e) {
      notify("error", errorText(e));
    }
  };
  return (
    <div className="group">
      <label className="cell">
        <span className="grow">Label</span>
        <input value={label} placeholder="Only you see this" maxLength={80} style={{ textAlign: "right" }} onChange={(e) => setLabel(e.target.value)} onBlur={() => void save()} onKeyDown={(e) => e.key === "Enter" && void save()} />
      </label>
    </div>
  );
}
