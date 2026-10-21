import { Icon } from "../components/Icon";
import { useWallet } from "../state/store";
import { METHOD_LABEL, relativeTime, signed } from "../format";
import type { Payment } from "../api/types";
import type { ModuleProps } from "./catalog";

export function ActivityRow({ p }: { p: Payment }) {
  const accounts = useWallet((s) => s.accounts);
  const account = accounts.find((a) => a.id === p.account);
  const title =
    p.description ||
    (p.direction === "incoming" ? `Received via ${METHOD_LABEL[p.method]}` : p.direction === "internal" ? "Transferred between accounts" : `Sent via ${METHOD_LABEL[p.method]}`);
  return (
    <li className="row">
      <span className={`row-icon ${p.direction === "incoming" ? "in" : ""}`}>
        <Icon name={p.direction === "incoming" ? "down" : p.direction === "internal" ? "swap" : "up"} size={18} />
      </span>
      <span className="row-main">
        <div className="row-title">{title}</div>
        <div className="row-sub">
          {relativeTime(p.timestamp)}
          {account && `, ${account.name}`}
          {p.status === "pending" && <span className="pending-chip">Pending</span>}
          {p.status === "failed" && <span className="pending-chip" style={{ color: "var(--red)" }}>Failed</span>}
        </div>
      </span>
      {/* Sends show what left the wallet: amount plus fee. */}
      <span className={`row-amount num ${p.direction === "incoming" ? "in" : ""}`}>
        {signed(p.direction === "outgoing" ? p.amountSat + p.feeSat : p.amountSat, p.direction)}
      </span>
    </li>
  );
}

export function ActivityModule({ module }: ModuleProps) {
  const activity = useWallet((s) => s.activity);
  const shown = activity.slice(0, module.size === "large" ? 6 : 3);
  return (
    <>
      <div className="tile-title">
        <Icon name="list" size={16} />
        Activity
      </div>
      {shown.length === 0 ? (
        <p className="empty">Payments you send and receive will show up here.</p>
      ) : (
        <ul className="list">
          {shown.map((p) => (
            <ActivityRow key={p.id} p={p} />
          ))}
        </ul>
      )}
    </>
  );
}
