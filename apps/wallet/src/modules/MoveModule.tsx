import { useEffect, useState } from "react";
import { Icon } from "../components/Icon";
import { AccountPicker } from "../components/Dropdown";
import { useWallet } from "../state/store";
import type { ModuleProps } from "./catalog";
import { activeAccounts } from "./AccountModule";

export function MoveModule({ module }: ModuleProps) {
  const { accounts: all, layout, open } = useWallet();
  const accounts = activeAccounts(all, layout);
  const sources = accounts.filter((a) => a.pays.length > 0);
  const targets = accounts.filter((a) => a.custody.type !== "watchOnly");
  const [from, setFrom] = useState<string | undefined>();
  const [to, setTo] = useState<string | undefined>();

  // Sensible defaults once accounts load: richest spendable account to the next one.
  useEffect(() => {
    if (!from && sources.length) setFrom([...sources].sort((a, b) => b.balance.confirmed - a.balance.confirmed)[0].id);
  }, [sources, from]);
  useEffect(() => {
    if ((!to || to === from) && targets.length) setTo(targets.find((a) => a.id !== from)?.id);
  }, [targets, to, from]);

  const title = (
    <div className="tile-title" style={{ ["--tint" as string]: "var(--blue)" }}>
      <Icon name="swap" size={16} />
      Transfer
    </div>
  );

  if (accounts.length < 2) {
    return (
      <>
        {title}
        <p className="tile-foot" style={{ marginTop: 0 }}>Add a second account, like a hardware wallet or ecash, to transfer sats between them.</p>
      </>
    );
  }

  if (module.size === "small") {
    return (
      <>
        {title}
        <p className="tile-foot" style={{ marginTop: 0 }}>Between your accounts.</p>
        <button className="btn small" style={{ marginTop: "auto", alignSelf: "flex-start" }} onClick={() => open({ name: "move" })}>
          Transfer
        </button>
      </>
    );
  }


  return (
    <div className="move-tile" style={{ display: "contents" }}>
      {title}
      <div className="group">
        <AccountPicker label="From" accounts={sources} value={from} onChange={setFrom} />
        <AccountPicker label="To" accounts={targets.filter((a) => a.id !== from)} value={to} onChange={setTo} />
      </div>
      <button className="btn small primary" style={{ alignSelf: "flex-start" }} disabled={!from || !to} onClick={() => open({ name: "move", from, to })}>
        Transfer
      </button>
    </div>
  );
}
