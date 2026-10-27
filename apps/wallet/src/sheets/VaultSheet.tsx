import { useRef, useState } from "react";
import { api } from "../api";
import type { AccountInfo, KeySource, VaultImportPreview, VaultKey } from "../api/types";
import { Icon } from "../components/Icon";
import { ScanButton, type ScanResult } from "../components/Scanner";
import { Sheet, useBusy } from "../components/ui";
import { errorText } from "../format";
import { DEVICES, DeviceIcon, deviceFor } from "../hw/devices";
import { KeyPicker } from "../hw/KeyPicker";
import { useWallet } from "../state/store";
import { walletHasCard } from "../modules/AccountModule";
import { VaultExport, VaultRegister } from "./VaultPanel";

const PRESETS = [
  { m: 2, n: 3, note: "Lose any one key and still spend" },
  { m: 3, n: 5, note: "Lose any two" },
];

/** Icon for where a key lives. */
export function KeyIcon({ source }: { source: KeySource }) {
  return (
    <span className="key-icon" aria-hidden>
      {source.type === "device" ? <DeviceIcon device={source.device} size={24} /> : <Icon name={source.type === "phone" ? "key" : "people"} size={20} />}
    </span>
  );
}

export function sourceText(s: KeySource): string {
  if (s.type === "phone") return "This phone";
  if (s.type === "device") return deviceFor(s.device).id !== "other" ? deviceFor(s.device).name : s.device;
  return "Elsewhere";
}

/** M of N with up and down buttons, and what the choice means. */
function QuorumPicker({ m, n, onChange }: { m: number; n: number; onChange: (m: number, n: number) => void }) {
  const [custom, setCustom] = useState(!PRESETS.some((p) => p.m === m && p.n === n));
  return (
    <>
      <div className="quorum-cards">
        {PRESETS.map((p) => (
          <button key={`${p.m}${p.n}`} className="quorum-card" aria-pressed={!custom && p.m === m && p.n === n} onClick={() => (setCustom(false), onChange(p.m, p.n))}>
            <strong>
              {p.m} of {p.n}
            </strong>
            <span>{p.note}</span>
          </button>
        ))}
        <button className="quorum-card" aria-pressed={custom} onClick={() => setCustom(true)}>
          <strong>
            <Icon name="edit" size={20} />
          </strong>
          <span>Custom</span>
        </button>
      </div>
      {custom && (
        <div className="group">
          <div className="cell">
            <span className="grow">Signatures needed</span>
            <span className="stepper">
              <button aria-label="Fewer signatures" disabled={m <= 1} onClick={() => onChange(m - 1, n)}>
                <Icon name="minus" size={16} />
              </button>
              <b className="num">{m}</b>
              <button aria-label="More signatures" disabled={m >= n} onClick={() => onChange(m + 1, n)}>
                <Icon name="plus" size={16} />
              </button>
            </span>
          </div>
          <div className="cell">
            <span className="grow">Keys in total</span>
            <span className="stepper">
              <button aria-label="Fewer keys" disabled={n <= 2} onClick={() => onChange(Math.min(m, n - 1), n - 1)}>
                <Icon name="minus" size={16} />
              </button>
              <b className="num">{n}</b>
              <button aria-label="More keys" disabled={n >= 15} onClick={() => onChange(m, n + 1)}>
                <Icon name="plus" size={16} />
              </button>
            </span>
          </div>
        </div>
      )}
      <div className="quorum-big num">
        {m} of {n} <small>signatures to spend</small>
      </div>
      {m === 1 ? (
        <p className="notice danger" role="alert">
          <b>1 of {n} defeats the purpose of multisig.</b> Any single key can spend everything, so one stolen key is enough to lose it all. Use at least 2.
        </p>
      ) : m === n ? (
        <p className="notice">
          Every key is needed to spend. If you lose any one of them, the funds are stuck for good. Most people leave room to lose one ({n - 1} of {n}).
        </p>
      ) : (
        <p className="group-note" style={{ marginTop: 0, textAlign: "center" }}>
          Any {m} of your {n} keys can spend. You can lose {n - m} and still get your bitcoin back.
        </p>
      )}
    </>
  );
}

type Step = "start" | "quorum" | "keys" | "review" | "import" | "importReview" | "done";

