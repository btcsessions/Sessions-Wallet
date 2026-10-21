import { useEffect, useMemo, useState } from "react";
import { api } from "../api";
import type { AccountInfo, Execution, FeePriority, Leg, PaymentRequest, Plan, Prepared, SpendStyle } from "../api/types";
import { Icon } from "../components/Icon";
import { ScanButton, scanText } from "../components/Scanner";
import { AmountInput, HoldButton, QuoteNotes, Sheet, SpendStyleSwitch, useBusy } from "../components/ui";
import { METHOD_LABEL, accountColor, errorText, formatFiat, formatSats } from "../format";
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
  const [style, setStyle] = useState<SpendStyle>(useWallet.getState().layout?.spendStyle ?? "efficient");
  const [plan, setPlan] = useState<Plan | null>(null);
  const [prepared, setPrepared] = useState<Prepared | null>(null);
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
        const prep = await a.prepare(p, fee, style);
        if (alive) setPrepared(prep);
      } catch (e) {
        if (alive) setPlanError(errorText(e));
      }
    })();
    return () => {
      alive = false;
    };
  }, [step, input, amount, from, fee, style]);

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
        <AmountInput value={amount || null} onChange={setAmount} autoFocus max={payer?.balance.confirmed} />
        <div className="quick-amounts">
          {[1_000, 10_000, 21_000, 100_000].map((v) => (
            <button key={v} className="chip num" onClick={() => setAmount(v)}>
              {formatSats(v)}
            </button>
          ))}
        </div>
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

        <p className="group-label">Send from</p>
        <div className="chips">
          <button className="chip" aria-pressed={from === null} onClick={() => setFrom(null)}>
            Best account
          </button>
          {payers.map((a) => (
            <button key={a.id} className="chip" aria-pressed={from === a.id} onClick={() => setFrom(a.id)}>
              <span style={{ display: "inline-block", width: 8, height: 8, borderRadius: 2, background: accountColor(a), marginRight: 6 }} />
              {a.name}
            </button>
          ))}
        </div>

        {planError ? (
          <p className="notice">{planError}</p>
        ) : plan ? (
          <RoutePreview plan={plan} />
        ) : (
          <p className="empty">Finding the best route…</p>
        )}

        {showFees && (
          <>
            <p className="group-label">Speed</p>
            <div className="segmented">
              {(["slow", "normal", "fast"] as const).map((f) => (
                <button key={f} aria-pressed={fee === f} onClick={() => setFee(f)}>
                  {f === "slow" ? "No rush" : f === "normal" ? "About an hour" : "Next block"}
                </button>
              ))}
            </div>
          </>
        )}

        {plan && onchainAccountLeg(plan, accounts) && <SpendStyleSwitch value={style} onChange={setStyle} />}

        {prepared && <QuoteNotes notes={prepared.quote.notes} payjoin={prepared.quote.payjoin} />}

        {prepared && (
          <div className="group">
            <div className="cell">
              <span className="grow">Fee</span>
              <span className="value num">{formatSats(prepared.quote.feeSat)} sats</span>
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
