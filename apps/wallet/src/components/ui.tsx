import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import QRCode from "qrcode";
import { Icon } from "./Icon";
import { api } from "../api";
import type { FeePriority, FeeRates } from "../api/types";
import { Dropdown } from "./Dropdown";
import { useWallet } from "../state/store";
import { formatFiat, formatSats } from "../format";

/** Inside a desktop page: sheets render in place, not as a dialog. */
export const InlineSheet = createContext<null | { onClose: () => void }>(null);

export function Sheet({
  title,
  children,
  left,
  right,
  onClose,
}: {
  title: string;
  children: ReactNode;
  left?: ReactNode;
  right?: ReactNode;
  onClose?: () => void;
}) {
  const close = useWallet((s) => s.close);
  const inline = useContext(InlineSheet);
  const dismiss = onClose ?? inline?.onClose ?? close;
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (inline) return;
    const prev = document.activeElement as HTMLElement | null;
    ref.current?.focus();
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && dismiss();
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
      prev?.focus?.();
    };
  }, [dismiss]);

  if (inline) {
    return (
      <section className="page" aria-label={title}>
        <header className="page-header">
          <div className="left">{left}</div>
          <h1>{title}</h1>
          <div className="right">{right}</div>
        </header>
        <div className="page-body">{children}</div>
      </section>
    );
  }

  return (
    <>
      <div className="sheet-scrim" onClick={dismiss} />
      <div className="sheet" role="dialog" aria-modal="true" aria-label={title} ref={ref} tabIndex={-1}>
        <div className="sheet-grabber" />
        <header className="sheet-header">
          <div className="left">{left}</div>
          <h2>{title}</h2>
          <div className="right">
            {right ?? (
              <button className="text-button strong" onClick={dismiss}>
                Done
              </button>
            )}
          </div>
        </header>
        <div className="sheet-body">{children}</div>
      </div>
    </>
  );
}

export function Toast() {
  const toast = useWallet((s) => s.toast);
  if (!toast) return null;
  return (
    <div className={`toast ${toast.kind}`} role="status" aria-live="polite">
      <Icon name={toast.kind === "ok" ? "check" : "warn"} size={18} />
      {toast.text}
    </div>
  );
}

/** Press and hold to confirm: prevents accidental sends without a modal. */
export function HoldButton({ children, onConfirm, disabled }: { children: ReactNode; onConfirm: () => void; disabled?: boolean }) {
  const [progress, setProgress] = useState(0);
  const raf = useRef<number | null>(null);
  const start = useRef(0);
  const HOLD_MS = 900;

  const stop = () => {
    if (raf.current) cancelAnimationFrame(raf.current);
    raf.current = null;
    setProgress(0);
  };

  const tick = (t: number) => {
    const p = Math.min(1, (t - start.current) / HOLD_MS);
    setProgress(p);
    if (p >= 1) {
      raf.current = null;
      navigator.vibrate?.(12);
      onConfirm();
      return;
    }
    raf.current = requestAnimationFrame(tick);
  };

  const begin = () => {
    if (disabled || raf.current) return;
    start.current = performance.now();
    raf.current = requestAnimationFrame(tick);
  };

  useEffect(() => stop, []);

  return (
    <button
      className="btn primary block hold"
      disabled={disabled}
      style={{ ["--progress" as string]: progress }}
      onPointerDown={begin}
      onPointerUp={stop}
      onPointerLeave={stop}
      onPointerCancel={stop}
      onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && !e.repeat && begin()}
      onKeyUp={stop}
      onContextMenu={(e) => e.preventDefault()}
    >
      <span className="fill" />
      <span>{children}</span>
    </button>
  );
}

export function QR({ value }: { value: string }) {
  const [svg, setSvg] = useState("");
  useEffect(() => {
    QRCode.toString(value, { type: "svg", margin: 0, errorCorrectionLevel: "M", color: { dark: "#000000", light: "#ffffff" } })
      .then(setSvg)
      .catch(() => setSvg(""));
  }, [value]);
  return <div className="qr" aria-label="QR code" role="img" dangerouslySetInnerHTML={{ __html: svg }} />;
}

