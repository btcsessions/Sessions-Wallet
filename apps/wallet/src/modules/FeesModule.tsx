import { useEffect, useState } from "react";
import { api } from "../api";
import type { FeeRates } from "../api/types";
import { Icon } from "../components/Icon";
import type { ModuleProps } from "./catalog";

/** Typical single-input, two-output P2WPKH payment. */
const TYPICAL_VBYTES = 141;

export function FeesModule({ module }: ModuleProps) {
  const [fees, setFees] = useState<FeeRates | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let alive = true;
    // Goes through the wallet's own chain server, so a user running their
    // own node never leaks fee lookups to a third party.
    api()
      .then((a) => a.feeRates())
      .then((f) => alive && setFees(f))
      .catch(() => alive && setFailed(true));
    return () => {
      alive = false;
    };
  }, []);

  const rows: [string, number][] = fees
    ? [
        ["Next block", fees.fast],
        ["Within an hour", fees.normal],
        ["No rush", fees.slow],
      ]
    : [];

  return (
    <>
      <div className="tile-title" style={{ ["--tint" as string]: "#FF2D55" }}>
        <Icon name="gauge" size={16} />
        Network fees
      </div>
      {fees ? (
        module.size === "small" ? (
          <>
            <div className="big-stat num">{fees.normal}</div>
            <div className="tile-foot">sat/vB, about {(fees.normal * TYPICAL_VBYTES).toLocaleString()} sats a payment</div>
          </>
        ) : (
          <div className="fee-bars num">
            {rows.map(([label, rate]) => (
              <div key={label}>
                <span>{label}</span>
                <span>
                  <b>{rate} sat/vB</b> ≈ {(rate * TYPICAL_VBYTES).toLocaleString()} sats
                </span>
              </div>
            ))}
          </div>
        )
      ) : (
        <p className="tile-foot" style={{ marginTop: 0 }}>{failed ? "Couldn't reach your chain server." : "Checking…"}</p>
      )}
    </>
  );
}
