import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import QrScanner from "qr-scanner";
import { Icon } from "./Icon";
import type { UrCollector } from "../hw/ur";

export type ScanResult = { kind: "text"; text: string } | { kind: "ur"; type: string; cbor: Uint8Array };

/**
 * Full-screen camera QR scanner. Reads plain codes in one frame and collects
 * animated UR codes (hardware wallets) frame by frame, showing progress.
 * Falls back to reading a QR from a photo or screenshot.
 */
export function Scanner({
  title = "Scan QR code",
  hint,
  onResult,
  onClose,
}: {
  title?: string;
  hint?: string;
  onResult: (r: ScanResult) => void;
  onClose: () => void;
}) {
  const video = useRef<HTMLVideoElement>(null);
  const file = useRef<HTMLInputElement>(null);
  const [error, setError] = useState<string | null>(null);
  const [progress, setProgress] = useState<number | null>(null);
  const done = useRef(false);
  const collector = useRef<UrCollector | null>(null);
  const handler = useRef(onResult);
  handler.current = onResult;

  async function handle(text: string) {
    if (done.current || !text) return;
    if (/^ur:/i.test(text.trim())) {
      if (!collector.current) {
        const { UrCollector } = await import("../hw/ur");
        collector.current ??= new UrCollector();
      }
      try {
        const p = collector.current.receive(text);
        if (!p) return;
        setProgress(p.progress);
        if (!p.done) return;
        const { type, cbor } = collector.current.result();
        finish({ kind: "ur", type, cbor: new Uint8Array(cbor) });
      } catch (e) {
        collector.current = null;
        setProgress(null);
        setError(e instanceof Error ? e.message : String(e));
      }
      return;
    }
    finish({ kind: "text", text: text.trim() });
  }

  function finish(r: ScanResult) {
    if (done.current) return;
    done.current = true;
    navigator.vibrate?.(15);
    handler.current(r);
  }

  useEffect(() => {
    if (!video.current) return;
    const scanner = new QrScanner(video.current, (r) => void handle(r.data), {
      preferredCamera: "environment",
      maxScansPerSecond: 20,
      returnDetailedScanResult: true,
      highlightScanRegion: false,
      highlightCodeOutline: false,
      // Phones: the middle of the frame at near full resolution, so small,
      // dense codes on a hardware wallet's screen stay sharp enough to read.
      // Computers: the whole frame, since codes held to a webcam are rarely centred.
      calculateScanRegion: (v) => {
        if (/android|iphone|ipad/i.test(navigator.userAgent)) {
          const side = Math.round(Math.min(v.videoWidth, v.videoHeight) * 0.85);
          const scaled = Math.min(side, 800);
          return { x: Math.round((v.videoWidth - side) / 2), y: Math.round((v.videoHeight - side) / 2), width: side, height: side, downScaledWidth: scaled, downScaledHeight: scaled };
        }
        const w = Math.min(v.videoWidth, 960);
        return { x: 0, y: 0, width: v.videoWidth, height: v.videoHeight, downScaledWidth: w, downScaledHeight: Math.round((w * v.videoHeight) / Math.max(1, v.videoWidth)) };
      },
    });
    scanner.start().catch((e: unknown) => {
      const msg = String(e instanceof Error ? e.message : e);
      setError(
        /not ?allowed|denied|permission/i.test(msg)
          ? "Camera access is off. Allow it in your device settings, or choose a photo of the QR code."
          : /camera not found|no camera|notfound/i.test(msg)
            ? "No camera found. Choose a photo or screenshot of the QR code instead."
            : `The camera didn't start (${msg}). Choose a photo of the QR code instead.`,
      );
    });
    return () => {
      scanner.destroy();
    };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    // Capture phase, so Escape closes only the scanner, not the sheet under it.
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopImmediatePropagation();
      onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  async function fromImage(f: File | undefined) {
    if (!f) return;
    try {
      const r = await QrScanner.scanImage(f, { returnDetailedScanResult: true });
      if (/^ur:/i.test(r.data) && !/^ur:[^/]+\/[^/]+$/i.test(r.data)) {
        setError("That's one frame of an animated code. Scan it with the camera instead.");
        return;
      }
      setError(null);
      await handle(r.data);
    } catch {
      setError("No QR code found in that image.");
    }
  }

  return createPortal(
    <div className="scanner" role="dialog" aria-modal="true" aria-label={title}>
      <video ref={video} className="scanner-video" muted playsInline />
      <div className="scanner-top">
        <button className="text-button" onClick={onClose}>
          Cancel
        </button>
        <h2>{title}</h2>
        <span style={{ width: 56 }} />
      </div>
      <div className="scanner-frame" aria-hidden>
        {progress !== null && (
          <svg viewBox="0 0 36 36" className="scanner-progress">
            <circle cx="18" cy="18" r="15.5" fill="none" stroke="rgba(255,255,255,0.25)" strokeWidth="3" />
            <circle cx="18" cy="18" r="15.5" fill="none" stroke="#fff" strokeWidth="3" strokeLinecap="round" strokeDasharray={`${Math.round(progress * 97.4)} 97.4`} transform="rotate(-90 18 18)" />
          </svg>
        )}
      </div>
      <div className="scanner-bottom">
        {error ? (
          <p className="scanner-msg error">{error}</p>
        ) : (
          <p className="scanner-msg">
            {progress !== null ? `Reading animated code… ${Math.round(progress * 100)}%. Keep it in view.` : hint ?? "Point the camera at a QR code."}
          </p>
        )}
        <button className="btn small" onClick={() => file.current?.click()}>
          <Icon name="photo" size={16} />
          Choose photo
        </button>
        <input ref={file} type="file" accept="image/*" hidden onChange={(e) => void fromImage(e.target.files?.[0])} />
      </div>
    </div>,
    document.body,
  );
}

/** A button that opens the scanner and hands back the result. */
export function ScanButton({
  onResult,
  label = "Scan",
  title,
  hint,
  className = "btn small",
}: {
  onResult: (r: ScanResult) => void;
  label?: string;
  title?: string;
  hint?: string;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button className={className} onClick={() => setOpen(true)}>
        <Icon name="scan" size={16} />
        {label}
      </button>
      {open && (
        <Scanner
          title={title}
          hint={hint}
          onClose={() => setOpen(false)}
          onResult={(r) => {
            setOpen(false);
            onResult(r);
          }}
        />
      )}
    </>
  );
}

/** Plain text from a scan: payment codes, or ecash tokens sent as animated `bytes`. */
export async function scanText(r: ScanResult): Promise<string> {
  if (r.kind === "text") return r.text;
  const { textFromUr } = await import("../hw/ur");
  return textFromUr(r.type, r.cbor);
}

/**
 * A wallet export from a scan: a single-sig descriptor, or one multisig
 * cosigner key (`[fp/48h/…/2h]xpub`), from plain text or a UR.
 */
export async function scanKey(r: ScanResult, want: "single" | "multisig"): Promise<string> {
  if (want === "single") {
    if (r.kind === "text") return r.text;
    const { descriptorFromUr } = await import("../hw/ur");
    const { Buffer } = await import("buffer");
    return descriptorFromUr(r.type, Buffer.from(r.cbor));
  }
  const keys =
    r.kind === "text"
      ? Array.from(r.text.matchAll(/\[[0-9a-fA-F]{8}[^\]]*\][a-zA-Z0-9]{100,}/g), (m) => m[0])
      : (await import("../hw/ur")).keysFromUr(r.type, r.cbor);
  const ms = keys.find((k) => /\/48['h]\/\d+['h]\/\d+['h]\/2['h]\]/.test(k)) ?? keys.find((k) => /\/48['h]\//.test(k));
  if (!ms) throw new Error("No multisig key in that QR code. On the device, export the multisig (BIP48) key.");
  return ms;
}
