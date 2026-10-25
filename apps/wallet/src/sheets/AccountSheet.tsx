import { ActivityRow } from "../modules/ActivityModule";
import { walletAccounts } from "../modules/AccountModule";
import { Dropdown, accountOptions } from "../components/Dropdown";
import { DiscoverHot } from "../components/Discover";
import { LightningAddressCard } from "./LightningAddressCard";
import { useEffect, useState, type ReactNode } from "react";
import { api } from "../api";
import { Icon } from "../components/Icon";
import { CopyButton, Sheet, useBusy } from "../components/ui";
import type { DetailRow } from "../api/types";
import { DeviceIcon, HardwareBadge } from "../hw/devices";
import { accountColor, accountKind, amountParts, errorText, formatFiat, held } from "../format";
import { PendingClock } from "../components/PendingClock";
import { SpendingDeposits } from "../components/SpendingDeposits";
import { SweepSetting } from "../components/Sweep";
import { useWallet } from "../state/store";

export function AccountSheet({ id }: { id: string }) {
  const { accounts, activity, unit, price, currency, open, refresh, notify, close } = useWallet();
  const advanced = useWallet((s) => s.layout?.advanced ?? false);
  const [busy, run] = useBusy();
  const a = accounts.find((x) => x.id === id);
  if (!a) return null;
  const { value, suffix } = amountParts(held(a), unit, price, currency);
  const items = activity.filter((p) => p.account === id);
  const siblings = walletAccounts(accounts, a);

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
      {(siblings.length > 1 || (advanced && a.group)) && (
        <div className="group" style={{ margin: "14px 0 6px" }}>
          <Dropdown
            label="Account"
            value={a.id}
            options={[
              ...accountOptions(siblings).map((o, i) => ({ ...o, detail: `Account ${(siblings[i].accountIndex ?? i) + 1} · ${o.detail?.split(" · ").pop()}` })),
              ...(advanced ? [{ value: "__add", label: "Add an account", detail: a.group === "hot" ? "Another account from your recovery phrase" : "Another account from this device", color: "var(--accent)", icon: <Icon name="plus" size={14} stroke={2.5} /> }] : []),
            ]}
            onChange={(v) => (v === "__add" ? open({ name: "add-account", kind: a.group === "hot" ? "savings" : "hardware" }) : open({ name: "account", id: v }))}
          />
        </div>
      )}
      {a.scanning && (
        <p className="device-progress" style={{ flexDirection: "row", justifyContent: "center", padding: "10px 14px" }}>
          <span className="spinner small" aria-hidden />
          <span>
            Loading this wallet's history. Past payments and the balance fill in when it's done.
            {(a.scanStatus || a.scanError) && (
              <>
                <br />
                <small style={{ color: "var(--label-2)" }}>{a.scanStatus ?? a.scanError}</small>
              </>
            )}
          </span>
        </p>
      )}
      <div className="amount-entry" style={{ paddingTop: 4 }}>
        <div className="big-stat num" style={{ fontSize: 44 }}>
          {value}
          {suffix && <span className="balance-unit">{suffix}</span>}
          <PendingClock sats={a.balance.pendingIn} size={18} />
        </div>
        <div className="balance-sub">
          {a.custody.type === "hardware" ? (
            formatFiat(held(a), price, currency)
          ) : (
            <>
              <span style={{ display: "inline-block", width: 8, height: 8, borderRadius: 2, background: accountColor(a), marginRight: 6 }} />
              {accountKind(a)}
            </>
          )}
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
            Transfer
          </button>
        )}
      </div>

      {a.rail === "spark" && <SpendingDeposits account={a.id} />}

      {a.rail === "spark" && (
        <>
          <p className="group-label">Lightning address</p>
          <LightningAddressCard account={a.id} />
          <SweepSetting account={a} />
        </>
      )}

      {a.rail === "onchain" && a.custody.type === "hot" && <PayjoinSwitch account={a.id} />}

      {a.custody.type === "mint" && (
        <p className="notice">
          Held by the mint at {a.custody.url}. Keep amounts small and transfer larger ones to Spending or a vault.
        </p>
      )}
      {a.custody.type === "multisig" && (
        <p className="notice">
          Spending needs {a.custody.threshold} of {a.custody.total} keys. This device holds {a.custody.localSigners}.
        </p>
      )}

      {advanced && a.rail === "onchain" && (
        <button className="btn block" style={{ marginBottom: 12 }} onClick={() => open({ name: "coins", account: a.id })}>
          <Icon name="coins" size={16} />
          Coins and labels
        </button>
      )}
      {advanced && a.group === "hot" && (
        <>
          <p className="group-label">Other accounts</p>
          <DiscoverHot />
          <div style={{ height: 18 }} />
        </>
      )}
      <WalletDetails id={a.id} name={a.name}>
        {a.id !== "spending" && a.balance.confirmed + a.balance.pendingIn + a.balance.pendingOut === 0 && (
          <button className="btn block" style={{ color: "var(--red)", marginBottom: 22 }} disabled={busy} onClick={remove}>
            Remove {a.name}
          </button>
        )}
      </WalletDetails>

      <p className="group-label">Activity</p>
      {items.length ? (
        <ul className="list">
          {items.map((p) => (
            <ActivityRow key={p.id} p={p} back={a.id} />
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

/** Technical details, one tap away so they don't crowd the account. */
/** Rows only shown in Advanced mode. */
const TECHNICAL = new Set(["Address type", "Fingerprint", "Derivation path", "Extended public key", "Descriptor", "Receives with", "Account id", "Policy"]);

function WalletDetails({ id, name, children }: { id: string; name: string; children?: ReactNode }) {
  const advanced = useWallet((s) => s.layout?.advanced ?? false);
  const [rows, setRows] = useState<DetailRow[] | null>(null);
  const [open, setOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const toggle = async () => {
    if (open) return setOpen(false);
    setOpen(true);
    if (!rows) {
      try {
        setRows(await (await api()).accountDetails(id));
      } catch (e) {
        setError(errorText(e));
      }
    }
  };

  return (
    <>
      <p style={{ textAlign: "center", margin: "4px 0 14px" }}>
        <button className="text-button with-icon subtle" aria-expanded={open} onClick={() => void toggle()}>
          <Icon name="key" size={14} />
          {open ? "Hide wallet details" : "Wallet details"}
        </button>
      </p>
      {open && <RenameRow id={id} name={name} />}
      {open && (
        <div className="group details">
          {error && <div className="cell field-error">{error}</div>}
          {!rows && !error && <div className="cell empty">Loading…</div>}
          {rows?.filter((r) => advanced || !TECHNICAL.has(r.label)).map((r, i) => (
            <div className="cell detail-row" key={`${r.label}-${i}`}>
              <span className="detail-label">{r.label}</span>
              <span className={`detail-value${r.mono ? " mono" : ""}`}>{r.value}</span>
              {r.mono && <CopyButton text={r.value} label="" />}
            </div>
          ))}
        </div>
      )}
      {open && children}
    </>
  );
}

/** Every account can be labelled. */
function RenameRow({ id, name }: { id: string; name: string }) {
  const { refresh, notify } = useWallet();
  const [value, setValue] = useState(name);
  useEffect(() => setValue(name), [name]);
  const save = async () => {
    const v = value.trim();
    if (!v || v === name) return setValue(name);
    try {
      await (await api()).renameAccount(id, v);
      await refresh();
      notify("ok", "Renamed");
    } catch (e) {
      notify("error", errorText(e));
      setValue(name);
    }
  };
  return (
    <div className="group">
      <label className="cell">
        <span className="grow">Name</span>
        <input value={value} maxLength={32} style={{ textAlign: "right" }} onChange={(e) => setValue(e.target.value)} onBlur={() => void save()} onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()} />
      </label>
    </div>
  );
}
