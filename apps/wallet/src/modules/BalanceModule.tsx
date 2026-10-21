import { useWallet } from "../state/store";
import { accountColor, amountParts, formatFiat, formatSats } from "../format";
import type { ModuleProps } from "./catalog";

export function BalanceModule({ module }: ModuleProps) {
  const { accounts, unit, price, currency, cycleUnit, open } = useWallet();
  const total = accounts.reduce((s, a) => s + a.balance.confirmed, 0);
  const incoming = accounts.reduce((s, a) => s + a.balance.pendingIn, 0);
  const { value, suffix } = amountParts(total, unit, price, currency);
  const large = module.size === "large";
  const funded = accounts.filter((a) => a.balance.confirmed + a.balance.pendingIn > 0);

  return (
    <>
      <button
        className="balance-amount num"
        onClick={cycleUnit}
        aria-label={`Total balance ${value} ${suffix}. Tap to change unit.`}
      >
        {value}
        {suffix && <span className="balance-unit">{suffix}</span>}
      </button>
      <div className="balance-sub num">
        {unit === "fiat" ? `${formatSats(total)} sats` : formatFiat(total, price, currency)}
        {incoming > 0 && <> and {formatSats(incoming)} sats on the way</>}
      </div>

      {large && (
        <>
          <div className="split" role="img" aria-label="How your balance is split across accounts">
            {funded.map((a) => (
              <span key={a.id} style={{ flexGrow: a.balance.confirmed + a.balance.pendingIn, background: accountColor(a) }} />
            ))}
          </div>
          <ul className="split-legend">
            {accounts.map((a) => (
              <li key={a.id}>
                <span className="dot" style={{ background: accountColor(a) }} />
                <button className="name" style={{ textAlign: "left" }} onClick={() => open({ name: "account", id: a.id })}>
                  {a.name}
                </button>
                <span className="amt num">{amountParts(a.balance.confirmed, unit === "fiat" ? "fiat" : "sats", price, currency).value}</span>
              </li>
            ))}
          </ul>
        </>
      )}
    </>
  );
}
