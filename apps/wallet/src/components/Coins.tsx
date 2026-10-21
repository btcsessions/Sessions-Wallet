import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import type { Utxo } from "../api/types";
import { errorText, formatSats } from "../format";
import { useWallet } from "../state/store";
import { Icon } from "./Icon";

const short = (a: string) => `${a.slice(0, 7)}…${a.slice(-5)}`;

/** The account's coins, fetched and kept fresh. */
export function useUtxos(account: string | null | undefined) {
  const [utxos, setUtxos] = useState<Utxo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const reload = useCallback(async () => {
    if (!account) return;
    try {
      setUtxos(await (await api()).listUtxos(account));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, [account]);
  useEffect(() => {
    setUtxos(null);
    void reload();
  }, [reload]);
  return { utxos, error, reload };
}

/**
 * Coins in an on-chain account, Sparrow style: amount, label, address and
 * confirmations. Selectable for coin control, or editable (label, freeze).
 */
export function CoinList({
  account,
  selected,
  onToggle,
}: {
  account: string;
  /** Present: rows are checkboxes for choosing coins to spend. */
  selected?: Set<string>;
  onToggle?: (u: Utxo) => void;
}) {
  const { utxos, error, reload } = useUtxos(account);
  const [editing, setEditing] = useState<string | null>(null);
  const selectable = !!selected;

  if (error) return <p className="field-error">{error}</p>;
  if (!utxos) return <p className="empty">Loading coins…</p>;
  if (!utxos.length) return <p className="empty">No coins in this account yet.</p>;

  return (
    <ul className="coin-list">
      {utxos.map((u) => {
        const checked = selected?.has(u.outpoint) ?? false;
        return (
          <li key={u.outpoint}>
            <button
              className={`coin-row${checked ? " checked" : ""}`}
              aria-pressed={selectable ? checked : undefined}
              aria-expanded={selectable ? undefined : editing === u.outpoint}
              onClick={() => (selectable ? onToggle?.(u) : setEditing(editing === u.outpoint ? null : u.outpoint))}
            >
              {selectable && (
                <span className={`coin-check${checked ? " on" : ""}`} aria-hidden>
                  {checked && <Icon name="check" size={14} stroke={3} />}
                </span>
              )}
              <span className="grow coin-main">
                <span className="coin-label">{u.label || <em>No label</em>}</span>
                <span className="coin-meta mono">
                  {short(u.address)}
                  {u.change ? " · change" : ""}
                </span>
              </span>
              <span className="coin-side">
                <span className="num coin-amount">{formatSats(u.amountSat)}</span>
                <span className="coin-meta">
                  {u.frozen && <span className="coin-frozen">Frozen</span>}
                  {u.confirmations === 0 ? (
                    <span className="coin-pending">
                      <Icon name="clock" size={11} stroke={2.4} /> Unconfirmed
                    </span>
                  ) : (
                    `${u.confirmations.toLocaleString()} conf`
                  )}
                </span>
              </span>
            </button>
            {!selectable && editing === u.outpoint && <CoinEditor account={account} coin={u} onDone={() => (setEditing(null), void reload())} />}
          </li>
        );
      })}
    </ul>
  );
}

function CoinEditor({ account, coin, onDone }: { account: string; coin: Utxo; onDone: () => void }) {
  const notify = useWallet((s) => s.notify);
  const [label, setLabel] = useState(coin.label ?? "");
  const [busy, setBusy] = useState(false);
  const act = async (f: () => Promise<void>, done: string) => {
    setBusy(true);
    try {
      await f();
      notify("ok", done);
      onDone();
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="coin-editor">
      <label className="cell">
        <span className="visually-hidden">Label</span>
        <input value={label} placeholder="Label, only you see it" maxLength={80} onChange={(e) => setLabel(e.target.value)} />
      </label>
      <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
        <button className="btn small primary" disabled={busy} onClick={() => act(async () => (await api()).setLabel(account, "output", coin.outpoint, label.trim() || null), "Label saved")}>
          Save label
        </button>
        <button className="btn small" disabled={busy} onClick={() => act(async () => (await api()).setFrozen(account, coin.outpoint, !coin.frozen), coin.frozen ? "Coin unfrozen" : "Coin frozen")}>
          <Icon name="lock" size={14} />
          {coin.frozen ? "Unfreeze" : "Freeze"}
        </button>
      </div>
      <p className="group-note" style={{ margin: "8px 0 0" }}>
        {coin.frozen ? "Frozen coins are never spent unless you pick them yourself." : "Freeze a coin to keep it out of normal sends."}
      </p>
    </div>
  );
}
