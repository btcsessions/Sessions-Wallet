import { useEffect, useState } from "react";
import QRCode from "qrcode";

/**
 * A PSBT as an animated UR (`crypto-psbt`) QR code, the format SeedSigner,
 * Jade, Keystone and Passport scan. Small transactions fit in one frame.
 */
export function AnimatedPsbtQR({ psbt, fragment = 100, intervalMs = 280 }: { psbt: string; fragment?: number; intervalMs?: number }) {
  const [svg, setSvg] = useState("");
  const [parts, setParts] = useState(0);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setInterval> | undefined;
    (async () => {
      const { psbtFrames } = await import("../hw/ur");
      const frames = psbtFrames(psbt, fragment);
      if (!alive) return;
      setParts(frames.count);
      const draw = async () => {
        // Uppercase UR parts use the compact alphanumeric QR mode.
        const s = await QRCode.toString(frames.next(), { type: "svg", margin: 0, errorCorrectionLevel: "L", color: { dark: "#000000", light: "#ffffff" } });
        if (alive) setSvg(s);
      };
      await draw();
      if (frames.count > 1) timer = setInterval(() => void draw(), intervalMs);
    })().catch(() => alive && setSvg(""));
    return () => {
      alive = false;
      if (timer) clearInterval(timer);
    };
  }, [psbt, fragment, intervalMs]);

  return (
    <div style={{ display: "grid", justifyItems: "center", gap: 8 }}>
      <div className="qr" aria-label={parts > 1 ? `Animated QR code, ${parts} parts` : "QR code"} role="img" dangerouslySetInnerHTML={{ __html: svg }} />
      {parts > 1 && <span className="group-note" style={{ margin: 0 }}>Animated · {parts} parts</span>}
    </div>
  );
}
