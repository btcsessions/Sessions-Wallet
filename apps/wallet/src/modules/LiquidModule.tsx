import { Icon } from "../components/Icon";
import { useWallet } from "../state/store";
import type { ModuleProps } from "./catalog";
import { LIQUID_SWAPS } from "../sheets/ReceiveSheet";

export function LiquidModule({ module }: ModuleProps) {
  const { accounts, open } = useWallet();
  const spending = accounts.find((a) => a.rail === "spark") ?? accounts[0];
  return (
    <>
      <div className="tile-title" style={{ ["--tint" as string]: "#32ADE6" }}>
        <Icon name="drop" size={16} />
        Liquid
      </div>
      <p className="tile-foot" style={{ marginTop: 0 }}>
        {!LIQUID_SWAPS
          ? "Paused. Boltz, which swapped Liquid for this wallet, suspended its service in August 2026."
          : module.size === "medium"
            ? "Paste a Liquid address in Send, or get one here. It's swapped into your bitcoin automatically."
            : "Swapped automatically."}
      </p>
      {LIQUID_SWAPS && (
      <div style={{ display: "flex", gap: 8, marginTop: "auto", flexWrap: "wrap" }}>
        <button className="btn small" onClick={() => open({ name: "pay" })}>
          Send
        </button>
        {spending && (
          <button className="btn small" onClick={() => open({ name: "receive", account: `${spending.id}#liquid` })}>
            Receive
          </button>
        )}
      </div>
      )}
    </>
  );
}
