import { useEffect, useState } from "react";
import { api } from "../api";
import type { BitboxEvent } from "../api/types";

type Step = { state: "connecting" | "unlock" | "confirm" | "paired"; code?: string };

/** Live progress from a plugged-in BitBox02 while `active`. */
export function useBitboxProgress(active: boolean): Step | null {
  const [step, setStep] = useState<Step | null>(null);
  useEffect(() => {
    if (!active) {
      setStep(null);
      return;
    }
    let off: (() => void) | undefined;
    let alive = true;
    void api()
      .then((a) =>
        a.onBitbox((e: BitboxEvent) => {
          if (e.kind === "pairing") setStep({ state: "unlock", code: e.code });
          else if (e.kind === "paired") setStep({ state: "paired" });
          else setStep((s) => ({ state: e.state, code: e.state === "unlock" ? s?.code : undefined }));
        }),
      )
      .then((f) => (alive ? (off = f) : f()));
    return () => {
      alive = false;
      off?.();
    };
  }, [active]);
  return step;
}

/** What to do on the BitBox02 right now. */
export function BitboxProgress({ step, signing }: { step: Step | null; signing?: boolean }) {
  if (!step) return null;
  if (step.code) {
    return (
      <div className="device-progress">
        <p>First time with this BitBox02. Check the code matches the one on its screen, then confirm on the device.</p>
        <div className="pairing-code mono">{step.code}</div>
      </div>
    );
  }
  const text =
    step.state === "connecting"
      ? "Connecting to your BitBox02…"
      : step.state === "unlock"
        ? "Unlock your BitBox02: enter your password on the device."
        : step.state === "confirm" || (step.state === "paired" && signing)
          ? "Check the amount and address on your BitBox02, then confirm on the device."
          : "Connected. Confirm on the BitBox02 if it asks.";
  return (
    <div className="device-progress">
      <span className="spinner" aria-hidden />
      <p>{text}</p>
    </div>
  );
}
