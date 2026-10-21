import { Icon } from "../components/Icon";
import { DeviceIcon, HardwareBadge } from "../hw/devices";
import { useWallet } from "../state/store";
import { accountColor, accountKind, amountParts } from "../format";
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

  const { value, suffix } = amountParts(account.balance.confirmed, unit, price, currency);
  return (
    <>
      <div className="tile-title" style={{ ["--tint" as string]: accountColor(account) }}>
        {account.custody.type === "hardware" ? <DeviceIcon device={account.custody.device} size={17} /> : <Icon name={def.icon} size={16} />}
        {account.name}
      </div>
      <button style={{ textAlign: "left" }} onClick={() => open({ name: "account", id: account.id })} aria-label={`${account.name}: ${value} ${suffix}`}>
        <div className="big-stat num">
          {value}
          {suffix && <span className="balance-unit">{suffix}</span>}
        </div>
      </button>
      <div className="tile-foot" style={{ marginTop: 4 }}>{account.custody.type === "hardware" ? <HardwareBadge device={account.custody.device} compact={module.size === "small"} /> : accountKind(account)}</div>
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
