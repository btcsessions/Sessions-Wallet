import { useEffect, useMemo, useState } from "react";
import { api } from "../api";
import type { AccountInfo, Arrival, LightningAddress, Method, ReceiveQuote, ReceiveTarget } from "../api/types";
import { AccountPicker } from "../components/Dropdown";
import { SuccessCheck } from "../components/SuccessCheck";
import { AmountInput, CopyButton, QR, Sheet } from "../components/ui";
import { ScanButton, scanText } from "../components/Scanner";
import { METHOD_LABEL, errorText, formatSats } from "../format";
import { deviceFor } from "../hw/devices";
import { useWallet } from "../state/store";
import { Icon } from "../components/Icon";
import { activeAccounts } from "../modules/AccountModule";
import { MintPicker, SpeedSwitch, usePrivateHub, type Speed } from "../components/PrivateLightning";

/**
 * Methods to offer for an account, best first. Spark addresses only work
 * between Spark wallets, so they aren't offered. Anything the account can't
 * take natively comes through a swap: Liquid into Lightning accounts, and
 * Lightning or Liquid straight into on-chain accounts (hardware included).
 */
function methodsFor(a: AccountInfo, hasSpending: boolean): Method[] {
  const out = (["bolt11", "bitcoinAddress"] as Method[]).filter((m) => a.receives.includes(m));
  if (LIQUID_SWAPS && a.receives.includes("bolt11")) out.push("liquidAddress");
  // Lightning into on-chain arrives in Spending and is sent on automatically.
  if (a.rail === "onchain" && hasSpending) out.push("bolt11");
  return out;
}

/** Boltz, which did the Liquid swaps, suspended its service in August 2026. */
export const LIQUID_SWAPS = false;

/** Received through a swap into this account (so: limits and fees apply). */
const viaSwap = (a: AccountInfo | undefined, m: Method) => !!a && !a.receives.includes(m) && m !== "lightningAddress";

const NEEDS_AMOUNT: Method[] = ["bolt11", "liquidAddress"];

/** Short labels so the options fit on a phone. */
const SHORT: Partial<Record<Method, string>> = {
  lightningAddress: "Address",
  bolt11: "Lightning",
  sparkAddress: "Spark",
  bitcoinAddress: "On-chain",
  liquidAddress: "Liquid",
};

const EXPLAIN: Partial<Record<Method, string>> = {
  bolt11: "Any Lightning wallet can pay this. It expires, so make a fresh one each time.",
  sparkAddress: "Free and instant from other Spark wallets. Reusable.",
  lightningAddress: "Your permanent address. Works with any Lightning wallet, any amount, any time.",
  bitcoinAddress: "Works with every bitcoin wallet. Takes about 10 to 60 minutes to confirm.",
  liquidAddress: "For someone paying from a Liquid wallet. It's swapped into your bitcoin automatically.",
};