/** `sensitive`: a secret; cleared from the clipboard after a minute. */
export function CopyButton({ text, label = "Copy", sensitive = false }: { text: string; label?: string; sensitive?: boolean }) {
  const notify = useWallet((s) => s.notify);
  return (
    <button
      className="btn small"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(text);
          notify("ok", sensitive ? "Copied. Cleared from the clipboard in a minute." : "Copied");
          if (sensitive) setTimeout(() => void navigator.clipboard.writeText("").catch(() => {}), 60_000);
        } catch {
          notify("error", "Couldn't copy. Select the text instead.");
        }
      }}
    >
      <Icon name="copy" size={16} />
      {label}
    </button>
  );
}

/** Big amount field in sats, with a fiat readout underneath. */
export function AmountInput({
  value,
  onChange,
  max,
  autoFocus,
}: {
  value: number | null;
  onChange: (v: number | null) => void;
  max?: number;
  autoFocus?: boolean;
}) {
  const { price, currency } = useWallet();
  return (
    <div className="amount-entry">
      <label className="visually-hidden" htmlFor="amount">
        Amount in sats
      </label>
      <input
        id="amount"
        className="num"
        inputMode="numeric"
        autoComplete="off"
        placeholder="0"
        autoFocus={autoFocus}
        value={value == null ? "" : formatSats(value)}
        onChange={(e) => {
          const digits = e.target.value.replace(/\D/g, "");
          onChange(digits ? Math.min(Number(digits), 21e14) : null);
        }}
      />
      <div className="balance-sub num">
        sats{value ? `, about ${formatFiat(value, price, currency)}` : ""}
        {max != null && <>. {formatSats(max)} available</>}
      </div>
    </div>
  );
}

let secureCount = 0;

/**
 * While `on`, a secret is on screen: no screenshots, screen recording, app
 * switcher preview or overlay taps (Android), no screen capture (desktop).
 */
export function useSecureScreen(on = true) {
  useEffect(() => {
    if (!on) return;
    if (secureCount++ === 0) void api().then((a) => a.setSecureScreen(true)).catch(() => {});
    return () => {
      if (--secureCount === 0) void api().then((a) => a.setSecureScreen(false)).catch(() => {});
    };
  }, [on]);
}

export function useBusy() {
  const [busy, setBusy] = useState(false);
  const fail = useWallet((s) => s.fail);
  const run = async (fn: () => Promise<void>) => {
    if (busy) return;
    setBusy(true);
    try {
      await fn();
    } catch (e) {
      fail(e);
    } finally {
      setBusy(false);
    }
  };
  return [busy, run] as const;
}

/** A row with a label and the in-app dropdown. */
export function SelectRow<T extends string>({
  label,
  value,
  onChange,
  options,
  dot,
}: {
  label: string;
  value: T | undefined;
  onChange: (v: T) => void;
  options: { value: T; label: string; detail?: string; color?: string }[];
  dot?: string;
}) {
  return <Dropdown label={label} value={value} onChange={onChange} options={options.map((o) => (o.value === value && dot && !o.color ? { ...o, color: dot } : o))} />;
}

/** Efficient / Private choice for on-chain sends. `unavailable` greys out Private and says why. */
export function SpendStyleSwitch({
  value,
  onChange,
  unavailable,
  payjoin = false,
}: {
  value: "efficient" | "private";
  onChange: (v: "efficient" | "private") => void;
  unavailable?: string | null;
  /** The recipient offers payjoin, so Private uses it. */
  payjoin?: boolean;
}) {
  const [why, setWhy] = useState(false);
  return (
    <>
      <p className="group-label">Privacy</p>
      <div className="segmented" style={{ marginBottom: 6 }}>
        <button aria-pressed={value === "efficient"} onClick={() => onChange("efficient")}>
          Efficient
        </button>
        {/* Not `disabled`: a disabled button swallows taps, and tapping is how you find out why. */}
        <button
          aria-pressed={value === "private"}
          aria-disabled={!!unavailable}
          aria-expanded={unavailable ? why : undefined}
          onClick={() => (unavailable ? setWhy(!why) : onChange("private"))}
          className={unavailable ? "unavailable" : undefined}
        >
          Private
          {unavailable && (
            <span
              className="info-dot"
              role="button"
              tabIndex={0}
              aria-label="Why Private isn't available"
              onClick={(e) => (e.stopPropagation(), setWhy(!why))}
              onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && setWhy(!why)}
            >
              <Icon name="info" size={13} stroke={2.2} />
            </span>
          )}
        </button>
      </div>
      <p className="group-note" style={{ margin: "0 16px 18px" }} role={unavailable && why ? "status" : undefined}>
        {unavailable && why
          ? `Private isn't available for this payment. ${unavailable.replace(/\.?$/, ".")}`
          : value === "private" && payjoin
            ? "Using payjoin: the recipient adds a coin of theirs, so observers can't tell what you paid. About the usual fee."
            : value === "private"
            ? "Using a coinjoin-style layout: looks like a two-person coinjoin, so observers can't tell which output is the payment. Uses more coins and costs about 2–3× the fee."
            : "Fewest coins and the lowest fee."}
      </p>
    </>
  );
}

