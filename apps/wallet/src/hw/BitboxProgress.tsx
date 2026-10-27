import { useEffect, useRef, useState } from "react";
import { api } from "../api";
import type { BitboxEvent } from "../api/types";
import { DeviceStuck } from "./Stuck";

type Step = { state: "connecting" | "unlock" | "confirm" | "paired" | "stuck"; code?: string };

/** No word from the BitBox02 for this long while waiting: say what to try. */
const STUCK_AFTER_MS = 20_000;

/** Live progress from a plugged-in BitBox02 while `active`. */
export function useBitboxProgress(active: boolean): Step | null {
  const [step, setStep] = useState<Step | null>(null);
  // Listen the whole time the screen is open, so the first update isn't missed.
  useEffect(() => {
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
  }, []);
  useEffect(() => {
    if (!active) setStep(null);
  }, [active]);

  // Nothing heard for a while: probably stuck (an earlier request, a cable).
  useEffect(() => {
    if (!active || (step && step.state !== "connecting")) return;
    const t = setTimeout(() => setStep((s) => (!s || s.state === "connecting" ? { state: "stuck" } : s)), STUCK_AFTER_MS);
    return () => clearTimeout(t);
  }, [active, step]);

  // Leaving the screen mid-request: stop it, so the device is free next time.
  const activeRef = useRef(active);
  activeRef.current = active;
  useEffect(
    () => () => {
      if (activeRef.current) void api().then((a) => a.bitboxCancel());
    },
    [],
  );
  return active ? step : null;
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
  if (step.state === "stuck") {
    return (
      <DeviceStuck
        text={'Still waiting for the BitBox02. If it shows "See the BitBoxApp" or nothing changes, unplug it, plug it back in and try again. Close the BitBoxApp if it\'s open.'}
        onStop={() => void api().then((a) => a.bitboxCancel())}
      />
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
