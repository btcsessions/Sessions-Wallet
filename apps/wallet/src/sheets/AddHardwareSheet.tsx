import { useState } from "react";
import { api } from "../api";
import type { DescriptorPreview, FoundAccount } from "../api/types";
import { Icon } from "../components/Icon";
import { ScanButton, type ScanResult } from "../components/Scanner";
import { SelectRow, Sheet, useBusy } from "../components/ui";
import { DEVICES, DeviceIcon, jadeLiveHint, viaFor, type DeviceDef } from "../hw/devices";
import { LedgerProgress, useLedgerProgress } from "../hw/LedgerProgress";
import { isMobile, trezorDescriptor, trezorDescriptors, type TrezorScript } from "../hw/trezor";
import { errorText, formatSats } from "../format";
import { JadeQrUnlock } from "../hw/JadeUnlock";
import { BitboxProgress, useBitboxProgress } from "../hw/BitboxProgress";
import { JadeModeSwitch, JadeProgress, useJadeProgress, type JadeMode } from "../hw/JadeProgress";
import { switchMode } from "../state/mode";
import { useWallet } from "../state/store";
import { walletHasCard } from "../modules/AccountModule";

/** Turn whatever the device showed into text the import understands. */
async function exportText(r: ScanResult): Promise<string> {
  if (r.kind === "text") return r.text;
  const { descriptorFromUr, textFromUr } = await import("../hw/ur");
  // Passport (Prime and Core) shows its export file as a "bytes" UR.
  if (r.type === "bytes") return textFromUr(r.type, r.cbor);
  const { Buffer } = await import("buffer");
  return descriptorFromUr(r.type, Buffer.from(r.cbor));
}