export function ReceiveSheet({ account: initial }: { account?: string }) {
  const { accounts, activity, open, notify, layout } = useWallet();
  const verified = useWallet((s) => s.synced.verified);
  const [skipVerify, setSkipVerify] = useState(false);
  const advanced = layout?.advanced ?? false;
  const [openedAt] = useState(() => Math.floor(Date.now() / 1000) - 5);
  const [lnAddress, setLnAddress] = useState<LightningAddress | null>(null);
  const [preferId, preferMethod] = (initial ?? "").split("#");
  const receivable = activeAccounts(accounts, layout).filter((a) => a.receives.length > 0 || a.id === preferId);
  const hasSpending = accounts.some((a) => a.rail === "spark");
  // Default to Spending: everything can arrive there, including on-chain.
  const [accountId, setAccountId] = useState(preferId || receivable.find((a) => a.rail === "spark")?.id || receivable[0]?.id || "");
  const account = accounts.find((a) => a.id === accountId);
  const isSpark = account?.rail === "spark";
  const methods = useMemo(() => {
    const m = account ? methodsFor(account, hasSpending) : [];
    return lnAddress && isSpark ? (["lightningAddress", ...m] as Method[]) : m;
  }, [account?.id, lnAddress, isSpark]); // eslint-disable-line react-hooks/exhaustive-deps
  const [method, setMethod] = useState<Method>(preferMethod === "liquid" && LIQUID_SWAPS ? "liquidAddress" : methods[0] ?? "bitcoinAddress");
  const [amount, setAmount] = useState<number | null>(null);
  /** The amount is settled and the request can be made (always true for amount-free methods). */
  const [ready, setReady] = useState(false);
  const [target, setTarget] = useState<ReceiveTarget | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [quote, setQuote] = useState<ReceiveQuote | null>(null);
  const swap = viaSwap(account, method);
  // Private (Advanced): the payer pays the ecash mint; the sats move on to this account.
  const [speed, setSpeed] = useState<Speed>("fast");
  const hub = usePrivateHub();
  const canPrivate = advanced && method === "bolt11" && !!account && account.rail !== "ecash";
  const isPrivate = canPrivate && speed === "private";
  const hubId = isPrivate ? hub?.id : undefined;
  const amountStep = !ready && (NEEDS_AMOUNT.includes(method) || swap);

  // A claimed Lightning address is the simplest thing to share, so lead with it.
  useEffect(() => {
    setLnAddress(null);
    if (!isSpark || !account) return;
    let alive = true;
    api()
      .then((a) => a.lightningAddress(account.id))
      .then((l) => {
        if (alive && l) {
          setLnAddress(l);
          if (preferMethod !== "liquid") setMethod("lightningAddress");
        }
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [account?.id, isSpark]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (account && !methods.includes(method)) setMethod(methods[0]);
  }, [account?.id, methods, method]); // eslint-disable-line react-hooks/exhaustive-deps

  // Switching account or method: methods that need an amount (or a swap) ask first.
  useEffect(() => {
    setReady(!(NEEDS_AMOUNT.includes(method) || viaSwap(account, method)));
  }, [account?.id, method]); // eslint-disable-line react-hooks/exhaustive-deps

  // Swap limits and fees, before anything is created.
  useEffect(() => {
    setQuote(null);
    if (!account || !swap) return;
    let alive = true;
    const t = setTimeout(() => {
      api()
        .then((a) => (hubId ? a.privateReceiveQuote(account.id, amount) : a.receiveQuote(account.id, method, amount)))
        .then((q) => alive && setQuote(q))
        .catch((e) => alive && setError(errorText(e)));
    }, 250);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [account?.id, method, amount, swap, hubId]); // eslint-disable-line react-hooks/exhaustive-deps

  // Watch for the payment landing while this sheet is open.
  // Lightning into an on-chain account lands in Spending first; that's the moment it's received.
  const spendingId = accounts.find((a) => a.rail === "spark")?.id;
  const received = activity.find(
    (p) =>
      p.direction === "incoming" &&
      p.timestamp >= openedAt &&
      (p.account === accountId || (swap && ready && p.account === spendingId && (!amount || p.amountSat === amount)) || (!!hubId && ready && p.account === hubId)),
  );

  // The moment the wallet sees it (pushed from the Rust side: Lightning
  // events, the chain server's address subscriptions), without waiting for a
  // refresh of the activity list.
  const [arrival, setArrival] = useState<Arrival | null>(null);
  useEffect(() => {
    if (!target || !account) return;
    let off: (() => void) | undefined;
    let alive = true;
    void api()
      .then((a) =>
        a.onPayment((e) => {
          const ours = e.account === account.id || (swap && ready && e.account === spendingId && (!amount || e.amountSat === amount)) || (!!hubId && ready && e.account === hubId);
          if (!ours) return;
          setArrival((prev) => (prev && prev.id !== e.id && prev.kind !== "incoming" && prev.kind !== "deposit" ? prev : e));
          void useWallet.getState().refresh();
        }),
      )
      .then((f) => (alive ? (off = f) : f()));
    return () => {
      alive = false;
      off?.();
    };
  }, [target, account?.id, swap, spendingId, ready, amount, hubId]); // eslint-disable-line react-hooks/exhaustive-deps

  // Ask the live connection to watch this address the moment it's on screen.
  useEffect(() => {
    if (!target || !account || account.rail !== "onchain" || target.method !== "bitcoinAddress" || !target.watchId) return;
    void api()
      .then((a) => a.watchAddress(account.id, target.watchId!))
      .catch(() => {});
  }, [target?.watchId, account?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  // While a request is on screen, watch for the payment. An on-chain address
  // is checked every 2 seconds with one cheap request, so a payment shows the
  // moment it reaches the mempool. Everything else (Lightning, swaps) syncs
  // every 4 seconds; Lightning also pushes an event the moment it lands.
  useEffect(() => {
    if (!target || !account || received || arrival) return;
    let alive = true;
    let busy = false;
    let tick = 0;
    const address = target.method === "bitcoinAddress" && !swap && target.watchId ? target.watchId : null;
    const watched = hubId ? [account.id, hubId] : swap && spendingId ? [account.id, spendingId] : [account.id];
    const t = setInterval(async () => {
      if (busy) return;
      busy = true;
      tick += 1;
      try {
        const a = await api();
        let changed = false;
        if (address) {
          changed = await a.checkIncoming(account.id, address);
          // A full sync now and then catches anything the quick check can't.
          if (!changed && tick % 15 === 0) await a.syncAccount(account.id);
        } else if (tick % 2 === 0) {
          await Promise.all(watched.map((id) => a.syncAccount(id)));
          changed = true;
        }
        if (alive && (changed || tick % 15 === 0)) await useWallet.getState().refresh();
      } catch {
        /* the next tick tries again */
      } finally {
        busy = false;
      }
    }, 2000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [target, account?.id, swap, spendingId, received, arrival, hubId]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    setError(null);
    if (!account || !ready) {
      setTarget(null);
      return;
    }
    if (method === "lightningAddress") {
      setTarget(lnAddress ? { method, request: lnAddress.address, amountSat: null } : null);
      return;
    }
    if (isPrivate && !hubId) {
      setTarget(null);
      return;
    }
    let alive = true;
    setTarget(null);
    api()
      .then((a) => (hubId && amount ? a.privateReceive(account.id, hubId, amount, null) : a.receive(account.id, method, amount, null)))
      .then((t) => alive && setTarget(t))
      .catch((e) => alive && setError(errorText(e)));
    return () => {
      alive = false;
    };
    // Keyed on the account id, not the object: refreshes must not replace an invoice on screen.
  }, [account?.id, method, ready, lnAddress, isPrivate, hubId]); // eslint-disable-line react-hooks/exhaustive-deps

  if (arrival || received) {
    const amountSat = arrival?.amountSat ?? received!.amountSat;
    const landedIn = arrival?.account ?? received!.account;
    const kind = arrival?.kind ?? (received!.status === "pending" ? "incoming" : "received");
    const pending = kind === "incoming" || kind === "deposit";
    const where = accounts.find((a) => a.id === landedIn)?.name ?? account?.name;
    const note =
      hubId && landedIn === hubId
        ? `Received privately. Moving it to ${account?.name} now.`
        : landedIn !== account?.id
        ? `Sending on-chain to ${account?.name} now.`
        : kind === "deposit"
          ? `Seen on the network. It's added to ${where} after it confirms (about 30 minutes).`
          : kind === "incoming"
            ? `Seen on the network. Waiting for confirmation, usually 10 to 60 minutes.`
            : `in ${where}`;
    return (
      <Sheet title={pending ? "Payment seen" : "Received"}>
        <div style={{ textAlign: "center", padding: "28px 0 24px" }}>
          <div style={{ marginBottom: 16 }}>
            <SuccessCheck pending={pending} />
          </div>
          <div className="big-stat num received-amount" style={{ fontSize: 44 }}>
            +{formatSats(amountSat)} <span className="balance-unit">sats</span>
          </div>
          <p className="balance-sub received-amount">{note}</p>
        </div>
        <button className="btn primary block" onClick={() => useWallet.getState().close()}>
          Done
        </button>
      </Sheet>
    );
  }

  const tooSmall = !!(quote && amount && amount < quote.minSat);
  const tooBig = !!(quote && amount && amount > quote.maxSat);
  const canContinue = !!amount && (!swap || (!!quote && !tooSmall && !tooBig)) || (!NEEDS_AMOUNT.includes(method) && !swap);

  return (
    <Sheet title="Receive">
      {receivable.length > 1 && (
        <div className="group">
          <AccountPicker label="Receive to" accounts={receivable} value={accountId} onChange={setAccountId} />
        </div>
      )}

      {methods.length > 1 && (
        <div className="segmented" role="radiogroup" aria-label="How they pay">
          {methods.map((m) => (
            <button key={m} role="radio" aria-checked={m === method} aria-pressed={m === method} onClick={() => setMethod(m)} aria-label={METHOD_LABEL[m]}>
              {SHORT[m] ?? METHOD_LABEL[m]}
            </button>
          ))}
        </div>
      )}

      {canPrivate && <SpeedSwitch value={speed} onChange={setSpeed} receive />}

      {isPrivate && !hub ? (
        <MintPicker />
      ) : amountStep ? (
        <>
          <AmountInput value={amount} onChange={setAmount} autoFocus />
          {swap ? (
            <SwapTerms quote={quote} amount={amount} method={method} account={account} tooSmall={tooSmall} tooBig={tooBig} viaMint={hubId ? hub?.name : undefined} />
          ) : (
            <p className="group-note" style={{ margin: "0 0 18px", textAlign: "center" }}>
              {METHOD_LABEL[method]} requests need an amount.
            </p>
          )}
          {error && <p className="notice">{error}</p>}
          <button className="btn primary block" disabled={!canContinue} onClick={() => setReady(true)}>
            Continue
          </button>
        </>
      ) : error ? (
        <p className="notice">{error}</p>
      ) : target && account?.custody.type === "hardware" && method === "bitcoinAddress" && !verified[account.id] && !skipVerify ? (
        // The first deposit to a hardware wallet waits until its device shows the same address.
        <VerifyOnDevice account={account} request={target.request} first onSkip={() => setSkipVerify(true)} />
      ) : target ? (
        <>
          <div className="qr-wrap">
            {/* LNURL in the QR scans in every Lightning wallet; the address shows as text. */}
            <QR value={method === "lightningAddress" && lnAddress ? `lightning:${lnAddress.lnurl.toUpperCase()}` : target.request} />
          </div>
          <p className="request-text">{target.request}</p>
          <div style={{ display: "flex", gap: 10, justifyContent: "center", flexWrap: "wrap", marginBottom: 18 }}>
            <CopyButton text={target.request} />
            {"share" in navigator && (
              <button className="btn small" onClick={() => navigator.share({ text: target.request }).catch(() => {})}>
                Share
              </button>
            )}
            {method !== "lightningAddress" && (
              <button className="btn small" onClick={() => setReady(false)}>
                {amount ? `${formatSats(amount)} sats` : "Add amount"}
              </button>
            )}
          </div>
          {account?.custody.type === "hardware" && method === "bitcoinAddress" && <VerifyOnDevice account={account} request={target.request} />}
          {advanced && account?.rail === "onchain" && method === "bitcoinAddress" && <AddressLabel account={account.id} request={target.request} />}
          <p className="group-note" style={{ margin: "0 0 8px", textAlign: "center" }}>
            {hubId
              ? `Paid to your ecash mint (${hub?.name}), then moved to ${account?.name} automatically, less a few sats of fees. If the wallet is closed, it moves the next time you open it.`
              : swap
              ? `Paid over Lightning into Spending, then sent on-chain to ${account?.name} automatically. If the wallet is closed, it's sent the next time you open it.`
              : method === "bitcoinAddress" && isSpark
                ? "Send from any bitcoin wallet. It lands in Spending after 3 confirmations, ready to spend over Lightning."
                : EXPLAIN[method]}
            {target.feeSat ? ` ${hubId ? "Fees" : "Swap fee"} about ${formatSats(target.feeSat)} sats.` : ""}
          </p>
          {isSpark && !lnAddress && (
            <p style={{ textAlign: "center", margin: "12px 0 0" }}>
              <button className="text-button" onClick={() => open({ name: "account", id: accountId })}>
                Get a Lightning address
              </button>
            </p>
          )}
        </>
      ) : (
        <p className="empty">
          {account?.scanning && method === "bitcoinAddress"
            ? "Checking this wallet's history so you get a fresh, unused address…"
            : "Getting a fresh request…"}
        </p>
      )}
      <p style={{ textAlign: "center", margin: "18px 0 0" }}>
        <ScanButton
          className="text-button with-icon"
          label="Scan an ecash token"
          title="Scan ecash token"
          hint="Scan a Cashu ecash token to claim it."
          onResult={(r) =>
            scanText(r)
              .then((text) => open({ name: "pay", prefill: text }))
              .catch(() => notify("error", "That QR code isn't an ecash token."))
          }
        />
      </p>
    </Sheet>
  );
}

/** Limits and costs of a swap into an on-chain account, shown before continuing. */
function SwapTerms({
  quote,
  amount,
  method,
  account,
  tooSmall,
  tooBig,
  viaMint,
}: {
  viaMint?: string;
  quote: ReceiveQuote | null;
  amount: number | null;
  method: Method;
  account: AccountInfo | undefined;
  tooSmall: boolean;
  tooBig: boolean;
}) {
  if (!quote) return <p className="group-note" style={{ textAlign: "center" }}>Checking swap limits and fees…</p>;
  const how = method === "bolt11" ? "Lightning" : "Liquid";
  return (
    <>
      <div className="group swap-terms">
        <div className="cell">
          <span className="grow">Minimum</span>
          <span className={`value num${tooSmall ? " bad" : ""}`}>{formatSats(quote.minSat)} sats</span>
        </div>
        <div className="cell">
          <span className="grow">Fee</span>
          <span className="value num">
            {quote.feePercent > 0 ? `${quote.feePercent}% + about ` : "About "}
            {formatSats(quote.networkFeeSat)} sats{quote.feePercent > 0 ? "" : " on-chain"}
          </span>
        </div>
        {quote.receiveSat != null && amount != null && !tooSmall && !tooBig && (
          <div className="cell">
            <span className="grow">You receive</span>
            <span className="value num strong">≈ {formatSats(quote.receiveSat)} sats</span>
          </div>
        )}
      </div>
      <p className="group-note" style={{ margin: "-12px 4px 18px" }}>
        {tooSmall
          ? `Ask for at least ${formatSats(quote.minSat)} sats so the ${viaMint ? "mint and on-chain fees are" : "on-chain fee is"} worth paying.`
          : tooBig
            ? `The most this can take at once is ${formatSats(quote.maxSat)} sats.`
            : viaMint
              ? `Paid to ${viaMint}, then through Spending and on-chain to ${account?.name ?? "this account"} automatically. The amount above is what they pay; a few sats of the mint's fee may stay in ${viaMint}.`
              : `${how} payments land in Spending, then go on-chain to ${account?.name ?? "this account"} automatically. The amount above is what they pay.`}
      </p>
    </>
  );
}

/** Advanced: name the address before sharing it; the label follows the coins it receives. */
function AddressLabel({ account, request }: { account: string; request: string }) {
  const notify = useWallet((s) => s.notify);
  const [label, setLabel] = useState("");
  const [saved, setSaved] = useState("");
  const address = request.replace(/^bitcoin:/i, "").split("?")[0];
  const save = async () => {
    const v = label.trim();
    if (v === saved) return;
    try {
      await (await api()).setLabel(account, "address", address, v || null);
      setSaved(v);
      if (v) notify("ok", "Label saved");
    } catch (e) {
      notify("error", errorText(e));
    }
  };
  return (
    <div className="group">
      <label className="cell">
        <span className="grow">Label</span>
        <input value={label} placeholder="Who's paying? Only you see this" maxLength={80} style={{ textAlign: "right" }} onChange={(e) => setLabel(e.target.value)} onBlur={() => void save()} onKeyDown={(e) => e.key === "Enter" && void save()} />
      </label>
    </div>
  );
}

/**
 * Show this receive address on the hardware wallet itself and check it
 * matches, so a swapped key or a compromised phone can't redirect deposits.
 * `first`: nothing has been checked on this device yet, so the address
 * stays hidden until it is (or they choose to skip).
 */
function VerifyOnDevice({ account, request, first = false, onSkip }: { account: AccountInfo; request: string; first?: boolean; onSkip?: () => void }) {
  const { synced, setSynced } = useWallet();
  const device = deviceFor(account.custody.type === "hardware" ? account.custody.device : undefined);
  const address = request.replace(/^bitcoin:/i, "").split("?")[0];
  const [state, setState] = useState<"idle" | "busy" | "ok">("idle");
  const [error, setError] = useState<string | null>(null);
  const live = device.connect !== "qr";
  const [expanded, setExpanded] = useState(false);

  const done = () => {
    setSynced("verified", { ...synced.verified, [account.id]: Math.floor(Date.now() / 1000) });
    setState("ok");
  };

  const verify = async () => {
    setError(null);
    setState("busy");
    try {
      const a = await api();
      const index = await a.hardwareAddressIndex(account.id, address);
      let shown: string;
      if (device.connect === "bitbox") shown = await a.bitboxShowAddress(account.id, index);
      else if (device.connect === "jade") shown = await a.jadeShowAddress(account.id, index);
      else shown = await (await import("../hw/trezor")).trezorShowAddress(account.id, index);
      if (shown.toLowerCase() !== address.toLowerCase()) throw new Error("Your device showed a different address. Don't use this one");
      done();
    } catch (e) {
      setState("idle");
      setError(errorText(e));
    }
  };

  if (!first && !expanded && state !== "ok") {
    return (
      <p style={{ textAlign: "center", margin: "0 0 14px" }}>
        <button className="text-button" onClick={() => (setExpanded(true), live && void verify())}>
          Check this address on your {device.name}
        </button>
      </p>
    );
  }

  if (state === "ok" && !first) {
    return (
      <p className="group-note" style={{ textAlign: "center" }}>
        <Icon name="check" size={14} /> Matches your {device.name}.
      </p>
    );
  }

  const body = (
    <>
      {first && (
        <p className="notice">
          Before the first deposit, check this address on your {device.name}. It proves the address belongs to your device and not to someone who tampered with this phone.
        </p>
      )}
      {first && (
        <p className="request-text" style={{ marginBottom: 12 }}>
          {address}
        </p>
      )}
      {live ? (
        <button className={first ? "btn primary block" : "btn small"} disabled={state === "busy"} onClick={verify}>
          {state === "busy" ? `Confirm on your ${device.name}…` : "Verify on device"}
        </button>
      ) : (
        <>
          <p className="group-note">
            On your {device.name}, use its address check (scan this address, or look it up in the wallet's addresses) and make sure it's exactly the same.
          </p>
          <button className={first ? "btn primary block" : "btn small"} onClick={done}>
            It matches my device
          </button>
        </>
      )}
      {error && (
        <p className="field-error" role="alert" style={{ marginTop: 10 }}>
          <Icon name="warn" size={14} /> {error}
        </p>
      )}
      {first && onSkip && (
        <p style={{ textAlign: "center", margin: "14px 0 0" }}>
          <button className="text-button" onClick={onSkip}>
            Show it without checking
          </button>
        </p>
      )}
    </>
  );
  return first ? body : <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 8, marginBottom: 14 }}>{body}</div>;
}
