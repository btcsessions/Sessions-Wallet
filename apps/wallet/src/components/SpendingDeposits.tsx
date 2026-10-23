import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import type { Deposit } from "../api/types";
import { errorText, formatSats } from "../format";
import { useWallet } from "../state/store";

/**
 * On-chain deposits into Spending that haven't landed yet: how far along
 * they are, and a tap to claim one when it needs approval (or to claim early).
 */
export function SpendingDeposits({ account }: { account: string }) {
  const notify = useWallet((s) => s.notify);
  const changed = useWallet((s) => s.accounts.find((a) => a.id === account)?.balance.pendingIn ?? 0);
  const [deposits, setDeposits] = useState<Deposit[]>([]);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setDeposits(await (await api()).spendingDeposits(account));
    } catch {
      /* shown again on the next refresh */
    }
  }, [account]);

  useEffect(() => {
    void load();
    const t = setInterval(() => void load(), 20_000);
    return () => clearInterval(t);
  }, [load, changed]);

  const claim = async (d: Deposit, instant: boolean) => {
    const fee = instant ? d.instantFeeSat : d.claimFeeSat;
    if (fee == null) return;
    setBusy(`${d.txid}:${d.vout}`);
    try {
      const r = await (await api()).claimDeposit(account, d.txid, d.vout, fee, instant);
      if (r.status === "deferred") notify("error", r.message ?? `The fee went up to ${formatSats(r.requiredFeeSat ?? 0)} sats. Try again.`);
      else notify("ok", r.status === "claimed" ? "Added to Spending" : "Claiming. It lands in a moment.");
      await useWallet.getState().refresh();
      await load();
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setBusy(null);
    }
  };

  if (!deposits.length) return null;
  return (
    <>
      <p className="group-label">Arriving on-chain</p>
      <div className="group">
        {deposits.map((d) => {
          const key = `${d.txid}:${d.vout}`;
          const conf = d.confirmations ?? 0;
          const sub =
            d.status === "waiting"
              ? `${Math.min(conf, d.confirmationsNeeded)} of ${d.confirmationsNeeded} confirmations, then it's added automatically`
              : d.status === "claiming"
                ? "Adding to Spending…"
                : d.status === "refunding"
                  ? "Being sent back on-chain"
                  : d.status === "needsApproval"
                    ? `Network fees are high right now. Adding it costs ${formatSats(d.claimFeeSat ?? 0)} sats.`
                    : (d.error ?? "Couldn't add it yet. It's retried automatically.");
          const canClaim = (d.status === "needsApproval" || d.status === "failed") && d.claimFeeSat != null;
          const canInstant = d.status === "waiting" && d.instantFeeSat != null;
          return (
            <div className="cell" key={key} style={{ alignItems: "flex-start", gap: 12 }}>
              <span className="grow">
                <div className="num">{formatSats(d.amountSat)} sats</div>
                <div className="row-sub">{sub}</div>
              </span>
              {canClaim && (
                <button className="btn small primary" disabled={busy !== null} onClick={() => void claim(d, false)}>
                  {busy === key ? "Adding…" : "Add now"}
                </button>
              )}
              {canInstant && (
                <button className="btn small" disabled={busy !== null} onClick={() => void claim(d, true)} title={`Fee ${formatSats(d.instantFeeSat ?? 0)} sats`}>
                  {busy === key ? "Adding…" : `Now for ${formatSats(d.instantFeeSat ?? 0)}`}
                </button>
              )}
            </div>
          );
        })}
      </div>
      <p className="group-note">Bitcoin sent to Spending's on-chain address is added once it has {deposits[0].confirmationsNeeded} confirmations. The fee comes out of the deposit.</p>
    </>
  );
}
