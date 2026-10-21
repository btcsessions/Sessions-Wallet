import { useState } from "react";
import { api } from "../api";
import type { Execution, FeePriority } from "../api/types";
import { AnimatedPsbtQR } from "../components/AnimatedQR";
import { Icon } from "../components/Icon";
import { ScanButton, type ScanResult } from "../components/Scanner";
import { CopyButton, Sheet, useBusy } from "../components/ui";
import { DeviceIcon, deviceFor } from "../hw/devices";
import { isMobile, trezorSign } from "../hw/trezor";
import { errorText } from "../format";
import { JadeQrUnlock } from "../hw/JadeUnlock";
import { useWallet } from "../state/store";

/** A signed PSBT from whatever the device showed: UR, base64 or hex. */
async function signedFrom(r: ScanResult): Promise<string> {
  if (r.kind === "ur") {
    const { psbtFromUr } = await import("../hw/ur");
    const { Buffer } = await import("buffer");
    return psbtFromUr(r.type, Buffer.from(r.cbor));
  }
  return r.text.trim();
}

/** Hardware wallet / cosigner round-trip for a paused payment or move. */
export function SignStep({ paused, fee, onDone }: { paused: Execution; fee: FeePriority; onDone: () => void }) {
  const { refresh, accounts } = useWallet();
  const [signed, setSigned] = useState("");
  const [showManual, setShowManual] = useState(false);
  const [unlock, setUnlock] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, run] = useBusy();
  const quote = paused.awaitingSignature;
  const ns = quote?.needsSignature;
  if (!quote || !ns) return null;

  const account = accounts.find((a) => a.id === quote.account);
  const device = account?.custody.type === "hardware" ? account.custody.device : undefined;
  const def = device ? deviceFor(device) : undefined;
  const isTrezor = def?.connect === "trezor";
  const name = def && def.id !== "other" ? def.name : "your hardware wallet";

  const broadcast = (psbt: string) =>
    run(async () => {
      setError(null);
      try {
        await (await api()).resume(paused, psbt.trim(), fee);
        await refresh();
        onDone();
      } catch (e) {
        setError(errorText(e));
      }
    });

  const signWithTrezor = () =>
    run(async () => {
      setError(null);
      try {
        const psbt = await trezorSign(ns.psbtBase64);
        await (await api()).resume(paused, psbt, fee);
        await refresh();
        onDone();
      } catch (e) {
        setError(errorText(e));
      }
    });

  const more = ns.signaturesNeeded > 1 ? ` and ${ns.signaturesNeeded - 1} more cosigner${ns.signaturesNeeded > 2 ? "s" : ""}` : "";

  return (
    <Sheet title="Sign on your device">
      {device && (
        <div className="hw-hero" style={{ width: 64, height: 64, borderRadius: 18, marginBottom: 12 }}>
          <DeviceIcon device={device} size={38} />
        </div>
      )}

      {isTrezor ? (
        <>
          <p className="notice">
            {isMobile()
              ? `This opens Trezor Suite. Check the amount and address on your ${name}, confirm, and you'll come back here to send.`
              : `Keep Trezor Suite open with your ${name} unlocked. Check the amount and address on the device's screen, then confirm.`}
          </p>
          <button className="btn primary block" disabled={busy} onClick={signWithTrezor}>
            {busy ? "Waiting for Trezor…" : "Sign with Trezor"}
          </button>
        </>
      ) : def?.connect === "jade" && unlock ? (
        <>
          <JadeQrUnlock onDone={() => setUnlock(false)} />
          <p style={{ textAlign: "center", margin: "14px 0 0" }}>
            <button className="text-button" onClick={() => setUnlock(false)}>
              Back to signing
            </button>
          </p>
        </>
      ) : (
        <>
          {def?.connect === "jade" && (
            <p style={{ textAlign: "center", margin: "0 0 10px" }}>
              <button className="text-button" onClick={() => setUnlock(true)}>
                Jade locked? Unlock it with QR first
              </button>
            </p>
          )}
          <p className="notice">
            Scan this with {name}
            {more}. Check the amount and address on its screen, then scan the signed transaction it shows.
          </p>
          <div className="qr-wrap">
            <AnimatedPsbtQR psbt={ns.psbtBase64} />
          </div>
          {signed ? (
            <>
              <p className="group-note" style={{ textAlign: "center", color: "var(--green)" }}>
                <Icon name="check" size={14} /> Signed transaction scanned
              </p>
              <button className="btn primary block" disabled={busy} onClick={() => broadcast(signed)}>
                {busy ? "Broadcasting…" : "Broadcast"}
              </button>
            </>
          ) : (
            <ScanButton
              className="btn primary block"
              label="Scan signed transaction"
              title="Scan signed transaction"
              hint={`Scan the QR code ${name} shows after signing.`}
              onResult={(r) =>
                signedFrom(r)
                  .then((p) => (setSigned(p), setError(null)))
                  .catch((e) => setError(errorText(e)))
              }
            />
          )}
        </>
      )}

      {error && (
        <p className="field-error" role="alert" style={{ marginTop: 14 }}>
          <Icon name="warn" size={14} /> {error}
        </p>
      )}

      <p style={{ textAlign: "center", margin: "16px 0 0" }}>
        <button className="text-button" onClick={() => setShowManual(!showManual)}>
          {showManual ? "Hide file and copy options" : "Use a PSBT file or copy-paste"}
        </button>
      </p>
      {showManual && (
        <>
          <div
            style={{
              display: "flex",
              justifyContent: "center",
              margin: "8px 0 18px",
            }}
          >
            <CopyButton text={ns.psbtBase64} label="Copy PSBT" />
          </div>
          <p className="group-label">Signed transaction</p>
          <div className="group">
            <label className="cell">
              <span className="visually-hidden">Signed PSBT</span>
              <textarea value={signed} onChange={(e) => setSigned(e.target.value)} placeholder="Paste the signed PSBT" spellCheck={false} />
            </label>
          </div>
          <button className="btn block" disabled={!signed.trim() || busy} onClick={() => broadcast(signed)}>
            {busy ? "Broadcasting…" : "Broadcast"}
          </button>
        </>
      )}
    </Sheet>
  );
}
