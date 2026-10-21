import { Icon } from "../components/Icon";
import { useWallet } from "../state/store";
import type { ModuleProps } from "./catalog";

export function MoveModule({ module }: ModuleProps) {
  const { accounts, open } = useWallet();
  const spendable = accounts.filter((a) => a.pays.length > 0 && a.balance.confirmed > 0);
  return (
    <>
      <div className="tile-title" style={{ ["--tint" as string]: "var(--blue)" }}>
        <Icon name="swap" size={16} />
        Move
      </div>
      {module.size === "medium" ? (
        <div className="chips" style={{ flexWrap: "wrap", paddingBottom: 0 }}>
          {spendable.map((a) =>
            accounts
              .filter((b) => b.id !== a.id && b.custody.type !== "watchOnly")
              .slice(0, 2)
              .map((b) => (
                <button key={`${a.id}-${b.id}`} className="chip" onClick={() => open({ name: "move", from: a.id, to: b.id })}>
                  {a.name} to {b.name}
                </button>
              )),
          )}
        </div>
      ) : (
        <p className="tile-foot" style={{ marginTop: 0 }}>Between any two accounts.</p>
      )}
      <button className="btn small" style={{ marginTop: "auto", alignSelf: "flex-start" }} onClick={() => open({ name: "move" })}>
        Move sats
      </button>
    </>
  );
}
