import { useEffect, useMemo, useRef, useState } from "react";
import { api } from "../api";
import type { AccountInfo, Execution, FeePriority, Leg, PaymentRequest, Plan, Prepared, SpendStyle } from "../api/types";
import { Icon } from "../components/Icon";
import { ScanButton, scanText } from "../components/Scanner";
import { AmountInput, FeePicker, HoldButton, QuoteNotes, Sheet, SpendStyleSwitch, feeEta, useBusy, useFeeRates } from "../components/ui";
import { Dropdown, accountOptions } from "../components/Dropdown";
import { CoinList } from "../components/Coins";
import { METHOD_LABEL, errorText, formatFiat, formatSats, held } from "../format";
import { useWallet } from "../state/store";
import { RoutePreview } from "./RoutePreview";
import { activeAccounts } from "../modules/AccountModule";
import { SignStep } from "./SignStep";

type Step = "input" | "amount" | "review" | "sign" | "done";

function describe(r: PaymentRequest): string {
  switch (r.type) {
    case "unified":
      return `Bitcoin or Lightning`;
    case "bitcoinAddress":
      return `Bitcoin address ${r.address.slice(0, 8)}…${r.address.slice(-6)}`;
    case "lightningAddress":
      return r.address;
    case "liquidAddress":
      return `Liquid address ${r.address.slice(0, 8)}…${r.address.slice(-6)}`;
    default:
      return METHOD_LABEL[r.type];
  }
}

/** First step is an on-chain send from an on-chain account? Then privacy style applies. */
export function onchainAccountLeg(plan: Plan, accounts: AccountInfo[]): boolean {
  const first = plan.legs[0];
  return onchainLeg(first) && accounts.find((a) => a.id === first.from)?.rail === "onchain" && first.type !== "viaSwap";
}

/** Leg on-chain? Then fee choice matters. */
export function onchainLeg(l: Leg): boolean {
  return (
    (l.type === "toExternal" && l.request.type === "bitcoinAddress") ||
    (l.type === "toAccount" && l.method === "bitcoinAddress") ||
    (l.type === "viaSwap" && l.payMethod === "bitcoinAddress")
  );
}

