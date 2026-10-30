import { useRef, useState } from "react";
import { api } from "../api";
import type { VaultKey } from "../api/types";
import { Icon } from "../components/Icon";
import { ScanButton, type ScanResult } from "../components/Scanner";
import { Sheet, useBusy } from "../components/ui";
import { errorText } from "../format";
import { BitboxProgress, useBitboxProgress } from "./BitboxProgress";
import { DEVICES, DeviceIcon, jadeLiveHint, viaFor, type DeviceDef } from "./devices";
import { JadeModeSwitch, JadeProgress, useJadeProgress, type JadeMode } from "./JadeProgress";
import { isMobile } from "./trezor";

/** The multisig (BIP48) key in whatever a device's QR showed. */
export async function keyTextFromScan(r: ScanResult): Promise<string> {
  if (r.kind === "text") return r.text;
  const { keysFromUr, textFromUr } = await import("./ur");
  if (r.type === "bytes") return textFromUr(r.type, r.cbor);
  const keys = keysFromUr(r.type, r.cbor);
  const multi = keys.find((k) => /\/48h\/[01]h\/\d+h\/2h\]/.test(k)) ?? keys.find((k) => /\/48h\//.test(k));
  if (!multi) throw new Error("That's a single-signature export. On the device, choose the Multisig export");
  return multi;
}

const QR_HINT: Record<string, string> = {
  "passport-prime": "On Prime: open Bitcoin Wallet, tap your account, then Connect Wallet › Sparrow › Multisig. Scan the animated QR.",
  jade: "On Jade: Options › Wallet › Export Xpub. Choose Multisig and Native SegWit, then scan the QR.",
  seedsigner: "On SeedSigner: load your seed, then Export Xpub › Multisig › Native SegWit › Sparrow. Scan the animated QR.",
};

type Source = { kind: "phone" } | { kind: "device"; def: DeviceDef } | { kind: "other" };

/**
 * Add one key to a vault: this phone's own key, a hardware wallet (plugged in, Bluetooth or QR), or a key from
 * someone else (QR, paste or file). Resolves with the key and a suggested label.
 */
