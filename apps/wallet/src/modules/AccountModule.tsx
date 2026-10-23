import { useEffect, useState } from "react";
import { api } from "../api";
import { Icon } from "../components/Icon";
import { DeviceIcon, HardwareBadge } from "../hw/devices";
import { useWallet } from "../state/store";
import { accountColor, accountKind, amountParts, held } from "../format";
import { PendingClock } from "../components/PendingClock";
import type { AccountInfo } from "../api/types";
import { moduleAccount, moduleDef, moduleType, type ModuleProps } from "./catalog";

/** Which account a generic account module shows. */
export function resolveAccount(accounts: AccountInfo[], moduleId: string): AccountInfo | undefined {
  const explicit = moduleAccount({ id: moduleId, size: "small" });
  if (explicit) return accounts.find((a) => a.id === explicit);
  switch (moduleType(moduleId)) {
    case "spending":
      return accounts.find((a) => a.rail === "spark");
    case "savings":
      return accounts.find((a) => a.rail === "onchain" && a.custody.type === "hot");
  }
  return undefined;
}

/**
 * Accounts the user is actually using: Spending, anything on the home screen,
 * and anything holding funds. Accounts that only exist because the seed can
 * derive them (like the hot on-chain wallet) stay out of pickers until added.
 */
export function activeAccounts(accounts: AccountInfo[], layout: { modules: { id: string }[] } | null | undefined): AccountInfo[] {
  const onHome = new Set((layout?.modules ?? []).map((m) => resolveAccount(accounts, m.id)?.id).filter(Boolean));
  // Accounts inside a wallet whose card is on the home screen count too.
  const groups = new Set(accounts.filter((a) => onHome.has(a.id) && a.group).map((a) => a.group));
  return accounts.filter((a) => a.rail === "spark" || onHome.has(a.id) || (a.group && groups.has(a.group)) || a.balance.confirmed + a.balance.pendingIn + a.balance.pendingOut > 0);
}

/** Is this account's wallet already shown by a card on the home screen? */
export function walletHasCard(accounts: AccountInfo[], layout: { modules: { id: string }[] } | null | undefined, a: AccountInfo): boolean {
  if (!a.group) return false;
  return (layout?.modules ?? []).some((m) => {
    const shown = resolveAccount(accounts, m.id);
    return !!shown && shown.id !== a.id && shown.group === a.group;
  });
}

/** The accounts in the same wallet as `a` (itself included), in account-number order. */
export function walletAccounts(accounts: AccountInfo[], a: AccountInfo): AccountInfo[] {
  if (!a.group) return [a];
  return accounts.filter((x) => x.group === a.group).sort((x, y) => (x.accountIndex ?? 0) - (y.accountIndex ?? 0));
}

export function AccountModule({ module }: ModuleProps) {
  const { accounts, unit, price, currency, open } = useWallet();
  const def = moduleDef(module.id)!;
  const account = resolveAccount(accounts, module.id);

  if (!account) {
    return (
      <>
        <div className="tile-title" style={{ ["--tint" as string]: def.tint }}>
          <Icon name={def.icon} size={16} />
          {def.name}
        </div>
        <p className="tile-foot" style={{ marginTop: 0 }}>{def.blurb}</p>
        {def.setup && (
          <button className="btn small primary" style={{ marginTop: "auto", alignSelf: "flex-start" }} onClick={() => open({ name: "add-account", kind: def.setup! })}>
            Set up
          </button>
        )}
      </>
    );
  }

  // A wallet holding several accounts shows them as one card with their total.
  const siblings = walletAccounts(accounts, account);
  const multi = siblings.length > 1;
  const total = siblings.reduce((s, a) => s + held(a), 0);
  const pending = siblings.reduce((s, a) => s + a.balance.pendingIn, 0);
  const title = multi ? (account.custody.type === "hardware" ? account.custody.device : "Hot wallet") : account.name;
  const { value, suffix } = amountParts(total, unit, price, currency);
  return (
    <>
      {multi && <span className="stack-edge" aria-hidden />}
      <div className="tile-title" style={{ ["--tint" as string]: accountColor(account) }}>
        {account.custody.type === "hardware" ? <DeviceIcon device={account.custody.device} size={17} /> : <Icon name={def.icon} size={16} />}
        {title}
      </div>
      <div className="balance-line">
        <button style={{ textAlign: "left" }} data-tile-open onClick={() => open({ name: "account", id: account.id })} aria-label={`${title}: ${value} ${suffix}${multi ? `, ${siblings.length} accounts` : ""}`}>
        <div className="big-stat num fit" style={{ ["--len" as string]: value.length }}>
          {value}
          {suffix && <span className="balance-unit">{suffix}</span>}
        </div>
      </button>
        <PendingClock sats={pending} />
      </div>
      <div className="tile-foot" style={{ marginTop: 4, display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
        {multi && (
          <button className="accounts-chip" onClick={() => open({ name: "account", id: account.id })}>
            <Icon name="stack" size={13} stroke={2.2} />
            {siblings.length} accounts
          </button>
        )}
        {account.rail === "spark" && <DepositChip account={account.id} pending={account.balance.pendingIn} />}
        {siblings.some((a) => a.scanning) && (
          <span className={`loading-chip${siblings.some((a) => a.scanError) ? " retrying" : ""}`} title={siblings.find((a) => a.scanError)?.scanError ?? "Loading this wallet's history"}>
            <span className="spinner small" aria-hidden />
            {siblings.some((a) => a.scanError) ? "Retrying history" : "Loading history"}
          </span>
        )}
        {account.custody.type === "hardware" ? <HardwareBadge device={account.custody.device} compact={module.size === "small" || multi} /> : !multi && accountKind(account)}
      </div>
      <div className="account-actions">
        {account.pays.length > 0 && (
          <button className="round-action" aria-label={`Send from ${account.name}`} title="Send" onClick={() => open({ name: "pay", from: account.id })}>
            <Icon name="up" size={18} stroke={2.2} />
          </button>
        )}
        <button className="round-action" aria-label={`Receive to ${account.name}`} title="Receive" onClick={() => open({ name: "receive", account: account.id })}>
          <Icon name="down" size={18} stroke={2.2} />
        </button>
        {account.pays.length > 0 && accounts.length > 1 && (
          <button className="round-action" aria-label={`Transfer from ${account.name}`} title="Transfer" onClick={() => open({ name: "move", from: account.id })}>
            <Icon name="swap" size={18} stroke={2.2} />
          </button>
        )}
        {account.rail === "ecash" && (
          <button className="round-action" aria-label={`Send ${account.name} as a link`} title="Send as link" onClick={() => open({ name: "ecash-token", account: account.id })}>
            <Icon name="ticket" size={18} stroke={2} />
          </button>
        )}
      </div>
    </>
  );
}

/** On the Spending card: an on-chain deposit is on its way, or needs a tap. */
function DepositChip({ account, pending }: { account: string; pending: number }) {
  const [state, setState] = useState<"none" | "arriving" | "approve">("none");
  useEffect(() => {
    if (pending <= 0) {
      setState("none");
      return;
    }
    let alive = true;
    api()
      .then((a) => a.spendingDeposits(account))
      .then((ds) => alive && setState(ds.some((d) => d.status === "needsApproval" || d.status === "failed") ? "approve" : ds.length ? "arriving" : "none"))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [account, pending]);
  if (state === "none") return null;
  return (
    <span className={`loading-chip${state === "approve" ? " retrying" : ""}`}>
      {state === "approve" ? "Deposit: tap to add" : "Deposit arriving"}
    </span>
  );
}
