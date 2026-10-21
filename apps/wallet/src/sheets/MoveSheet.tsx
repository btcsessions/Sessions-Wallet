import { useEffect, useRef, useState } from "react";
import { api } from "../api";
import type { Execution, FeePriority, Plan, Prepared, SpendStyle } from "../api/types";
import { Icon } from "../components/Icon";
import { AmountInput, HoldButton, QuoteNotes, Sheet, SpendStyleSwitch, useBusy } from "../components/ui";
import { AccountPicker } from "../components/Dropdown";
import { errorText, formatSats } from "../format";
import { useWallet } from "../state/store";
import { onchainAccountLeg } from "./PaySheet";
import { activeAccounts } from "../modules/AccountModule";
import { RoutePreview } from "./RoutePreview";
import { SignStep } from "./SignStep";

export function MoveSheet({ from: initialFrom, to: initialTo }: { from?: string; to?: string }) {
  const { accounts: everyAccount, layout, refresh, notify, close } = useWallet();
  const accounts = activeAccounts(everyAccount, layout);
  const [paused, setPaused] = useState<Execution | null>(null);
  const sources = accounts.filter((a) => a.pays.length > 0);
  const [from, setFrom] = useState(initialFrom ?? sources.find((a) => a.balance.confirmed > 0)?.id ?? sources[0]?.id);
  const [to, setTo] = useState(initialTo ?? accounts.find((a) => a.id !== from && a.custody.type !== "watchOnly")?.id);
  const [amount, setAmount] = useState<number | null>(null);
  /** "All": fees come out of the amount, so it's tuned once fees are known. */
  const [all, setAll] = useState(false);
  const tunes = useRef(0);
  const allRef = useRef(false);
  allRef.current = all;
  const [fee, setFee] = useState<FeePriority>("normal");
  const [style, setStyle] = useState<SpendStyle>(useWallet.getState().layout?.spendStyle ?? "efficient");
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
        const prep = await a.prepare(p, fee, style);
        if (alive) setPrepared(prep);
      } catch (e) {
        if (!alive) return;
        // Sending everything and it doesn't fit: take the shortfall (or 1%) off and retry.
        const balance = accounts.find((x) => x.id === from)?.balance.confirmed ?? 0;
        if (allRef.current && tunes.current < 5) {
          tunes.current += 1;
          const need = /need ([\d,]+)/i.exec(errorText(e))?.[1];
          const over = need ? Number(need.replace(/,/g, "")) - balance : Math.ceil(amount * 0.01);
          setAmount(Math.max(1, amount - Math.max(1, over)));
          return;
        }
        setError(errorText(e));
      }
    }, 350);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [from, to, amount, fee, style]);

  // Sending everything: shrink the amount until amount + fees fits the balance.
  useEffect(() => {
    if (!all || !prepared || !source || !amount) return;
    const spend = prepared.quote.amountSat + prepared.quote.feeSat;
    const over = spend - source.balance.confirmed;
    if ((over > 0 || over < -3) && tunes.current < 4) {
      tunes.current += 1;
      setAmount(Math.max(1, amount - over));
    }
  }, [all, prepared, source, amount]);

  // New fee speed, route or style: let "All" re-fit the amount.
  useEffect(() => {
    tunes.current = 0;
  }, [fee, to, from, style]);

  const pickAmount = (v: number | null, everything = false) => {
    tunes.current = 0;
    setAll(everything);
    setAmount(v);
  };

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
      notify("ok", `Transferred ${formatSats(amount ?? 0)} sats`);
      close();
    });

  if (paused) {
    return (
      <SignStep
        paused={paused}
        fee={fee}
        onDone={() => {
          notify("ok", `Transferred ${formatSats(amount ?? 0)} sats`);
          close();
        }}
      />
    );
  }


  return (
    <Sheet title="Transfer">
      <AmountInput value={amount} onChange={(v) => pickAmount(v)} max={source?.balance.confirmed} />
      {source && (
        <div className="quick-amounts">
          {[0.25, 0.5].map((f) => (
            <button key={f} className="chip" aria-pressed={false} onClick={() => pickAmount(Math.floor(source.balance.confirmed * f))}>
              {`${f * 100}%`}
            </button>
          ))}
          <button className="chip" aria-pressed={all} onClick={() => pickAmount(source.balance.confirmed, true)}>
            All
          </button>
        </div>
      )}
      {all && <p className="group-note" style={{ textAlign: "center", margin: "-6px 0 16px" }}>Fees are taken from this amount, so the whole balance moves.</p>}

      <div className="group" style={{ position: "relative" }}>
        <AccountPicker label="From" accounts={sources} value={from} onChange={(id) => (id === to ? swap() : (setFrom(id), all && pickAmount(accounts.find((a) => a.id === id)?.balance.confirmed ?? null, true)))} />
        <AccountPicker label="To" accounts={accounts.filter((a) => a.custody.type !== "watchOnly" && a.id !== from)} value={to} onChange={(id) => (id === from ? swap() : setTo(id))} />
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

      {plan && onchainAccountLeg(plan, accounts) && <SpendStyleSwitch value={style} onChange={setStyle} />}
      {prepared && <QuoteNotes notes={prepared.quote.notes} />}

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
        {busy ? "Transferring…" : "Hold to transfer"}
      </HoldButton>
    </Sheet>
  );
}
