import { useEffect, useRef, useState } from "react";
import { formatSats } from "../format";
import { Icon } from "./Icon";

/**
 * A small clock next to a balance that includes unconfirmed sats. Tapping it
 * says how much is still waiting for a confirmation.
 */
export function PendingClock({ sats, size = 16 }: { sats: number; size?: number }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLSpanElement>(null);

  useEffect(() => {
    if (!open) return;
    const close = (e: PointerEvent) => ref.current && !ref.current.contains(e.target as Node) && setOpen(false);
    const t = setTimeout(() => setOpen(false), 4000);
    document.addEventListener("pointerdown", close, true);
    return () => {
      clearTimeout(t);
      document.removeEventListener("pointerdown", close, true);
    };
  }, [open]);

  if (sats <= 0) return null;
  return (
    <span className="pending-clock" ref={ref} data-no-tap>
      <button
        type="button"
        aria-label={`${formatSats(sats)} sats waiting for confirmation`}
        aria-expanded={open}
        onClick={(e) => {
          e.stopPropagation();
          setOpen(!open);
        }}
      >
        <Icon name="clock" size={size} stroke={2} />
      </button>
      {open && (
        <span className="pending-pop" role="status">
          <strong className="num">{formatSats(sats)} sats</strong> waiting for confirmation
        </span>
      )}
    </span>
  );
}
