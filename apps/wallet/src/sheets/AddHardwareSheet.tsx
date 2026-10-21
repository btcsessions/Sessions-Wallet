import { useState } from "react";
import { api } from "../api";
import type { DescriptorPreview, FoundAccount } from "../api/types";
import { Icon } from "../components/Icon";
import { ScanButton, type ScanResult } from "../components/Scanner";
import { SelectRow, Sheet, useBusy } from "../components/ui";
import { DEVICES, DeviceIcon, type DeviceDef } from "../hw/devices";
import { isMobile, trezorDescriptor, trezorDescriptors, type TrezorScript } from "../hw/trezor";
import { errorText, formatSats } from "../format";
import { switchMode } from "../state/mode";
import { useWallet } from "../state/store";
import { walletHasCard } from "../modules/AccountModule";

/** Turn whatever the device showed into text the import understands. */
async function exportText(r: ScanResult): Promise<string> {
  if (r.kind === "text") return r.text;
  const { descriptorFromUr } = await import("../hw/ur");
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
  const [more, setMore] = useState<{ name: string; found: FoundAccount[] | null } | null>(null);
  const [adding, setAdding] = useState<number | null>(null);

  const inspect = async (text: string) => {
    setError(null);
    try {
      const p = await (await api()).inspectDescriptor(text);
      setPreview(p);
    } catch (e) {
      setError(errorText(e));
    }
  };

  const fromTrezor = () =>
    run(async () => {
      setError(null);
      try {
        await inspect(await trezorDescriptor(script, false, Number(accountNo)));
      } catch (e) {
        setError(errorText(e));
      }
    });

  const add = () =>
    run(async () => {
      if (!device || !preview) return;
      const account = await (await api()).addAccount({
        kind: "hardware",
        name: label.trim() || device.name,
        descriptor: preview.descriptor,
        device: device.name,
      });
      const { accounts: all, layout } = useWallet.getState();
      if (!walletHasCard([...all, account], layout, account)) await addModule({ id: `hardware:${account.id}`, size: "small" });
      await refresh();
      notify("ok", `Added ${account.name}`);
      if (device.connect === "trezor" && accountNo === "0") void lookForMore(account.name);
      else close();
    });

  // A Trezor that's been used before may have accounts 2, 3… with coins in them.
  const lookForMore = async (name: string) => {
    setMore({ name, found: null });
    try {
      const descs = await trezorDescriptors(script, [1, 2, 3, 4, 5]);
      const found = (await (await api()).scanDescriptors(descs)).filter((f) => f.index > 0);
      if (!found.length) return close();
      setMore({ name, found });
    } catch {
      close();
    }
  };

  const addMore = async (f: FoundAccount) => {
    if (!device || !more) return;
    setAdding(f.index);
    try {
      if (!useWallet.getState().layout?.advanced) await switchMode(true);
      const a = await (await api()).addAccount({ kind: "hardware", name: `${more.name} ${f.index + 1}`, descriptor: f.descriptor, device: device.name });
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
          <p className="empty">Checking your {device?.name} for other accounts with history…</p>
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
                      {formatSats(f.balanceSat)} sats · {f.txCount} transaction{f.txCount === 1 ? "" : "s"}
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
    const [featured, others] = [DEVICES.filter((d) => d.connect !== "import"), DEVICES.filter((d) => d.connect === "import")];
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
          {featured.map((d) => (
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
              <span>{d.connect === "trezor" ? "USB or Bluetooth, via Trezor Suite" : "Air-gapped, by QR code"}</span>
            </button>
          ))}
        </div>
        <p className="group-label">Other devices</p>
        <div className="device-grid small">
          {others.map((d) => (
            <button
              key={d.id}
              className="device-card"
              onClick={() => {
                setDevice(d);
                setLabel(d.id === "other" ? "Cold storage" : d.name);
              }}
            >
              <DeviceIcon device={d.name} size={28} />
              <span>{d.name}</span>
            </button>
          ))}
        </div>
        <p className="group-note">These import by QR code, file or copy-paste and sign by QR or PSBT file. Direct support is coming.</p>
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
          {advanced && (
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
              options={Array.from({ length: 10 }, (_, i) => ({ value: String(i), label: `Account ${i + 1}` }))}
            />
          </div>
          )}
          <button className="btn primary block" disabled={busy} onClick={fromTrezor}>
            {busy ? "Waiting for Trezor…" : "Connect Trezor"}
          </button>
          {isMobile() && <p className="group-note">Needs the Trezor Suite app, version 26.8 or later.</p>}
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