const SPEEDS: { id: "slow" | "normal" | "fast"; label: string }[] = [
  { id: "slow", label: "Slow" },
  { id: "normal", label: "Medium" },
  { id: "fast", label: "Fast" },
];

/** Rough time to confirm for a fee choice, given current rates. */
export function feeEta(fee: FeePriority, rates: FeeRates | null): string {
  if (fee === "fast") return "~10 min";
  if (fee === "normal") return "~1 hour";
  if (fee === "slow") return "~1 day";
  if (!rates) return "";
  if (fee >= rates.fast) return "~10 min";
  if (fee >= rates.normal) return "~1 hour";
  if (fee >= rates.slow) return "~1 day";
  return "Could take days";
}

/** Live fee rates, fetched once per screen. */
export function useFeeRates(): FeeRates | null {
  const [rates, setRates] = useState<FeeRates | null>(null);
  useEffect(() => {
    let alive = true;
    api()
      .then((a) => a.feeRates())
      .then((r) => alive && setRates(r))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, []);
  return rates;
}

/** Slow / Medium / Fast, plus a custom sat/vB rate in advanced mode. */
export function FeePicker({ value, onChange, rates }: { value: FeePriority; onChange: (v: FeePriority) => void; rates: FeeRates | null }) {
  const advanced = useWallet((s) => s.layout?.advanced ?? false);
  const custom = typeof value === "number";
  const [draft, setDraft] = useState(custom ? String(value) : "");
  return (
    <>
      <p className="group-label">Speed</p>
      <div className="segmented">
        {SPEEDS.map((s) => (
          <button key={s.id} aria-pressed={value === s.id} onClick={() => onChange(s.id)}>
            {s.label}
          </button>
        ))}
        {advanced && (
          <button
            aria-pressed={custom}
            onClick={() => {
              const start = draft || String(Math.max(1, Math.round(rates?.normal ?? 2)));
              setDraft(start);
              onChange(Number(start));
            }}
          >
            Custom
          </button>
        )}
      </div>
      {advanced && custom && (
        <div className="group" style={{ marginTop: -8 }}>
          <label className="cell">
            <span className="grow">Fee rate</span>
            <input
              className="num"
              inputMode="decimal"
              value={draft}
              style={{ textAlign: "right", maxWidth: 90 }}
              onChange={(e) => {
                const v = e.target.value.replace(/[^\d.]/g, "");
                setDraft(v);
                const n = Number(v);
                if (n >= 1) onChange(Math.round(n * 10) / 10);
              }}
            />
            <span className="value">sat/vB</span>
          </label>
          {rates && (
            <div className="cell" style={{ fontSize: 13, color: "var(--label-2)" }}>
              Now: {rates.slow} slow · {rates.normal} medium · {rates.fast} fast sat/vB
            </div>
          )}
        </div>
      )}
    </>
  );
}

/** Caveats attached to a quote (privacy, payjoin). */
export function QuoteNotes({ notes, payjoin }: { notes?: string[]; payjoin?: boolean }) {
  if (!notes?.length) return null;
  return (
    <div className="notice" role="note">
      {payjoin && <strong>Payjoin. </strong>}
      {notes.map((n, i) => (
        <span key={i}>
          {n}
          {i < notes.length - 1 ? " " : ""}
        </span>
      ))}
    </div>
  );
}