export function PaySheet({ prefill, from: initialFrom }: { prefill?: string; from?: string }) {
  const { accounts, close, notify, refresh, price, currency } = useWallet();
  const [step, setStep] = useState<Step>("input");
  const [input, setInput] = useState(prefill ?? "");
  const [request, setRequest] = useState<PaymentRequest | null>(null);
  const [fixedAmount, setFixedAmount] = useState<number | null>(null);
  const [parseError, setParseError] = useState<string | null>(null);
  const [amount, setAmount] = useState<number | null>(null);
  const [from, setFrom] = useState<string | null>(initialFrom ?? null);
  const [fee, setFee] = useState<FeePriority>("normal");
  const advanced = useWallet((s) => s.layout?.advanced ?? false);
  // Basic mode is always Efficient; advanced starts from the user's default.
  const [style, setStyle] = useState<SpendStyle>(advanced ? (useWallet.getState().layout?.spendStyle ?? "efficient") : "efficient");
  /** Why Private can't be used for this payment, once we've found out. */
  const [privateBlocked, setPrivateBlocked] = useState<string | null>(null);
  /** "All": fees come out of the amount, which is tuned once fees are known. */
  const [all, setAll] = useState(false);
  const allRef = useRef(false);
  allRef.current = all;
  const tunes = useRef(0);
  const rates = useFeeRates();
  // Coin control and labels (Advanced, on-chain accounts).
  const [coinMode, setCoinMode] = useState<"auto" | "manual">("auto");
  const [coins, setCoins] = useState<Map<string, number>>(new Map());
  const [label, setLabel] = useState("");
  const [labelSent, setLabelSent] = useState("");
  const coinSum = [...coins.values()].reduce((x, y) => x + y, 0);
  const manual = coinMode === "manual" && coins.size > 0;
  /** What "All" can spend: the chosen coins, or the whole account. */
  const pool = (payer: AccountInfo | undefined) => (manual ? coinSum : payer ? held(payer) : 0);
  const [plan, setPlan] = useState<Plan | null>(null);
  const [prepared, setPrepared] = useState<Prepared | null>(null);
  /** "All" from an on-chain account: the wallet sweeps it exactly, no tuning needed. */
  const payerRail = accounts.find((x) => x.id === from)?.rail;
  const drain = all && payerRail === "onchain" && style !== "private";
  const [planError, setPlanError] = useState<string | null>(null);
  const [paused, setPaused] = useState<Execution | null>(null);
  const [busy, run] = useBusy();

  // Parse as the user types/pastes.
  useEffect(() => {
    if (!input.trim()) {
      setRequest(null);
      setParseError(null);
      return;
    }
    let alive = true;
    api()
      .then((a) => a.parse(input))
      .then((r) => alive && (setRequest(r.request), setFixedAmount(r.amountSat), setParseError(null)))
      .catch((e) => alive && (setRequest(null), setFixedAmount(null), setParseError(errorText(e))));
    return () => {
      alive = false;
    };
  }, [input]);

  // Re-plan and re-price whenever the inputs on the review screen change.
  useEffect(() => {
    if (step !== "review" || !amount) return;
    let alive = true;
    setPrepared(null);
    setPlanError(null);
    (async () => {
      try {
        const a = await api();
        const p = await a.planPayment(input, amount, from);
        if (!alive) return;
        setPlan(p);
        const spend = labelSent || manual || drain ? { label: labelSent || null, coins: manual ? [...coins.keys()] : null, sendMax: drain } : undefined;
        const prep = await a.prepare(p, fee, style, spend);
        if (!alive) return;
        setPrepared(prep);
        // Show what actually arrives once the fee is taken out.
        if (drain && prep.amountSat !== amount) setAmount(prep.amountSat);
      } catch (e) {
        if (!alive) return;
        if (style === "private") {
          // Private isn't possible here: fall back to Efficient and say why on tap.
          setPrivateBlocked(errorText(e));
          setStyle("efficient");
          return;
        }
        const payer = accounts.find((x) => x.id === from);
        if (allRef.current && !drain && payer && tunes.current < 5) {
          tunes.current += 1;
          const need = /need ([\d,]+)/i.exec(errorText(e))?.[1];
          const over = need && !manual ? Number(need.replace(/,/g, "")) - pool(payer) : Math.ceil(amount * 0.01);
          setAmount(Math.max(1, amount - Math.max(1, over)));
          return;
        }
        setPlanError(errorText(e));
      }
    })();
    return () => {
      alive = false;
    };
  }, [step, input, amount, from, fee, style, coins, coinMode, labelSent]); // eslint-disable-line react-hooks/exhaustive-deps

  // Sending everything: shrink the amount until amount + fees is exactly the balance.
  useEffect(() => {
    const payer = accounts.find((x) => x.id === from);
    if (!all || drain || !prepared || !payer || !amount) return;
    const over = prepared.quote.amountSat + prepared.quote.feeSat - pool(payer);
    if ((over > 0 || over < -3) && tunes.current < 5) {
      tunes.current += 1;
      setAmount(Math.max(1, amount - over));
    }
  }, [all, prepared, from, amount]); // eslint-disable-line react-hooks/exhaustive-deps

  // A new speed or account re-fits "All".
  useEffect(() => {
    tunes.current = 0;
  }, [fee, from, style, coins, coinMode]);

  // Coins belong to one account.
  useEffect(() => {
    setCoins(new Map());
    setCoinMode("auto");
  }, [from]);

  // "All" with chosen coins sends exactly those coins.
  useEffect(() => {
    if (all && manual) {
      tunes.current = 0;
      setAmount(coinSum);
    }
  }, [all, manual, coinSum]);

  // Privacy only matters per account.
  useEffect(() => {
    setPrivateBlocked(null);
  }, [from]);

  const layout = useWallet((s) => s.layout);
  const payers = useMemo(() => activeAccounts(accounts, layout).filter((a) => a.pays.length > 0 || a.id === initialFrom), [accounts, layout, initialFrom]);
  const isToken = request?.type === "cashuToken";

  async function paste() {
    try {
      setInput((await navigator.clipboard.readText()).trim());
    } catch {
      notify("error", "Clipboard access was blocked. Paste into the box instead.");
    }
  }

  function next() {
    if (!request) return;
    if (fixedAmount) {
      setAmount(fixedAmount);
      setStep("review");
    } else {
      setStep("amount");
    }
  }

  const send = () =>
    run(async () => {
      if (!prepared) return;
      const ex = await (await api()).confirm(prepared, fee);
      await refresh();
      if (ex.awaitingSignature) {
        setPaused(ex);
        setStep("sign");
      } else {
        setStep("done");
      }
    });

  const redeem = () =>
    run(async () => {
      const p = await (await api()).redeem(input, null);
      await refresh();
      notify("ok", `Received ${formatSats(p.amountSat)} sats`);
      close();
    });

  if (step === "done") {
    return (
      <Sheet title="Sent">
        <div className="center-screen" style={{ minHeight: 0, padding: "32px 0", textAlign: "center" }}>
          <div style={{ color: "var(--green)", margin: "0 auto 12px" }}>
            <Icon name="check" size={56} stroke={2.5} />
          </div>
          <div className="big-stat num">{formatSats(amount ?? 0)} sats</div>
          <p className="balance-sub">{request ? describe(request) : ""}</p>
        </div>
        <button className="btn primary block" onClick={close}>
          Done
        </button>
      </Sheet>
    );
  }

  if (step === "sign" && paused) {
    return <SignStep paused={paused} fee={fee} onDone={() => setStep("done")} />;
  }

  if (step === "amount") {
    const payer = accounts.find((a) => a.id === from);
    return (
      <Sheet
        title="Amount"
        left={
          <button className="text-button" onClick={() => setStep("input")}>
            Back
          </button>
        }
        right={
          <button className="text-button strong" disabled={!amount} onClick={() => setStep("review")}>
            Next
          </button>
        }
      >
        <AmountInput value={amount || null} onChange={(v) => (setAll(false), setAmount(v))} autoFocus max={payer ? held(payer) : undefined} />
        <div className="quick-amounts">
          {[1_000, 10_000, 100_000].map((v) => (
            <button key={v} className="chip num" onClick={() => (setAll(false), setAmount(v))}>
              {formatSats(v)}
            </button>
          ))}
          <button
            className="chip"
            aria-pressed={all}
            onClick={() => {
              // "All" needs a source: the chosen account, else the one holding the most.
              const src = payer ?? [...payers].sort((x, y) => held(y) - held(x))[0];
              if (!src) return;
              tunes.current = 0;
              setFrom(src.id);
              setAll(true);
              setAmount(held(src));
            }}
          >
            All
          </button>
        </div>
        {all && payer && <p className="group-note" style={{ textAlign: "center", margin: "-6px 0 16px" }}>Everything in {payer.name}. Fees are taken from this amount.</p>}
        <button className="btn primary block" disabled={!amount} onClick={() => setStep("review")}>
          Review
        </button>
      </Sheet>
    );
  }

  if (step === "review" && request) {
    const showFees = plan ? plan.legs.some(onchainLeg) : false;
    const total = prepared ? prepared.quote.amountSat + prepared.quote.feeSat : null;
    return (
      <Sheet
        title="Review"
        left={
          <button className="text-button" onClick={() => setStep(fixedAmount ? "input" : "amount")}>
            Back
          </button>
        }
        right={
          <button className="text-button" onClick={close}>
            Cancel
          </button>
        }
      >
        <div className="amount-entry" style={{ paddingTop: 4 }}>
          <div className="big-stat num" style={{ fontSize: 44 }}>
            {formatSats(amount ?? 0)} <span className="balance-unit">sats</span>
          </div>
          <div className="balance-sub">to {describe(request)}</div>
        </div>

        <div className="group">
          <Dropdown
            label="Send from"
            value={from ?? "auto"}
            onChange={(v) => {
              tunes.current = 0;
              if (all && v !== "auto") {
                const src = accounts.find((a) => a.id === v);
                if (src) setAmount(held(src));
              }
              if (v === "auto") setAll(false);
              setFrom(v === "auto" ? null : v);
            }}
            options={[{ value: "auto", label: "Best account", detail: "Cheapest way to pay", color: "var(--label-2)", icon: <Icon name="bolt" size={14} /> }, ...accountOptions(payers)]}
          />
        </div>
        {all && <p className="group-note" style={{ margin: "-12px 16px 16px" }}>Sending everything. Fees are taken from the amount.</p>}

        {planError ? (
          <p className="notice">{planError}</p>
        ) : plan ? (
          <RoutePreview plan={plan} />
        ) : (
          <p className="empty">Finding the best route…</p>
        )}

        {showFees && <FeePicker value={fee} onChange={setFee} rates={rates} />}

        {advanced && plan && onchainAccountLeg(plan, accounts) && <SpendStyleSwitch value={style} onChange={setStyle} unavailable={privateBlocked} />}

        {advanced && from && plan && onchainAccountLeg(plan, accounts) && (
          <>
            <p className="group-label">Coins</p>
            <div className="segmented" style={{ marginBottom: 8 }}>
              <button aria-pressed={coinMode === "auto"} onClick={() => setCoinMode("auto")}>
                Automatic
              </button>
              <button aria-pressed={coinMode === "manual"} onClick={() => setCoinMode("manual")}>
                {manual ? `${coins.size} chosen · ${formatSats(coinSum)}` : "Choose coins"}
              </button>
            </div>
            {coinMode === "manual" && (
              <CoinList
                account={from}
                selected={new Set(coins.keys())}
                onToggle={(u) =>
                  setCoins((m) => {
                    const n = new Map(m);
                    if (n.has(u.outpoint)) n.delete(u.outpoint);
                    else n.set(u.outpoint, u.amountSat);
                    return n;
                  })
                }
              />
            )}
          </>
        )}

        {advanced && from && plan && plan.legs[0] && accounts.find((a) => a.id === plan.legs[0].from)?.rail === "onchain" && (
          <div className="group">
            <label className="cell">
              <span className="grow">Label</span>
              <input
                value={label}
                placeholder="Only you see this"
                maxLength={80}
                style={{ textAlign: "right" }}
                onChange={(e) => setLabel(e.target.value)}
                onBlur={() => setLabelSent(label.trim())}
              />
            </label>
          </div>
        )}

        {prepared && <QuoteNotes notes={prepared.quote.notes} payjoin={prepared.quote.payjoin} />}

        {prepared && (
          <div className="group">
            <div className="cell">
              <span className="grow">Fee</span>
              <span className="value num">
                {formatSats(prepared.quote.feeSat)} sats{showFees && feeEta(fee, rates) ? ` · ${feeEta(fee, rates)}` : ""}
              </span>
            </div>
            {prepared.swapFeeSat > 0 && (
              <div className="cell">
                <span className="grow">Swap fee</span>
                <span className="value num">{formatSats(prepared.swapFeeSat)} sats</span>
              </div>
            )}
            {prepared.hopsAfter > 0 && (
              <div className="cell">
                <span className="grow">Extra hop</span>
                <span className="value">A few sats more</span>
              </div>
            )}
            <div className="cell">
              <span className="grow" style={{ fontWeight: 600 }}>
                Total
              </span>
              <span className="num" style={{ fontWeight: 600, textAlign: "right" }}>
                {formatSats(total!)} sats
                <div className="row-sub">{formatFiat(total!, price, currency)}</div>
              </span>
            </div>
          </div>
        )}

        <HoldButton disabled={!prepared || busy} onConfirm={send}>
          {busy ? "Sending…" : prepared?.quote.needsSignature ? "Hold to prepare for signing" : "Hold to send"}
        </HoldButton>
      </Sheet>
    );
  }

  // Step: input
  return (
    <Sheet title="Send">
      <p className="group-label">Send to</p>
      <div className="group">
        <label className="cell">
          <span className="visually-hidden">Address, invoice or Lightning address</span>
          <textarea
            autoFocus
            value={input}
            onChange={(e) => setInput(e.target.value)}
            placeholder="Bitcoin or Liquid address, Lightning invoice, Lightning address, or ecash token"
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
          />
        </label>
      </div>
      {parseError && input.trim().length > 6 && <p className="field-error">{parseError}</p>}
      {request && !isToken && (
        <p className="group-note">
          <Icon name="check" size={14} /> {describe(request)}
        </p>
      )}

      <div style={{ display: "flex", gap: 10, marginBottom: 22 }}>
        <button className="btn small" onClick={paste}>
          <Icon name="paste" size={16} />
          Paste
        </button>
        <ScanButton
          hint="Scan a bitcoin address, Lightning invoice or ecash token."
          onResult={(r) =>
            scanText(r)
              .then(setInput)
              .catch(() => notify("error", "That QR code isn't a payment. Try a bitcoin address or Lightning invoice."))
          }
        />
      </div>

      {isToken ? (
        <>
          <p className="notice">This is an ecash token. Receiving it moves the sats into your ecash account for that mint.</p>
          <button className="btn primary block" disabled={busy} onClick={redeem}>
            {busy ? "Receiving…" : "Receive ecash"}
          </button>
        </>
      ) : (
        <button className="btn primary block" disabled={!request} onClick={next}>
          Next
        </button>
      )}
    </Sheet>
  );
}
