import { Icon } from "../components/Icon";
import { useWallet } from "../state/store";
import type { ModuleProps } from "./catalog";

export function PayReceiveModule({ module }: ModuleProps) {
  const { open, accounts } = useWallet();
  const small = module.size === "small";
  const canMove = accounts.length > 1;
  return (
    <div className={canMove && !small ? "three-actions" : "pay-receive"} style={small ? { gridTemplateColumns: "1fr" } : undefined}>
      <button className="btn primary" onClick={() => open({ name: "pay" })}>
        <Icon name="up" size={22} />
        Send
      </button>
      <button className="btn" onClick={() => open({ name: "receive" })}>
        <Icon name="down" size={22} />
        Receive
      </button>
      {canMove && !small && (
        <button className="btn" onClick={() => open({ name: "move" })}>
          <Icon name="swap" size={22} />
          Move
        </button>
      )}
    </div>
  );
}
