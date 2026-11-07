import { useEffect, useState } from "react";
import { api } from "../api";
import type { RecoveryQuote } from "../api/types";
import { errorText, formatSats, relativeTime } from "../format";
import { useWallet } from "../state/store";
import { AccountPicker } from "./Dropdown";
import { confirmDialog } from "./Confirm";

/** Spending hasn't synced for this long: its balance may be out of date. */
const STALE_AFTER_S = 15 * 60;
/** Give a fresh unlock time for the first sync before warning. */
const GRACE_MS = 2 * 60_000;
let since = Date.now();

/** Checked with the chain status, every minute while unlocked. */
export async function refreshSpendingHealth() {
  const s = useWallet.getState();
  const spending = s.accounts.find((a) => a.rail === "spark");
  if (!spending) return void (s.spendingHealth && useWallet.setState({ spendingHealth: null }));
  try {
    const h = await (await api()).spendingHealth(spending.id);
    useWallet.setState({ spendingHealth: h });
  } catch {
    /* not open yet: keep the last one */
  }
}

/** Reset on unlock, so a sync from last week doesn't flash a warning. */
export function resetSpendingHealth() {
  since = Date.now();
  useWallet.setState({ spendingHealth: null });
}

function rel(unix: number): string {
  const r = relativeTime(unix);
  return r === "Just now" ? "just now" : /ago$/.test(r) ? r : `on ${r}`;
}

function staleness(): { text: string } | null {
  const h = useWallet.getState().spendingHealth;
  if (!h || Date.now() - since < GRACE_MS) return null;
  const old = h.lastSyncedAt != null && Date.now() / 1000 - h.lastSyncedAt > STALE_AFTER_S;
  if (!old && !h.syncError) return null;
  const when = h.lastSyncedAt != null ? `Last synced ${rel(h.lastSyncedAt)}` : "Not synced yet";
  return { text: h.syncError ? `${h.syncError}. ${when}.` : `${when}.` };
}

/** Home: Spending's balance may be out of date, or it has funds to recover. */
export function SpendingWarning() {
  const health = useWallet((s) => s.spendingHealth);
  const accounts = useWallet((s) => s.accounts);
  const open = useWallet((s) => s.open);
  const spending = accounts.find((a) => a.rail === "spark");
  const spendingId = spending?.id;
  // First check once Spending has opened (unlock lists accounts after).
  useEffect(() => {
    if (spendingId && !useWallet.getState().spendingHealth) void refreshSpendingHealth();
  }, [spendingId]);
  if (!health || !spending) return null;
  const st = staleness();
  if (health.recoverableSat > 0) {
    return (
      <p className="notice" role="alert">
        <strong>{formatSats(health.recoverableSat)} sats left Spending for the bitcoin network.</strong> They're yours to bring back.{" "}
        <button className="text-button strong" style={{ padding: 0 }} onClick={() => open({ name: "account", id: spending.id })}>
          Recover
        </button>
      </p>
    );
  }
  if (!st) return null;
  return (
    <p className="notice" role="alert">
      <strong>Spending's balance may be out of date.</strong> {st.text}{" "}
      <button className="text-button strong" style={{ padding: 0 }} onClick={() => void useWallet.getState().refresh().then(refreshSpendingHealth)}>
        Sync now
      </button>
    </p>
  );
}

/** Spending's account sheet: last sync, and recovering funds that left it. */
export function SpendingRecovery({ account }: { account: string }) {
  const notify = useWallet((s) => s.notify);
  const health = useWallet((s) => s.spendingHealth);
  const accounts = useWallet((s) => s.accounts);
  // Savings (keys on this phone) first, then hardware and watch-only.
  const targets = accounts
    .filter((a) => a.rail === "onchain" && a.receives.includes("bitcoinAddress"))
    .sort((x, y) => Number(y.custody.type === "hot") - Number(x.custody.type === "hot"));
  const [to, setTo] = useState<string | undefined>(targets[0]?.id);
  const [quote, setQuote] = useState<RecoveryQuote | null>(null);
  const [busy, setBusy] = useState<"quote" | "send" | null>(null);

  useEffect(() => {
    void refreshSpendingHealth();
  }, [account]);
  useEffect(() => {
    if (!to && targets[0]) setTo(targets[0].id);
  }, [to, targets]);

  const check = async () => {
    if (!to) return;
    setBusy("quote");
    setQuote(null);
    try {
      const a = await api();
      const target = await a.receive(to, "bitcoinAddress", null, null);
      const address = target.request.replace(/^bitcoin:/i, "").split("?")[0];
      const rates = await a.feeRates();
      setQuote(await a.prepareSpendingRecovery(account, address, Math.ceil(rates.normal)));
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setBusy(null);
    }
  };

  const recover = async () => {
    if (!quote) return;
    const name = targets.find((t) => t.id === to)?.name ?? "Savings";
    if (!(await confirmDialog(`Send ${formatSats(quote.arrivesSat)} sats to ${name}? ${formatSats(quote.feeSat)} sats go to network fees.`, { ok: "Recover" }))) return;
    setBusy("send");
    try {
      const txids = await (await api()).recoverSpending(account);
      notify("ok", txids.length > 1 ? `Recovering in ${txids.length} transactions. They arrive in ${name} once confirmed.` : `Recovering. It arrives in ${name} once confirmed.`);
      setQuote(null);
      await refreshSpendingHealth();
      await useWallet.getState().refresh();
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setBusy(null);
    }
  };

  if (!health) return null;
  const st = staleness();
  return (
    <>
      {st && (
        <p className="notice" role="status">
          <strong>Balance may be out of date.</strong> {st.text}
        </p>
      )}
      {health.recoverableSat > 0 && (
        <>
          <p className="group-label">Recover funds</p>
          <div className="group">
            <div className="cell" style={{ display: "block" }}>
              <div className="num">{formatSats(health.recoverableSat)} sats</div>
              <div className="row-sub">Left Spending for the bitcoin network. Bring them back on-chain to an account of yours.</div>
            </div>
          </div>
          {targets.length ? (
            <>
              <AccountPicker label="Send to" accounts={targets} value={to} onChange={(id) => (setTo(id), setQuote(null))} />
              {quote && (
                <p className="group-note" role="status">
                  {quote.valueSat > 0
                    ? `${formatSats(quote.arrivesSat)} sats arrive after ${formatSats(quote.feeSat)} sats in network fees.`
                    : "Nothing can be recovered this way right now."}
                  {quote.leftOutSat > 0 &&
                    ` ${formatSats(quote.leftOutSat)} sats ${quote.needsExit ? "need an emergency exit, which isn't in the wallet yet. They stay safe and recoverable" : "can't be recovered yet (too small for the fee, or still being found)"}.`}
                </p>
              )}
              {quote && quote.valueSat > 0 && quote.arrivesSat > 0 ? (
                <button className="btn primary block" disabled={busy !== null} onClick={() => void recover()} style={{ marginBottom: 10 }}>
                  {busy === "send" ? "Recovering…" : `Recover ${formatSats(quote.arrivesSat)} sats`}
                </button>
              ) : (
                <button className="btn block" disabled={busy !== null || !to} onClick={() => void check()} style={{ marginBottom: 10 }}>
                  {busy === "quote" ? "Checking…" : "Check recovery"}
                </button>
              )}
            </>
          ) : (
            <p className="group-note">Add a Savings account to recover these to.</p>
          )}
        </>
      )}
    </>
  );
}
