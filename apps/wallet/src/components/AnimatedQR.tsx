import { useEffect, useState } from "react";
import QRCode from "qrcode";

/** QR density: bytes per frame and how long each frame stays up. Lower is easier for slow cameras. */
const DENSITY = [
  { label: "Low", fragment: 70, intervalMs: 500 },
  { label: "Medium", fragment: 120, intervalMs: 420 },
  { label: "High", fragment: 220, intervalMs: 350 },
] as const;

function savedDensity(): number {
  try {
    const v = Number(localStorage.getItem("qrDensity"));
    return v >= 0 && v <= 2 && localStorage.getItem("qrDensity") !== null ? v : 1;
  } catch {
    return 1;
  }
}

/**
 * A PSBT as an animated UR (`crypto-psbt`) QR code, the format SeedSigner
 * and Jade scan. With `adjustable`, a density slider shows when the PSBT
 * needs more than one frame.
 */
export function AnimatedPsbtQR({ psbt, adjustable = false }: { psbt: string; adjustable?: boolean }) {
  const [level, setLevel] = useState(savedDensity);
  const d = DENSITY[level];
  // base64 → bytes; one frame at the lowest density means density changes nothing.
  const multiFrame = Math.floor((psbt.length * 3) / 4) > DENSITY[0].fragment;
  const choose = (v: number) => {
    setLevel(v);
    try {
      localStorage.setItem("qrDensity", String(v));
    } catch {
      /* best effort */
    }
  };
  return (
    <div style={{ display: "grid", gap: 12, justifyItems: "center", width: "100%" }}>
      <AnimatedUrQR make={async () => (await import("../hw/ur")).psbtFrames(psbt, d.fragment)} deps={[psbt, d.fragment]} intervalMs={d.intervalMs} large={adjustable} />
      {adjustable && multiFrame && (
        <label className="density">
          <span className="density-head">
            <span>QR density</span>
            <strong>{d.label}</strong>
          </span>
          <input type="range" min={0} max={2} step={1} value={level} onChange={(e) => choose(Number(e.target.value))} aria-valuetext={d.label} />
          <span className="density-ends" aria-hidden>
            <span>Easier to scan</span>
            <span>Fewer frames</span>
          </span>
        </label>
      )}
    </div>
  );
}

/** Any UR payload as a QR code (animated when it doesn't fit one frame). */
export function UrQR({ type, cbor, fragment = 400 }: { type: string; cbor: Uint8Array; fragment?: number }) {
  return <AnimatedUrQR make={async () => (await import("../hw/ur")).urFrames(type, cbor, fragment)} deps={[type, cbor, fragment]} intervalMs={280} />;
}

type Frames = { next: () => string; count: number };

function AnimatedUrQR({ make, deps, intervalMs, large }: { make: () => Promise<Frames>; deps: unknown[]; intervalMs: number; large?: boolean }) {
  const [svg, setSvg] = useState("");
  const [parts, setParts] = useState(0);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setInterval> | undefined;
    (async () => {
      const frames = await make();
      if (!alive) return;
      setParts(frames.count);
      const draw = async () => {
        // Uppercase UR parts use the compact alphanumeric QR mode.
        const s = await QRCode.toString(frames.next(), {
          type: "svg",
          margin: 0,
          errorCorrectionLevel: "L",
          color: { dark: "#000000", light: "#ffffff" },
        });
        if (alive) setSvg(s);
      };
      await draw();
      if (frames.count > 1) timer = setInterval(() => void draw(), intervalMs);
    })().catch(() => alive && setSvg(""));
    return () => {
      alive = false;
      if (timer) clearInterval(timer);
    };
  }, [...deps, intervalMs]); // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <div style={{ display: "grid", justifyItems: "center", gap: 8 }}>
      <div className={`qr${large ? " large" : ""}`} aria-label={parts > 1 ? `Animated QR code, ${parts} parts` : "QR code"} role="img" dangerouslySetInnerHTML={{ __html: svg }} />
      {parts > 1 && (
        <span className="group-note" style={{ margin: 0 }}>
          Animated · {parts} parts
        </span>
      )}
    </div>
  );
}
