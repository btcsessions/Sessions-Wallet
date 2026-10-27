import { useEffect, useRef, useState } from "react";
import { api } from "../api";
import type { JadeEvent } from "../api/types";
import { DeviceStuck } from "./Stuck";

type State = "scanning" | "reconnecting" | "connecting" | "pairing" | "unlock" | "confirm" | "connected" | "stuck";

/** No word from Jade for this long while looking for it: say what to try. */
const STUCK_AFTER_MS = 25_000;

/** Live progress from Jade over Bluetooth while `active`. */
export function useJadeProgress(active: boolean): State | null {
  const [state, setState] = useState<State | null>(null);
  // Listen the whole time the screen is open, so the first update isn't missed.
  useEffect(() => {
    let off: (() => void) | undefined;
    let alive = true;
    void api()
      .then((a) => a.onJade((e: JadeEvent) => setState(e.kind === "connected" ? "connected" : e.state)))
      .then((f) => (alive ? (off = f) : f()));
    return () => {
      alive = false;
      off?.();
    };
  }, []);
  useEffect(() => {
    if (!active) setState(null);
  }, [active]);

  // Nothing heard, or still scanning, after a while: probably stuck.
  useEffect(() => {
    if (!active || (state && state !== "scanning" && state !== "reconnecting")) return;
    const t = setTimeout(() => setState((s) => (!s || s === "scanning" || s === "reconnecting" ? "stuck" : s)), STUCK_AFTER_MS);
    return () => clearTimeout(t);
  }, [active, state]);

  // Leaving the screen mid-request: stop it, so Jade is free next time.
  const activeRef = useRef(active);
  activeRef.current = active;
  useEffect(
    () => () => {
      if (activeRef.current) void api().then((a) => a.jadeCancel());
    },
    [],
  );
  return active ? state : null;
}

export function JadeProgress({ state, signing }: { state: State | null; signing?: boolean }) {
  if (!state) return null;
  if (state === "stuck") {
    return (
      <DeviceStuck
        text="Still can't reach Jade. Make sure it's on the Bluetooth screen and unlocked, and that Bluetooth and Nearby devices are allowed for Sessions Wallet. Or turn Jade off and on, then try again."
        onStop={() => void api().then((a) => a.jadeCancel())}
      />
    );
  }
  const text = {
    scanning: "Looking for your Jade. Turn it on, choose Bluetooth, and keep it close.",
    connecting: "Connecting… If asked to pair, check the number matches the one on Jade and confirm on both.",
    reconnecting: "Reconnecting to your Jade… keep it on and close by.",
    pairing: "Pair Jade with this phone: check the number on Jade matches the one on your phone, then confirm on both.",
    unlock: "Enter your PIN on Jade. The app checks it with Jade's PIN server automatically.",
    confirm: "Check the amount and address on Jade, then confirm.",
    connected: signing ? "Check the amount and address on Jade, then confirm." : "Connected to Jade.",
  }[state];
  return (
    <div className="device-progress">
      <span className="spinner" aria-hidden />
      <p>{text}</p>
    </div>
  );
}

export type JadeMode = "ble" | "qr";

/** Bluetooth or QR, for Jade. */
export function JadeModeSwitch({ value, onChange }: { value: JadeMode; onChange: (m: JadeMode) => void }) {
  return (
    <div className="segmented" role="radiogroup" aria-label="Connect Jade by" style={{ marginBottom: 14 }}>
      {(
        [
          ["ble", "Bluetooth"],
          ["qr", "QR code"],
        ] as const
      ).map(([m, label]) => (
        <button key={m} role="radio" aria-checked={m === value} aria-pressed={m === value} onClick={() => onChange(m)}>
          {label}
        </button>
      ))}
    </div>
  );
}
