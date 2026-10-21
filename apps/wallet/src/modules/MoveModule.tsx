import { useEffect, useState } from "react";
import { Icon } from "../components/Icon";
import { SelectRow } from "../components/ui";
import { accountColor, formatSats } from "../format";
import { useWallet } from "../state/store";
import type { ModuleProps } from "./catalog";

export function MoveModule({ module }: ModuleProps) {
  const { accounts, open } = useWallet();
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
      Move
    </div>
  );

  if (accounts.length < 2) {
    return (
      <>
        {title}
        <p className="tile-foot" style={{ marginTop: 0 }}>Add a second account, like a hardware wallet or ecash, to move sats between them.</p>
      </>
    );
  }

  if (module.size === "small") {
    return (
      <>
        {title}
        <p className="tile-foot" style={{ marginTop: 0 }}>Between your accounts.</p>
        <button className="btn small" style={{ marginTop: "auto", alignSelf: "flex-start" }} onClick={() => open({ name: "move" })}>
          Move sats
        </button>
      </>
    );
  }

  const opt = (a: (typeof accounts)[number]) => ({ value: a.id, label: `${a.name} (${formatSats(a.balance.confirmed)})` });
  const fromAcct = accounts.find((a) => a.id === from);
  const toAcct = accounts.find((a) => a.id === to);

  return (
    <div className="move-tile" style={{ display: "contents" }}>
      {title}
      <div className="group">
        <SelectRow label="From" value={from} onChange={setFrom} options={sources.map(opt)} dot={fromAcct && accountColor(fromAcct)} />
        <SelectRow label="To" value={to} onChange={setTo} options={targets.filter((a) => a.id !== from).map(opt)} dot={toAcct && accountColor(toAcct)} />
      </div>
      <button className="btn small primary" style={{ alignSelf: "flex-start" }} disabled={!from || !to} onClick={() => open({ name: "move", from, to })}>
        Move sats
      </button>
    </div>
  );
}
