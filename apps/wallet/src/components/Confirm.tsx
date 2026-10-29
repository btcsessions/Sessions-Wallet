import { useEffect, useState } from "react";

/**
 * An in-app yes/no. `window.confirm` does nothing in the desktop app's
 * webview (it just returns false), so every confirmation goes through here.
 */
type Ask = { message: string; ok: string; danger: boolean; resolve: (yes: boolean) => void };
let show: ((a: Ask) => void) | null = null;

export function confirmDialog(message: string, opts: { ok?: string; danger?: boolean } = {}): Promise<boolean> {
  return new Promise((resolve) => {
    if (!show) return resolve(false);
    show({ message, ok: opts.ok ?? "OK", danger: opts.danger ?? false, resolve });
  });
}

/** Mounted once at the app root. */
export function ConfirmHost() {
  const [ask, setAsk] = useState<Ask | null>(null);
  useEffect(() => {
    show = setAsk;
    return () => {
      show = null;
    };
  }, []);
  useEffect(() => {
    if (!ask) return;
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && answer(false);
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }); // eslint-disable-line react-hooks/exhaustive-deps
  if (!ask) return null;
  const answer = (yes: boolean) => {
    ask.resolve(yes);
    setAsk(null);
  };
  return (
    <>
      <div className="sheet-scrim confirm-scrim" onClick={() => answer(false)} />
      <div className="confirm" role="alertdialog" aria-modal="true" aria-label="Confirm">
        <p>{ask.message}</p>
        <div className="confirm-actions">
          <button className="btn" autoFocus onClick={() => answer(false)}>
            Cancel
          </button>
          <button className={`btn primary${ask.danger ? " danger" : ""}`} onClick={() => answer(true)}>
            {ask.ok}
          </button>
        </div>
      </div>
    </>
  );
}
