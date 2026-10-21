import { useEffect, useState } from "react";
import { api } from "../api";
import type { Execution, FeePriority, Plan, Prepared } from "../api/types";
import { Icon } from "../components/Icon";
import { AmountInput, HoldButton, SelectRow, Sheet, useBusy } from "../components/ui";
import { accountColor, errorText, formatSats } from "../format";
import { useWallet } from "../state/store";
import { RoutePreview } from "./RoutePreview";
import { SignStep } from "./SignStep";

export function MoveSheet({ from: initialFrom, to: initialTo }: { from?: string; to?: string }) {
  const { accounts, refresh, notify, close } = useWallet();
  const [paused, setPaused] = useState<Execution | null>(null);
  const sources = accounts.filter((a) => a.pays.length > 0);
  const [from, setFrom] = useState(initialFrom ?? sources.find((a) => a.balance.confirmed > 0)?.id ?? sources[0]?.id);
  const [to, setTo] = useState(initialTo ?? accounts.find((a) => a.id !== from && a.custody.type !== "watchOnly")?.id);
  const [amount, setAmount] = useState<number | null>(null);
  const [fee, setFee] = useState<FeePriority>("normal");
  const [plan, setPlan] = useState<Plan | null>(null);
  const [prepared, setPrepared] = useState<Prepared | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, run] = useBusy();
  const source = accounts.find((a) => a.id === from);

  useEffect(() => {
    setPlan(null);
    setPrepared(null);
    setError(null);
    if (!from || !to || !amount) return;
    let alive = true;
    const t = setTimeout(async () => {
      try {
        const a = await api();
        const p = await a.planMove(from, to, amount);
        if (!alive) return;
        setPlan(p);
        const prep = await a.prepare(p, fee);
        if (alive) setPrepared(prep);
      } catch (e) {
        if (alive) setError(errorText(e));
      }
    }, 350);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [from, to, amount, fee]);

  const swap = () => {
    if (!to) return;
    const target = accounts.find((a) => a.id === to);
    if (!target?.pays.length) return;
    setFrom(to);
    setTo(from);
  };

  const onchain = plan?.legs.some((l) => l.type === "toAccount" && l.method === "bitcoinAddress");

  const confirm = () =>
    run(async () => {
      if (!prepared) return;
      const ex: Execution = await (await api()).confirm(prepared, fee);
      await refresh();
      if (ex.awaitingSignature) {
        setPaused(ex);
        return;
      }
      notify("ok", `Moved ${formatSats(amount ?? 0)} sats`);
      close();
    });

  if (paused) {
    return (
      <SignStep
        paused={paused}
        fee={fee}
        onDone={() => {
          notify("ok", `Moved ${formatSats(amount ?? 0)} sats`);
          close();
        }}
      />
    );
  }

  const opt = (a: (typeof accounts)[number]) => ({ value: a.id, label: `${a.name} (${formatSats(a.balance.confirmed)})` });
  const target = accounts.find((a) => a.id === to);

  return (
    <Sheet title="Move">
      <AmountInput value={amount} onChange={setAmount} max={source?.balance.confirmed} />
      {source && (
        <div className="quick-amounts">
          {[0.25, 0.5, 1].map((f) => (
            <button key={f} className="chip" onClick={() => setAmount(Math.floor(source.balance.confirmed * f * (f === 1 ? 0.99 : 1)))}>
              {f === 1 ? "Almost all" : `${f * 100}%`}
            </button>
          ))}
        </div>
      )}

      <div className="group" style={{ position: "relative" }}>
        <SelectRow label="From" value={from} onChange={(id) => (id === to ? swap() : setFrom(id))} options={sources.map(opt)} dot={source && accountColor(source)} />
        <SelectRow
          label="To"
          value={to}
          onChange={(id) => (id === from ? swap() : setTo(id))}
          options={accounts.filter((a) => a.custody.type !== "watchOnly" && a.id !== from).map(opt)}
          dot={target && accountColor(target)}
        />
      </div>
      <div style={{ display: "flex", justifyContent: "center", margin: "-12px 0 12px" }}>
        <button className="btn small" onClick={swap} aria-label="Swap from and to">
          <Icon name="swap" size={16} />
          Swap
        </button>
      </div>

      {error && <p className="notice">{error}</p>}
      {plan && <RoutePreview plan={plan} />}

      {onchain && (
        <div className="segmented">
          {(["slow", "normal", "fast"] as const).map((f) => (
            <button key={f} aria-pressed={fee === f} onClick={() => setFee(f)}>
              {f === "slow" ? "No rush" : f === "normal" ? "About an hour" : "Next block"}
            </button>
          ))}
        </div>
      )}

      {prepared && (
        <div className="group">
          <div className="cell">
            <span className="grow">Fee</span>
            <span className="value num">
              {formatSats(prepared.quote.feeSat + prepared.swapFeeSat)} sats{prepared.hopsAfter > 0 ? " plus a few for the extra hop" : ""}
            </span>
          </div>
        </div>
      )}

      <HoldButton disabled={!prepared || busy} onConfirm={confirm}>
        {busy ? "Moving…" : "Hold to move"}
      </HoldButton>
    </Sheet>
  );
}
