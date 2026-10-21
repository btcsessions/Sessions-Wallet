import { Icon } from "../components/Icon";
import { useWallet } from "../state/store";
import type { ModuleProps } from "./catalog";

export function PriceModule({ module }: ModuleProps) {
  const { price, currency } = useWallet();
  const fmt = (v: number) => new Intl.NumberFormat(undefined, { style: "currency", currency, maximumFractionDigits: 0 }).format(v);
  const fmtSmall = (v: number) => new Intl.NumberFormat(undefined, { style: "currency", currency, maximumFractionDigits: 2 }).format(v);
  return (
    <>
      <div className="tile-title">
        <Icon name="chart" size={16} />
        Bitcoin
      </div>
      <div className="big-stat num">{price ? fmt(price) : "—"}</div>
      <div className="tile-foot">
        {price ? (
          <>
            {Math.round(1e8 / price).toLocaleString()} sats for 1 {currency}
            {module.size === "medium" && <>, so 1,000 sats is about {fmtSmall(price / 1e5)}</>}
          </>
        ) : (
          "Price unavailable offline"
        )}
      </div>
    </>
  );
}
