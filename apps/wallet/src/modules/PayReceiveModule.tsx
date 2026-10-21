import { Icon } from "../components/Icon";
import { useWallet } from "../state/store";
import type { ModuleProps } from "./catalog";

export function PayReceiveModule({ module }: ModuleProps) {
  const open = useWallet((s) => s.open);
  const small = module.size === "small";
  return (
    <div className="pay-receive" style={small ? { gridTemplateColumns: "1fr" } : undefined}>
      <button className="btn primary" onClick={() => open({ name: "pay" })}>
        <Icon name="up" size={22} />
        Pay
      </button>
      <button className="btn" onClick={() => open({ name: "receive" })}>
        <Icon name="down" size={22} />
        Receive
      </button>
    </div>
  );
}
