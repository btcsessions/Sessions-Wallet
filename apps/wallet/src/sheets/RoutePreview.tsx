import type { Leg, Plan } from "../api/types";
import { Icon } from "../components/Icon";
import { METHOD_LABEL, accountColor } from "../format";
import { useWallet } from "../state/store";

/** Shows how the sats will travel, as a short vertical route. */
export function RoutePreview({ plan }: { plan: Plan }) {
  const accounts = useWallet((s) => s.accounts);
  const acct = (id: string) => accounts.find((a) => a.id === id);

  const stops: { label: string; via?: string; color: string; icon: "up" | "down" | "swap" | "drop" }[] = [];
  plan.legs.forEach((leg: Leg, i) => {
    const from = acct(leg.from);
    if (i === 0) stops.push({ label: from?.name ?? leg.from, color: from ? accountColor(from) : "var(--label-3)", icon: "up" });
    if (leg.type === "toAccount") {
      const to = acct(leg.to);
      stops.push({ label: to?.name ?? leg.to, via: `via ${METHOD_LABEL[leg.method]}`, color: to ? accountColor(to) : "var(--label-3)", icon: "down" });
    } else if (leg.type === "viaSwap") {
      stops.push({ label: "Boltz swap", via: `${METHOD_LABEL[leg.payMethod]} in, Liquid out`, color: "#32ADE6", icon: "drop" });
      stops.push({ label: "Recipient", via: "on Liquid", color: "var(--label-3)", icon: "down" });
    } else {
      const m = leg.request.type === "unified" ? "Bitcoin" : METHOD_LABEL[leg.request.type];
      stops.push({ label: "Recipient", via: `via ${m}`, color: "var(--label-3)", icon: "down" });
    }
  });

  return (
    <ol className="route" aria-label="Payment route">
      {stops.map((s, i) => (
        <li key={i}>
          <span className="node" style={{ background: s.color }}>
            <Icon name={s.icon} size={14} stroke={2.5} />
          </span>
          <span>{s.label}</span>
          <span className="via">{s.via}</span>
        </li>
      ))}
    </ol>
  );
}
