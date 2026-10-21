import { ActivityRow } from "../modules/ActivityModule";
import { Sheet } from "../components/ui";
import { accountColor, accountKind, amountParts, formatSats } from "../format";
import { useWallet } from "../state/store";

export function AccountSheet({ id }: { id: string }) {
  const { accounts, activity, unit, price, currency, open, layout } = useWallet();
  const a = accounts.find((x) => x.id === id);
  if (!a) return null;
  const { value, suffix } = amountParts(a.balance.confirmed, unit, price, currency);
  const items = activity.filter((p) => p.account === id);

  return (
    <Sheet title={a.name}>
      <div className="amount-entry" style={{ paddingTop: 4 }}>
        <div className="big-stat num" style={{ fontSize: 44 }}>
          {value}
          {suffix && <span className="balance-unit">{suffix}</span>}
        </div>
        <div className="balance-sub">
          <span style={{ display: "inline-block", width: 8, height: 8, borderRadius: 2, background: accountColor(a), marginRight: 6 }} />
          {accountKind(a)}
          {a.balance.pendingIn > 0 && <>, {formatSats(a.balance.pendingIn)} sats on the way</>}
        </div>
      </div>

      <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 10, margin: "8px 0 24px" }}>
        {a.pays.length > 0 && (
          <button className="btn" onClick={() => open({ name: "move", from: a.id })}>
            Move out
          </button>
        )}
        <button className="btn" onClick={() => open({ name: "receive", account: a.id })}>
          Receive
        </button>
      </div>

      {a.custody.type === "mint" && (
        <p className="notice">
          Held by the mint at {a.custody.url}. Move larger amounts to Spending or Savings.
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
