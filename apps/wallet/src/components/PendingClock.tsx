import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { formatSats } from "../format";
import { Icon } from "./Icon";

/**
 * A small clock next to a balance that includes unconfirmed sats. Tapping it
 * says how much is still waiting for a confirmation.
 */
export function PendingClock({ sats, size = 16 }: { sats: number; size?: number }) {
  const [open, setOpen] = useState(false);
  useEffect(() => {
    if (!open) setPos(null);
  }, [open]);
  const ref = useRef<HTMLSpanElement>(null);
  const pop = useRef<HTMLSpanElement>(null);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);

  // Place the bubble under the clock, kept inside the screen. It lives in
  // <body> so cards (which contain their layout) can't clip or shift it.
  useLayoutEffect(() => {
    if (!open || !ref.current || !pop.current) return;
    const r = ref.current.getBoundingClientRect();
    const w = pop.current.offsetWidth;
    const margin = 12;
    const left = Math.min(Math.max(r.right + 10 - w, margin), window.innerWidth - w - margin);
    setPos({ top: r.bottom + 8, left });
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const close = (e: PointerEvent) => ref.current && !ref.current.contains(e.target as Node) && !pop.current?.contains(e.target as Node) && setOpen(false);
    const away = () => setOpen(false);
    window.addEventListener("scroll", away, { capture: true, passive: true });
    const t = setTimeout(() => setOpen(false), 4000);
    document.addEventListener("pointerdown", close, true);
    return () => {
      clearTimeout(t);
      document.removeEventListener("pointerdown", close, true);
      window.removeEventListener("scroll", away, { capture: true });
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
      {open &&
        createPortal(
          <span className="pending-pop" role="status" ref={pop} style={pos ? { top: pos.top, left: pos.left } : { visibility: "hidden", top: 0, left: 0 }}>
            <strong className="num">{formatSats(sats)} sats</strong> waiting for confirmation
          </span>,
          document.body,
        )}
    </span>
  );
}
