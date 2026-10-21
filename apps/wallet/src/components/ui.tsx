import { useEffect, useRef, useState, type ReactNode } from "react";
import QRCode from "qrcode";
import { Icon } from "./Icon";
import { Dropdown } from "./Dropdown";
import { useWallet } from "../state/store";
import { formatFiat, formatSats } from "../format";

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
  const dismiss = onClose ?? close;
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    ref.current?.focus();
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && dismiss();
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
      prev?.focus?.();
    };
  }, [dismiss]);

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

export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const notify = useWallet((s) => s.notify);
  return (
    <button
      className="btn small"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(text);
          notify("ok", "Copied");
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

/** Efficient / Private choice for on-chain sends. */
export function SpendStyleSwitch({ value, onChange }: { value: "efficient" | "private"; onChange: (v: "efficient" | "private") => void }) {
  return (
    <>
      <p className="group-label">Privacy</p>
      <div className="segmented" style={{ marginBottom: 6 }}>
        <button aria-pressed={value === "efficient"} onClick={() => onChange("efficient")}>
          Efficient
        </button>
        <button aria-pressed={value === "private"} onClick={() => onChange("private")}>
          Private
        </button>
      </div>
      <p className="group-note" style={{ margin: "0 16px 18px" }}>
        {value === "private"
          ? "Looks like a two-person coinjoin, so observers can't tell which output is the payment. Uses more coins and costs about 2–3× the fee."
          : "Fewest coins and the lowest fee."}
      </p>
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
