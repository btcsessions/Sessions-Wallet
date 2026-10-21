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
      <div className="tile-title">Total balance</div>
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

      {/* Colour bar on every size; the named breakdown when there's room. */}
      <div className="split" role="img" aria-label="Where your balance sits, by account" style={large ? undefined : { margin: "14px 0 0" }}>
        {funded.map((a) => (
          <span key={a.id} title={a.name} style={{ flexGrow: a.balance.confirmed + a.balance.pendingIn, background: accountColor(a) }} />
        ))}
      </div>
      {large && (
        <>
          <ul className="split-legend">
            {[...accounts]
              .sort((x, y) => y.balance.confirmed + y.balance.pendingIn - (x.balance.confirmed + x.balance.pendingIn))
              .map((a) => (
              <li key={a.id} style={a.balance.confirmed + a.balance.pendingIn === 0 ? { opacity: 0.5 } : undefined}>
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