/** Create a multisig vault from keys, or import one from another wallet's file or QR. */
export function VaultSheet() {
  const { close, notify, refresh, addModule } = useWallet();
  const [step, setStep] = useState<Step>("start");
  const [m, setM] = useState(2);
  const [n, setN] = useState(3);
  const [name, setName] = useState("");
  const [keys, setKeys] = useState<(VaultKey | null)[]>([]);
  const [picking, setPicking] = useState<number | null>(null);
  const [preview, setPreview] = useState<VaultImportPreview | null>(null);
  const [created, setCreated] = useState<AccountInfo | null>(null);
  const [paste, setPaste] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, run] = useBusy();
  const file = useRef<HTMLInputElement>(null);

  const setQuorum = (mm: number, nn: number) => {
    setM(mm);
    setN(nn);
  };

  const toKeys = () => {
    setKeys((old) => Array.from({ length: n }, (_, i) => old[i] ?? null));
    setStep("keys");
  };

  const finish = async (account: AccountInfo) => {
    const { accounts: all, layout } = useWallet.getState();
    if (!walletHasCard([...all, account], layout, account)) await addModule({ id: `vault:${account.id}`, size: "small" });
    await refresh();
    setCreated(account);
    setStep("done");
  };

  const create = () =>
    run(async () => {
      setError(null);
      try {
        const account = await (
          await api()
        ).vaultCreate({
          name: name.trim() || "Vault",
          threshold: m,
          keys: keys.map((k, i) => ({ key: k!.key, label: k!.label.trim() || `Key ${i + 1}`, source: k!.source })),
        });
        await finish(account);
      } catch (e) {
        setError(errorText(e));
      }
    });

  const readImport = (text: string) =>
    run(async () => {
      setError(null);
      try {
        const p = await (await api()).vaultImportPreview(text);
        setPreview(p);
        setName(p.name);
        setStep("importReview");
      } catch (e) {
        setError(errorText(e));
      }
    });

  const scannedImport = async (r: ScanResult) => {
    try {
      if (r.kind === "text") return readImport(r.text);
      const { multisigDescriptorFromUr } = await import("../hw/ur");
      return readImport(multisigDescriptorFromUr(r.type, r.cbor));
    } catch (e) {
      setError(errorText(e));
    }
  };

  const doImport = () =>
    run(async () => {
      if (!preview) return;
      setError(null);
      try {
        const account = await (
          await api()
        ).vaultImport({
          name: name.trim() || preview.name,
          descriptor: preview.descriptor,
          keys: preview.keys.map((k) => ({ fingerprint: k.fingerprint, label: k.label, source: k.source })),
        });
        await finish(account);
      } catch (e) {
        setError(errorText(e));
      }
    });

  const errorLine = error && (
    <p className="field-error" role="alert" style={{ marginTop: 14 }}>
      <Icon name="warn" size={14} /> {error}
    </p>
  );

  if (picking !== null) {
    return (
      <KeyPicker
        index={picking}
        phoneUsed={keys.some((k) => k?.source.type === "phone")}
        taken={keys.filter((k, i): k is VaultKey => !!k && i !== picking).map((k) => k.fingerprint)}
        onCancel={() => setPicking(null)}
        onPick={(k) => {
          setKeys((ks) => ks.map((x, i) => (i === picking ? k : x)));
          setPicking(null);
        }}
      />
    );
  }

  if (step === "done" && created) {
    return (
      <Sheet title={created.name}>
        <p className="notice">
          <b>Save a copy of this vault's setup.</b> To recover it you need this file (or the keys' xpubs) <i>and</i> {m} of your keys. Your encrypted wallet backup keeps it too.
        </p>
        <VaultRegister account={created.id} />
        <VaultExport account={created.id} />
        <button
          className="btn primary block"
          onClick={() => {
            notify("ok", `${created.name} is ready`);
            close();
          }}
        >
          Done
        </button>
      </Sheet>
    );
  }

  if (step === "start") {
    return (
      <Sheet
        title="Multisig vault"
        left={
          <button className="text-button" onClick={close}>
            Cancel
          </button>
        }
      >
        <p className="notice">A vault needs several keys to spend, kept in different places. A thief or a lost device on its own can't take your bitcoin.</p>
        <div className="device-grid">
          <button className="device-card featured" onClick={() => setStep("quorum")}>
            <span className="device-icon-wrap">
              <Icon name="vault" size={36} />
            </span>
            <strong>Create a vault</strong>
            <span>Pick your keys: hardware wallets, this phone, people you trust</span>
          </button>
          <button className="device-card featured" onClick={() => setStep("import")}>
            <span className="device-icon-wrap">
              <Icon name="down" size={36} />
            </span>
            <strong>Import a vault</strong>
            <span>From Sparrow, Nunchuk, Caravan, BlueWallet and others, by file or QR</span>
          </button>
        </div>
      </Sheet>
    );
  }

  if (step === "quorum") {
    return (
      <Sheet
        title="How many keys?"
        left={
          <button className="text-button" onClick={() => setStep("start")}>
            Back
          </button>
        }
      >
        <QuorumPicker m={m} n={n} onChange={setQuorum} />
        <button className="btn primary block" onClick={toKeys}>
          Choose the {n} keys
        </button>
      </Sheet>
    );
  }

  if (step === "keys") {
    const ready = keys.every(Boolean);
    return (
      <Sheet
        title={`${m} of ${n} keys`}
        left={
          <button className="text-button" onClick={() => setStep("quorum")}>
            Back
          </button>
        }
      >
        <p className="group-note" style={{ marginTop: 0 }}>
          Add each key and name it so you know where it lives. Your devices use the standard multisig path (m/48'/0'/0'/2').
        </p>
        <div className="group">
          {keys.map((k, i) =>
            k ? (
              <div className="cell key-row" key={i}>
                <KeyIcon source={k.source} />
                <span className="grow">
                  <input
                    value={k.label}
                    placeholder={`Key ${i + 1}`}
                    maxLength={32}
                    aria-label={`Name of key ${i + 1}`}
                    onChange={(e) => setKeys((ks) => ks.map((x, j) => (j === i && x ? { ...x, label: e.target.value } : x)))}
                  />
                  <span className="key-meta">
                    {sourceText(k.source) !== k.label ? `${sourceText(k.source)} · ` : ""}{k.fingerprint}
                  </span>
                </span>
                <button className="icon-button" aria-label={`Remove key ${i + 1}`} onClick={() => setKeys((ks) => ks.map((x, j) => (j === i ? null : x)))}>
                  <Icon name="close" size={16} />
                </button>
              </div>
            ) : (
              <button className="cell" key={i} onClick={() => (setError(null), setPicking(i))}>
                <span className="key-icon" aria-hidden>
                  <Icon name="plus" size={18} />
                </span>
                <span className="grow" style={{ color: "var(--accent-ink)" }}>
                  Add key {i + 1}
                </span>
                <Icon name="chevron" size={16} />
              </button>
            ),
          )}
        </div>
        <button className="btn primary block" disabled={!ready} onClick={() => setStep("review")}>
          Continue
        </button>
        {errorLine}
      </Sheet>
    );
  }

  if (step === "review") {
    return (
      <Sheet
        title="Name your vault"
        left={
          <button className="text-button" onClick={() => setStep("keys")}>
            Back
          </button>
        }
      >
        <div className="group">
          <label className="cell">
            <span className="grow">Vault name</span>
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Vault" maxLength={32} style={{ textAlign: "right" }} autoFocus />
          </label>
        </div>
        <div className="group">
          <div className="cell">
            <span className="grow">To spend</span>
            <span className="value">
              {m} of {n} keys
            </span>
          </div>
          <div className="cell">
            <span className="grow">Address type</span>
            <span className="value">Native SegWit (P2WSH)</span>
          </div>
          {keys.map((k, i) => (
            <div className="cell key-row" key={i}>
              <KeyIcon source={k!.source} />
              <span className="grow">
                {k!.label.trim() || `Key ${i + 1}`}
                <span className="key-meta">
                  {sourceText(k!.source) !== k!.label ? `${sourceText(k!.source)} · ` : ""}{k!.fingerprint}
                </span>
              </span>
            </div>
          ))}
        </div>
        {m === 1 && <p className="notice danger">1 of {n}: any single key can spend everything.</p>}
        <button className="btn primary block" disabled={busy} onClick={create}>
          {busy ? "Creating…" : "Create vault"}
        </button>
        {errorLine}
      </Sheet>
    );
  }

  if (step === "import") {
    return (
      <Sheet
        title="Import a vault"
        left={
          <button className="text-button" onClick={() => (setStep("start"), setError(null))}>
            Back
          </button>
        }
      >
        <p className="notice">
          Export the wallet from the other app (Sparrow: File › Export Wallet; Nunchuk: Wallet config › Export; Caravan: Download wallet details) and scan or open it here.
        </p>
        <ScanButton
          className="btn primary block"
          label="Scan wallet QR"
          title="Scan the vault"
          hint="Scan the wallet's descriptor or setup QR code."
          onResult={(r) => void scannedImport(r)}
        />
        <div style={{ height: 10 }} />
        <button className="btn block" onClick={() => file.current?.click()}>
          <Icon name="paste" size={16} /> Open a file
        </button>
        <input
          ref={file}
          type="file"
          accept=".txt,.json,.bsms,text/plain,application/json"
          hidden
          onChange={async (e) => {
            const f = e.target.files?.[0];
            e.target.value = "";
            if (f) readImport(await f.text());
          }}
        />
        <p className="group-label" style={{ marginTop: 22 }}>
          Or paste it
        </p>
        <div className="group">
          <label className="cell">
            <span className="visually-hidden">Wallet descriptor or file contents</span>
            <textarea
              value={paste}
              onChange={(e) => setPaste(e.target.value)}
              placeholder="wsh(sortedmulti(2,[…]xpub…)) or the file's contents"
              spellCheck={false}
              autoCapitalize="off"
            />
          </label>
        </div>
        <button className="btn block" disabled={!paste.trim() || busy} onClick={() => readImport(paste)}>
          Read
        </button>
        {errorLine}
      </Sheet>
    );
  }

  if (step === "importReview" && preview) {
    const deviceOptions = DEVICES.map((d) => d.name);
    return (
      <Sheet
        title="Import vault"
        left={
          <button className="text-button" onClick={() => setStep("import")}>
            Back
          </button>
        }
      >
        <div className="group">
          <label className="cell">
            <span className="grow">Vault name</span>
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Vault" maxLength={32} style={{ textAlign: "right" }} />
          </label>
          <div className="cell">
            <span className="grow">To spend</span>
            <span className="value">
              {preview.threshold} of {preview.total} keys
            </span>
          </div>
          <div className="cell">
            <span className="grow">Address type</span>
            <span className="value">{preview.script}</span>
          </div>
        </div>
        <p className="group-label">Keys</p>
        <div className="group">
          {preview.keys.map((k, i) => (
            <div className="cell key-row" key={k.fingerprint}>
              <KeyIcon source={k.source} />
              <span className="grow">
                <input
                  value={k.label}
                  maxLength={32}
                  aria-label={`Name of key ${i + 1}`}
                  onChange={(e) => setPreview({ ...preview, keys: preview.keys.map((x, j) => (j === i ? { ...x, label: e.target.value } : x)) })}
                />
                <span className="key-meta">{k.fingerprint}</span>
              </span>
              {k.ours ? (
                <span className="value">This phone</span>
              ) : (
                <select
                  aria-label={`Where key ${i + 1} lives`}
                  value={k.source.type === "device" ? k.source.device : ""}
                  onChange={(e) => {
                    const v = e.target.value;
                    const source: KeySource = v ? { type: "device", device: v } : { type: "external" };
                    setPreview({ ...preview, keys: preview.keys.map((x, j) => (j === i ? { ...x, source } : x)) });
                  }}
                >
                  <option value="">Elsewhere</option>
                  {deviceOptions.map((d) => (
                    <option key={d} value={d}>
                      {d}
                    </option>
                  ))}
                </select>
              )}
            </div>
          ))}
        </div>
        {preview.notes.map((note) => (
          <p className="group-note" key={note}>
            {note}
          </p>
        ))}
        <button className="btn primary block" disabled={busy} onClick={doImport}>
          {busy ? "Adding…" : "Add vault"}
        </button>
        {errorLine}
      </Sheet>
    );
  }

  return null;
}