/** Pick a device → connect or scan → name it and check the first address. */
export function AddHardwareSheet() {
  const { addModule, refresh, notify, close } = useWallet();
  const advanced = useWallet((s) => s.layout?.advanced ?? false);
  const [device, setDevice] = useState<DeviceDef | null>(null);
  const [preview, setPreview] = useState<DescriptorPreview | null>(null);
  const [label, setLabel] = useState("");
  const [paste, setPaste] = useState("");
  const [showPaste, setShowPaste] = useState(false);
  const [script, setScript] = useState<TrezorScript>("wpkh");
  const [accountNo, setAccountNo] = useState("0");
  const [error, setError] = useState<string | null>(null);
  const [busy, run] = useBusy();
  // After adding a Trezor: its other used accounts (null = still looking).
  const [more, setMore] = useState<{
    name: string;
    found: FoundAccount[] | null;
  } | null>(null);
  const [adding, setAdding] = useState<number | null>(null);
  const [unlock, setUnlock] = useState(false);
  const [talking, setTalking] = useState(false);
  const bitbox = useBitboxProgress(talking && device?.connect === "bitbox");
  const [jadeMode, setJadeMode] = useState<JadeMode>("ble");
  const jade = useJadeProgress(talking && device?.connect === "jade");
  const ledger = useLedgerProgress(talking && device?.connect === "ledger");
  // Devices we talk to directly (not by scanning QR codes).
  const direct = device?.connect === "trezor" || device?.connect === "bitbox" || device?.connect === "ledger" || (device?.connect === "jade" && jadeMode === "ble");

  const inspect = async (text: string) => {
    setError(null);
    try {
      const p = await (await api()).inspectDescriptor(text);
      setPreview(p);
    } catch (e) {
      setError(errorText(e));
    }
  };

  /** Descriptors for these account numbers from a connected device. */
  const descriptorsFrom = async (accounts: number[]): Promise<string[]> => {
    if (device?.connect === "bitbox") return (await api()).bitboxDescriptors(script, accounts);
    if (device?.connect === "ledger") return (await api()).ledgerDescriptors(script, accounts);
    if (device?.connect === "jade") return (await api()).jadeDescriptors(script, accounts);
    return trezorDescriptors(script, accounts);
  };

  const fromDevice = () =>
    run(async () => {
      setError(null);
      setTalking(true);
      try {
        const n = Number(accountNo);
        const [d] = device?.connect === "trezor" ? [await trezorDescriptor(script, false, n)] : await descriptorsFrom([n]);
        await inspect(d);
      } catch (e) {
        setError(errorText(e));
      } finally {
        setTalking(false);
      }
    });

  const add = () =>
    run(async () => {
      if (!device || !preview) return;
      const account = await (
        await api()
      ).addAccount({
        kind: "hardware",
        name: label.trim() || device.name,
        descriptor: preview.descriptor,
        device: device.name,
      });
      const { accounts: all, layout } = useWallet.getState();
      if (!walletHasCard([...all, account], layout, account)) await addModule({ id: `hardware:${account.id}`, size: "small" });
      await refresh();
      notify("ok", `Added ${account.name}`);
      if (direct && accountNo === "0") void lookForMore(account.name);
      else close();
    });

  // A Trezor that's been used before may have accounts 2, 3… with coins in them.
  const lookForMore = async (name: string) => {
    setMore({ name, found: null });
    setTalking(true);
    try {
      const descs = await descriptorsFrom([1, 2, 3, 4, 5]);
      const found = (await (await api()).scanDescriptors(descs)).filter((f) => f.index > 0);
      if (!found.length) return close();
      setMore({ name, found });
    } catch {
      close();
    } finally {
      setTalking(false);
    }
  };

  const addMore = async (f: FoundAccount) => {
    if (!device || !more) return;
    setAdding(f.index);
    try {
      if (!useWallet.getState().layout?.advanced) await switchMode(true);
      const a = await (
        await api()
      ).addAccount({
        kind: "hardware",
        name: `${more.name} ${f.index + 1}`,
        descriptor: f.descriptor,
        device: device.name,
      });
      await refresh();
      notify("ok", `Added ${a.name}`);
      setMore((m) => (m && m.found ? { ...m, found: m.found.filter((x) => x.index !== f.index) } : m));
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setAdding(null);
    }
  };

  if (more) {
    return (
      <Sheet
        title="Other accounts"
        left={
          <button className="text-button" onClick={close}>
            Done
          </button>
        }
      >
        {!more.found ? (
          <>
            <BitboxProgress step={bitbox} />
            <JadeProgress state={jade} />
            <p className="empty">Checking your {device?.name} for other accounts with history…</p>
          </>
        ) : more.found.length === 0 ? (
          <p className="empty">All added.</p>
        ) : (
          <>
            <p className="group-note">This {device?.name} has other accounts that have been used. Add them to keep everything in one card.</p>
            <div className="group">
              {more.found.map((f) => (
                <div className="cell" key={f.index}>
                  <span className="grow">
                    <div>Account {f.index + 1}</div>
                    <div className="row-sub num">
                      {formatSats(f.balanceSat)} sats · {f.txCount} transaction
                      {f.txCount === 1 ? "" : "s"}
                    </div>
                  </span>
                  <button className="btn small primary" disabled={adding !== null} onClick={() => void addMore(f)}>
                    {adding === f.index ? "Adding…" : "Add"}
                  </button>
                </div>
              ))}
            </div>
            {!advanced && <p className="group-note">Adding a second account turns on Advanced mode.</p>}
          </>
        )}
      </Sheet>
    );
  }

  const back = (
    <button
      className="text-button"
      onClick={() => {
        if (preview) setPreview(null);
        else {
          setDevice(null);
          setError(null);
        }
      }}
    >
      Back
    </button>
  );

  // Step 1: which device?
  if (!device) {
    return (
      <Sheet
        title="Add hardware wallet"
        left={
          <button className="text-button" onClick={close}>
            Cancel
          </button>
        }
      >
        <p className="notice">Your keys stay on the device. This wallet only sees balances and prepares transactions for the device to sign.</p>
        <div className="device-grid">
          {DEVICES.map((d) => (
            <button
              key={d.id}
              className="device-card featured"
              onClick={() => {
                setDevice(d);
                setLabel(d.name);
              }}
            >
              <DeviceIcon device={d.name} size={40} />
              <strong>{d.name}</strong>
              <span>{viaFor(d)}</span>
            </button>
          ))}
        </div>
      </Sheet>
    );
  }

  // Step 3: name it and check the address on the device.
  if (preview) {
    return (
      <Sheet
        title={device.name}
        left={back}
        right={
          <button className="text-button strong" disabled={busy} onClick={add}>
            Add
          </button>
        }
      >
        <div className="hw-hero">
          <DeviceIcon device={device.name} size={56} />
        </div>
        <p className="group-label">Label</p>
        <div className="group">
          <label className="cell">
            <span className="visually-hidden">Label</span>
            <input value={label} onChange={(e) => setLabel(e.target.value)} maxLength={32} placeholder={device.name} autoFocus />
          </label>
        </div>
        <div className="group">
          <div className="cell">
            <span className="grow">Device</span>
            <span className="value">{device.name}</span>
          </div>
          {advanced && (
            <>
              <div className="cell">
                <span className="grow">Fingerprint</span>
                <span className="value mono">{preview.fingerprint}</span>
              </div>
              <div className="cell">
                <span className="grow">Address type</span>
                <span className="value">{preview.scriptType}</span>
              </div>
            </>
          )}
        </div>
        <p className="group-label">First receive address</p>
        <div className="group">
          <div className="cell mono" style={{ wordBreak: "break-all", fontSize: 14 }}>
            {preview.firstAddress}
          </div>
        </div>
        <p className="group-note">Check this matches the first address your {device.name} shows. If it doesn't, don't use this account.</p>
        <button className="btn primary block" disabled={busy} onClick={add}>
          {busy ? "Adding…" : `Add ${label.trim() || device.name}`}
        </button>
      </Sheet>
    );
  }

  // Address type and account number, for devices we talk to directly.
  const pickers = advanced && (
    <div className="group">
      <SelectRow
        label="Address type"
        value={script}
        onChange={(v) => setScript(v as TrezorScript)}
        options={[
          { value: "wpkh", label: "Native SegWit (bc1q)" },
          { value: "tr", label: "Taproot (bc1p)" },
        ]}
      />
      <SelectRow
        label="Account"
        value={accountNo}
        onChange={setAccountNo}
        options={Array.from({ length: 10 }, (_, i) => ({
          value: String(i),
          label: `Account ${i + 1}`,
        }))}
      />
    </div>
  );

  // Step 2: connect or scan.
  return (
    <Sheet title={device.name} left={back} right={<span />}>
      <div className="hw-hero">
        <DeviceIcon device={device.name} size={56} />
      </div>

      {device.connect === "trezor" ? (
        <>
          <p className="notice">
            {isMobile()
              ? "This opens the Trezor Suite app. Unlock your Trezor there and approve sharing the wallet, and you'll be brought back here."
              : "Open Trezor Suite on this computer and unlock your Trezor. Suite will ask you to allow Sessions Wallet."}
          </p>
          {pickers}
          <button className="btn primary block" disabled={busy} onClick={fromDevice}>
            {busy ? "Waiting for Trezor…" : "Connect Trezor"}
          </button>
          {isMobile() && <p className="group-note">Needs the Trezor Suite app, version 26.8 or later.</p>}
        </>
      ) : device.connect === "ledger" ? (
        <>
          <p className="notice">
            {isMobile()
              ? "Plug your Ledger into this phone (USB-C), or turn it on with Bluetooth enabled and keep it close. Unlock it with your PIN; the wallet opens the Bitcoin app for you."
              : "Plug your Ledger into this computer, or turn it on with Bluetooth enabled and keep it close. Unlock it with your PIN; the wallet opens the Bitcoin app for you."}
          </p>
          {pickers}
          <LedgerProgress step={ledger} />
          <button className="btn primary block" disabled={busy} onClick={fromDevice}>
            {busy ? "Waiting for Ledger…" : "Connect Ledger"}
          </button>
          <p className="group-note" style={{ marginTop: 12 }}>Needs the Bitcoin app (2.1 or newer), installed with Ledger Live. Close Ledger Live while you connect.</p>
        </>
      ) : device.connect === "bitbox" ? (
        <>
          <p className="notice">
            {isMobile()
              ? "Plug your BitBox02 into this phone (USB-C, or with an OTG adapter) and allow access when asked. You'll unlock it with your password on the device."
              : "Plug your BitBox02 into this computer. You'll unlock it with your password on the device."}
          </p>
          {pickers}
          <BitboxProgress step={bitbox} />
          <button className="btn primary block" disabled={busy} onClick={fromDevice}>
            {busy ? "Waiting for BitBox02…" : "Connect BitBox02"}
          </button>
        </>
      ) : (
        <>
          {device.connect === "jade" && <JadeModeSwitch value={jadeMode} onChange={(m) => (setJadeMode(m), setError(null))} />}
          {device.connect === "jade" && jadeMode === "ble" ? (
            <>
              <p className="notice">
                {jadeLiveHint()} {isMobile() ? "Allow Bluetooth access when asked." : ""} You'll enter your PIN on Jade; the app checks it with Jade's PIN server
                automatically.
              </p>
              {pickers}
              <JadeProgress state={jade} />
              <button className="btn primary block" disabled={busy} onClick={fromDevice}>
                {busy ? "Waiting for Jade…" : "Connect Jade"}
              </button>
            </>
          ) : device.connect === "jade" && unlock ? (
            <>
              <JadeQrUnlock onDone={() => setUnlock(false)} />
              <p style={{ textAlign: "center", margin: "14px 0 0" }}>
                <button className="text-button" onClick={() => setUnlock(false)}>
                  Back to export
                </button>
              </p>
            </>
          ) : (
            <>
              <p className="notice">{device.exportHint}</p>
              <ScanButton
                className="btn primary block"
                label="Scan wallet QR"
                title={`Scan from ${device.name}`}
                hint="Hold steady. Animated codes fill in as you scan."
                onResult={(r) =>
                  exportText(r)
                    .then(inspect)
                    .catch((e) => setError(errorText(e)))
                }
              />
              {!showPaste ? (
                <p style={{ textAlign: "center", margin: "14px 0 0" }}>
                  <button className="text-button" onClick={() => setShowPaste(true)}>
                    Paste instead
                  </button>
                </p>
              ) : (
                <>
                  <p className="group-label" style={{ marginTop: 20 }}>
                    Wallet export
                  </p>
                  <div className="group">
                    <label className="cell">
                      <span className="visually-hidden">Descriptor or xpub</span>
                      <textarea
                        value={paste}
                        onChange={(e) => setPaste(e.target.value)}
                        placeholder="wpkh([fingerprint/84h/0h/0h]xpub…/<0;1>/*) or [fingerprint/84h/0h/0h]zpub…"
                        spellCheck={false}
                        autoCapitalize="off"
                        autoCorrect="off"
                      />
                    </label>
                  </div>
                  <button className="btn block" disabled={paste.trim().length < 20 || busy} onClick={() => run(() => inspect(paste))}>
                    Continue
                  </button>
                </>
              )}
              {device.connect === "jade" && (
                <p style={{ textAlign: "center", margin: "6px 0 0" }}>
                  <button className="text-button" onClick={() => setUnlock(true)}>
                    Jade locked? Unlock it with QR
                  </button>
                </p>
              )}
            </>
          )}
        </>
      )}
      {error && (
        <p className="field-error" role="alert" style={{ marginTop: 14 }}>
          <Icon name="warn" size={14} /> {error}
        </p>
      )}
    </Sheet>
  );
}
