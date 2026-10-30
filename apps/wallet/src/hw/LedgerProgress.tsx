import { useEffect, useRef, useState } from "react";
import { api } from "../api";
import type { LedgerState } from "../api/types";
import { DeviceStuck } from "./Stuck";

type Step = LedgerState | "stuck";

/** No word from the Ledger for this long while connecting: say what to try. */
const STUCK_AFTER_MS = 25_000;

/** Live progress from a Ledger while `active`. */
export function useLedgerProgress(active: boolean): Step | null {
  const [step, setStep] = useState<Step | null>(null);
  useEffect(() => {
    let off: (() => void) | undefined;
    let alive = true;
    void api()
      .then((a) => a.onLedger((s) => setStep(s)))
      .then((f) => (alive ? (off = f) : f()));
    return () => {
      alive = false;
      off?.();
    };
  }, []);
  useEffect(() => {
    if (!active) setStep(null);
  }, [active]);
  useEffect(() => {
    if (!active || (step && step !== "connecting" && step !== "bluetooth")) return;
    const t = setTimeout(() => setStep((s) => (!s || s === "connecting" || s === "bluetooth" ? "stuck" : s)), STUCK_AFTER_MS);
    return () => clearTimeout(t);
  }, [active, step]);
  // Leaving the screen mid-request: stop it, so the Ledger is free next time.
  const activeRef = useRef(active);
  activeRef.current = active;
  useEffect(
    () => () => {
      if (activeRef.current) void api().then((a) => a.ledgerCancel());
    },
    [],
  );
  return active ? step : null;
}

/** What to do on the Ledger right now. */
export function LedgerProgress({ step, signing, address }: { step: Step | null; signing?: boolean; address?: boolean }) {
  if (!step) return null;
  if (step === "stuck") {
    return (
      <DeviceStuck
        text="Still looking for the Ledger. Plug it in (or turn on its Bluetooth and keep it close), unlock it, and close Ledger Live if it's open. Then try again."
        onStop={() => void api().then((a) => a.ledgerCancel())}
      />
    );
  }
  const text: Record<LedgerState, string> = {
    connecting: "Connecting to your Ledger…",
    bluetooth: "Not plugged in: looking for your Ledger over Bluetooth…",
    pairing: "Pair with your Ledger: check the code on its screen matches, then confirm on both.",
    unlock: "Unlock your Ledger with its PIN.",
    "open-app": "Confirm opening the Bitcoin app on your Ledger.",
    confirm: signing
      ? "Check the amount and address on your Ledger, then sign on the device."
      : address
        ? "Check the address on your Ledger matches, then confirm on the device."
        : "Confirm on your Ledger.",
  };
  return (
    <div className="device-progress" role="status">
      <p>{text[step]}</p>
    </div>
  );
}