export function KeyPicker({
  index,
  phoneUsed,
  taken,
  onPick,
  onCancel,
}: {
  index: number;
  phoneUsed: boolean;
  /** Fingerprints already in the vault. */
  taken: string[];
  onPick: (k: VaultKey) => void;
  onCancel: () => void;
}) {
  const [source, setSource] = useState<Source | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, run] = useBusy();
  const [talking, setTalking] = useState(false);
  const def = source?.kind === "device" ? source.def : null;
  const bitbox = useBitboxProgress(talking && def?.connect === "bitbox");
  const jade = useJadeProgress(talking && def?.connect === "jade");
  const [jadeMode, setJadeMode] = useState<JadeMode>("ble");
  const [paste, setPaste] = useState("");
  const file = useRef<HTMLInputElement>(null);

  const accept = async (k: VaultKey, label: string) => {
    if (taken.includes(k.fingerprint)) {
      setError("That key is already in this vault. Each key needs its own device or seed.");
      return;
    }
    onPick({ ...k, label: k.label || label });
  };

  const read = async (text: string, label: string, device?: string) => {
    setError(null);
    try {
      const k = await (await api()).vaultReadKey(text);
      const source = device && k.source.type === "external" ? { type: "device" as const, device } : k.source;
      await accept({ ...k, source }, label);
    } catch (e) {
      setError(errorText(e));
    }
  };

  const fromPhone = () =>
    run(async () => {
      try {
        await accept(await (await api()).vaultPhoneKey(), "This phone");
      } catch (e) {
        setError(errorText(e));
      }
    });

  const fromDevice = (d: DeviceDef) =>
    run(async () => {
      setError(null);
      setTalking(true);
      try {
        const a = await api();
        let key: string;
        if (d.connect === "bitbox") key = await a.bitboxMultisigKey(0);
        else if (d.connect === "jade") key = await a.jadeMultisigKey(0);
        else {
          const { trezorMultisigKey } = await import("./trezor");
          key = await trezorMultisigKey(0);
        }
        await read(key, d.name, d.name);
      } catch (e) {
        setError(errorText(e));
      } finally {
        setTalking(false);
      }
    });

  const scanned = (r: ScanResult, label: string, device?: string) =>
    keyTextFromScan(r)
      .then((t) => read(t, label, device))
      .catch((e) => setError(errorText(e)));

  const back = (
    <button className="text-button" onClick={() => (source ? (setSource(null), setError(null)) : onCancel())}>
      {source ? "Back" : "Cancel"}
    </button>
  );

  const errorLine = error && (
    <p className="field-error" role="alert" style={{ marginTop: 14 }}>
      <Icon name="warn" size={14} /> {error}
    </p>
  );

  if (!source) {
    return (
      <Sheet title={`Key ${index + 1}`} left={back}>
        <p className="group-note" style={{ marginTop: 0 }}>
          Where does this key live? Use a different device or person for each key, so losing one never costs you two.
        </p>
        <div className="device-grid">
          <button className="device-card featured" disabled={phoneUsed || busy} onClick={() => (setSource({ kind: "phone" }), void fromPhone())}>
            <span className="device-icon-wrap">
              <Icon name="key" size={36} />
            </span>
            <strong>This phone</strong>
            <span>{phoneUsed ? "Already in this vault" : "A key from your recovery phrase"}</span>
          </button>
          {/* Ledger can't sign for vaults here yet. */}
          {DEVICES.filter((d) => d.connect !== "ledger").map((d) => (
            <button key={d.id} className="device-card featured" onClick={() => setSource({ kind: "device", def: d })}>
              <DeviceIcon device={d.name} size={40} />
              <strong>{d.name}</strong>
              <span>{viaFor(d)}</span>
            </button>
          ))}
          <button className="device-card featured" onClick={() => setSource({ kind: "other" })}>
            <span className="device-icon-wrap">
              <Icon name="people" size={36} />
            </span>
            <strong>Someone else's key</strong>
            <span>QR code, file or text</span>
          </button>
        </div>
        {errorLine}
      </Sheet>
    );
  }

  if (source.kind === "phone") {
    return (
      <Sheet title="This phone" left={back}>
        <p className="empty">{busy ? "Making this phone's key…" : ""}</p>
        {errorLine}
      </Sheet>
    );
  }

  if (source.kind === "other") {
    return (
      <Sheet title="Someone else's key" left={back}>
        <p className="notice">Ask them to export their multisig key (often called "Multisig xpub" or "BIP48") from their wallet, then scan it, paste it or open the file.</p>
        <ScanButton className="btn primary block" label="Scan their key" title="Scan a key" hint="Scan the multisig key (xpub) QR code." onResult={(r) => void scanned(r, "")} />
        <div style={{ height: 12 }} />
        <div className="group">
          <label className="cell">
            <span className="visually-hidden">Key</span>
            <textarea value={paste} onChange={(e) => setPaste(e.target.value)} placeholder="[fingerprint/48h/0h/0h/2h]xpub… or Zpub…" spellCheck={false} autoCapitalize="off" />
          </label>
        </div>
        <div style={{ display: "flex", gap: 10 }}>
          <button className="btn block" disabled={!paste.trim() || busy} onClick={() => void read(paste, "")}>
            Use this key
          </button>
          <button className="btn block" onClick={() => file.current?.click()}>
            <Icon name="paste" size={16} /> Open file
          </button>
        </div>
        <input
          ref={file}
          type="file"
          accept=".json,.txt,text/plain,application/json"
          hidden
          onChange={async (e) => {
            const f = e.target.files?.[0];
            e.target.value = "";
            if (f) await read(await f.text(), "");
          }}
        />
        {errorLine}
      </Sheet>
    );
  }

  const d = source.def;
  const direct = d.connect === "trezor" || d.connect === "bitbox" || (d.connect === "jade" && jadeMode === "ble");
  return (
    <Sheet title={d.name} left={back}>
      <div className="hw-hero">
        <DeviceIcon device={d.name} size={56} />
      </div>
      {d.connect === "jade" && <JadeModeSwitch value={jadeMode} onChange={(m) => (setJadeMode(m), setError(null))} />}
      {direct ? (
        <>
          <p className="notice">
            {d.connect === "bitbox"
              ? `Plug in your BitBox02${isMobile() ? " and allow access when asked" : ""}. You'll unlock it with your password on the device.`
              : d.connect === "jade"
                ? `${jadeLiveHint()} Enter your PIN on Jade if asked.`
                : isMobile()
                  ? "This opens Trezor Suite. Unlock your Trezor and approve sharing the key, and you'll come back here."
                  : "Open Trezor Suite on this computer and unlock your Trezor."}{" "}
            The wallet reads its multisig key (m/48'/0'/0'/2'). Nothing is signed.
          </p>
          {d.connect === "bitbox" && <BitboxProgress step={bitbox} />}
          {d.connect === "jade" && <JadeProgress state={jade} />}
          <button className="btn primary block" disabled={busy} onClick={() => fromDevice(d)}>
            {busy ? `Waiting for ${d.name}…` : `Get key from ${d.name}`}
          </button>
        </>
      ) : (
        <>
          <p className="notice">{QR_HINT[d.id] ?? QR_HINT[d.connect] ?? "Export the device's multisig key (BIP48, Native SegWit) as a QR code and scan it."}</p>
          <ScanButton
            className="btn primary block"
            label="Scan key"
            title={`Scan ${d.name} key`}
            hint="Scan the multisig key QR."
            onResult={(r) => void scanned(r, d.name, d.name)}
          />
        </>
      )}
      {errorLine}
    </Sheet>
  );
}
