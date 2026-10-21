import { Icon } from "../components/Icon";
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
        <Icon name={def.icon} size={16} />
        {account.name}
      </div>
      <button style={{ textAlign: "left" }} onClick={() => open({ name: "account", id: account.id })} aria-label={`${account.name}: ${value} ${suffix}`}>
        <div className="big-stat num">
          {value}
          {suffix && <span className="balance-unit">{suffix}</span>}
        </div>
      </button>
      <div className="tile-foot">{accountKind(account)}</div>
      {module.size === "medium" && (
        <div style={{ display: "flex", gap: 8, marginTop: 12 }}>
          {account.pays.length > 0 && (
            <button className="btn small" onClick={() => open({ name: "move", from: account.id })}>
              Move out
            </button>
          )}
          <button className="btn small" onClick={() => open({ name: "receive", account: account.id })}>
            Receive
          </button>
          {account.rail === "ecash" && (
            <button className="btn small" onClick={() => open({ name: "ecash-token", account: account.id })}>
              Send as link
            </button>
          )}
        </div>
      )}
    </>
  );
}
