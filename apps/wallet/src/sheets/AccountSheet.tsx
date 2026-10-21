import { ActivityRow } from "../modules/ActivityModule";
import { LightningAddressCard } from "./LightningAddressCard";
import { useEffect, useState } from "react";
import { api } from "../api";
import { Icon } from "../components/Icon";
import { Sheet, useBusy } from "../components/ui";
import { DeviceIcon, HardwareBadge } from "../hw/devices";
import { accountColor, accountKind, amountParts, errorText, formatSats } from "../format";
import { useWallet } from "../state/store";

export function AccountSheet({ id }: { id: string }) {
  const { accounts, activity, unit, price, currency, open, layout, refresh, notify, close } = useWallet();
  const [busy, run] = useBusy();
  const a = accounts.find((x) => x.id === id);
  if (!a) return null;
  const { value, suffix } = amountParts(a.balance.confirmed, unit, price, currency);
  const items = activity.filter((p) => p.account === id);

  const remove = () =>
    run(async () => {
      if (!window.confirm(`Remove ${a.name}? It's empty. Its keys come from your recovery phrase, so you can add it back any time.`)) return;
      await (await api()).removeAccount(a.id);
      await refresh();
      notify("ok", `Removed ${a.name}`);
      close();
    });

  return (
    <Sheet title={a.name}>
      {a.custody.type === "hardware" && (
        <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 10, marginTop: 4 }}>
          <div className="hw-hero" style={{ margin: 0, width: 72, height: 72, borderRadius: 20 }}>
            <DeviceIcon device={a.custody.device} size={44} />
          </div>
          <HardwareBadge device={a.custody.device} />
        </div>
      )}
      <div className="amount-entry" style={{ paddingTop: 4 }}>
        <div className="big-stat num" style={{ fontSize: 44 }}>
          {value}
          {suffix && <span className="balance-unit">{suffix}</span>}
        </div>
        <div className="balance-sub">
          <span style={{ display: "inline-block", width: 8, height: 8, borderRadius: 2, background: accountColor(a), marginRight: 6 }} />
          {a.custody.type === "hardware" ? `Fingerprint ${a.custody.fingerprint}` : accountKind(a)}
          {a.balance.pendingIn > 0 && <>, {formatSats(a.balance.pendingIn)} sats on the way</>}
        </div>
      </div>

      <div className="three-actions" style={{ margin: "8px 0 24px", gridTemplateColumns: `repeat(${a.pays.length > 0 ? 3 : 1}, 1fr)` }}>
        {a.pays.length > 0 && (
          <button className="btn primary" onClick={() => open({ name: "pay", from: a.id })}>
            <Icon name="up" size={20} />
            Send
          </button>
        )}
        <button className="btn" onClick={() => open({ name: "receive", account: a.id })}>
          <Icon name="down" size={20} />
          Receive
        </button>
        {a.pays.length > 0 && accounts.length > 1 && (
          <button className="btn" onClick={() => open({ name: "move", from: a.id })}>
            <Icon name="swap" size={20} />
            Move
          </button>
        )}
      </div>

      {a.rail === "spark" && (
        <>
          <p className="group-label">Lightning address</p>
          <LightningAddressCard account={a.id} />
        </>
      )}

      {a.rail === "onchain" && a.custody.type === "hot" && <PayjoinSwitch account={a.id} />}

      {a.custody.type === "mint" && (
        <p className="notice">
          Held by the mint at {a.custody.url}. Keep amounts small and move larger ones to Spending or a vault.
        </p>
      )}
      {a.custody.type === "hardware" && (
        <p className="notice">
          Keys stay on your {a.custody.device}. Sending prepares the transaction here; you check and sign it on the device.
        </p>
      )}
      {a.custody.type === "multisig" && (
        <p className="notice">
          Spending needs {a.custody.threshold} of {a.custody.total} keys. This device holds {a.custody.localSigners}.
        </p>
      )}

      {layout?.advanced && (
        <>
          <p className="group-label">Details</p>
          <div className="group">
            <div className="cell">
              <span className="grow">Account id</span>
              <span className="value">{a.id}</span>
            </div>
            <div className="cell">
              <span className="grow">Receives</span>
              <span className="value">{a.receives.join(", ")}</span>
            </div>
          </div>
        </>
      )}

      {a.id !== "spending" && a.balance.confirmed + a.balance.pendingIn + a.balance.pendingOut === 0 && (
        <button className="btn block" style={{ color: "var(--red)", marginBottom: 22 }} disabled={busy} onClick={remove}>
          Remove {a.name}
        </button>
      )}

      <p className="group-label">Activity</p>
      {items.length ? (
        <ul className="list">
          {items.map((p) => (
            <ActivityRow key={p.id} p={p} />
          ))}
        </ul>
      ) : (
        <p className="empty">Nothing here yet.</p>
      )}
    </Sheet>
  );
}

function PayjoinSwitch({ account }: { account: string }) {
  const notify = useWallet((s) => s.notify);
  const [on, setOn] = useState<boolean | null>(null);
  useEffect(() => {
    api()
      .then((a) => a.payjoinReceive(account))
      .then(setOn)
      .catch(() => setOn(false));
  }, [account]);
  const toggle = async (v: boolean) => {
    setOn(v);
    try {
      await (await api()).setPayjoinReceive(account, v);
    } catch (e) {
      setOn(!v);
      notify("error", errorText(e));
    }
  };
  return (
    <>
      <div className="group">
        <label className="cell">
          <span className="grow">
            <div>Offer payjoin when receiving</div>
            <div className="row-sub">Payjoin-capable senders mix one of your coins into their payment, which breaks common chain analysis.</div>
          </span>
          <input
            type="checkbox"
            role="switch"
            disabled={on === null}
            checked={!!on}
            onChange={(e) => void toggle(e.target.checked)}
            style={{ flex: "none", width: 22, height: 22, accentColor: "var(--accent)" }}
          />
        </label>
      </div>
      <p className="group-note">Makes your receive QR longer. Needs at least one confirmed coin in this wallet; otherwise a normal address is shown.</p>
    </>
  );
}
