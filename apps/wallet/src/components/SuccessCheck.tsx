import { useEffect } from "react";

/** A short buzz, where the phone allows it. */
export function buzz(pattern: number | number[] = [18, 60, 36]) {
  try {
    navigator.vibrate?.(pattern);
  } catch {
    /* no vibration motor or not allowed */
  }
}

/**
 * The "it landed" moment: a green disc pops in, a ring pulses out, and the
 * tick draws itself. `pending` shows it amber with a clock-like ring (seen,
 * not yet confirmed).
 */
export function SuccessCheck({ pending = false, size = 96 }: { pending?: boolean; size?: number }) {
  useEffect(() => {
    buzz(pending ? 24 : [18, 60, 36]);
  }, [pending]);
  return (
    <div className={`success-check${pending ? " pending" : ""}`} style={{ width: size, height: size }} aria-hidden>
      <span className="success-ring" />
      <svg viewBox="0 0 96 96" width={size} height={size}>
        <circle className="success-disc" cx="48" cy="48" r="44" />
        <path className="success-tick" d="M29 49.5 L42.5 63 L68 35" />
      </svg>
    </div>
  );
}
