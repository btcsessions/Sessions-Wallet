import { useEffect, useState } from "react";
import { api } from "../api";
import type { NewAccount } from "../api/types";
import { Icon } from "../components/Icon";
import { CopyButton, QR, SelectRow, Sheet, useBusy } from "../components/ui";
import { useWallet } from "../state/store";
import { ScanButton, scanKey } from "../components/Scanner";
import { errorText } from "../format";

type Kind = "multisig" | "hardware" | "watchOnly" | "ecash" | "savings";

/** Popular mints, checked live. Minibits is the default. */
const MINTS = [
  { value: "https://mint.minibits.cash/Bitcoin", label: "Minibits" },
  { value: "https://mint.coinos.io", label: "Coinos" },
  { value: "https://mint.macadamia.cash", label: "macadamia" },
  { value: "https://mint.cubabitcoin.org", label: "Cuba Bitcoin" },
  { value: "custom", label: "Other mint…" },
];

const TITLES: Record<Kind, string> = {
  multisig: "New vault",
  hardware: "Add hardware wallet",
  watchOnly: "Watch a wallet",
  ecash: "Add ecash",
  savings: "Hot on-chain wallet",
};

const MODULE_FOR: Record<Kind, string> = {
  multisig: "vault",
  hardware: "hardware",
  watchOnly: "watch",
  ecash: "ecash",
  savings: "savings",
};

export function AddAccountSheet({ kind }: { kind: Kind }) {
  const { addModule, refresh, notify, close } = useWallet();
  const [busy, run] = useBusy();
  const [name, setName] = useState(
    kind === "multisig" ? "Vault" : kind === "ecash" ? "Ecash" : kind === "hardware" ? "Cold storage" : kind === "savings" ? "On-Chain" : "Watching",
  );
  const [mintChoice, setMintChoice] = useState(MINTS[0].value);
  const [understood, setUnderstood] = useState(false);
  const [descriptor, setDescriptor] = useState("");
  const [customMint, setCustomMint] = useState("https://");
  const mintUrl = mintChoice === "custom" ? customMint : mintChoice;
  const [threshold, setThreshold] = useState(2);
  const [cosigners, setCosigners] = useState<string[]>(["", ""]);
  const [ourKey, setOurKey] = useState<string | null>(null);

  useEffect(() => {
    if (kind === "multisig") {
      api()
        .then((a) => a.cosignerKey())
        .then(setOurKey)
        .catch(() => setOurKey(null));
    }
  }, [kind]);

  const total = cosigners.length + 1;
  const valid =
    name.trim() &&
    (kind === "savings"
      ? understood
      : kind === "ecash"
      ? /^https:\/\/[^/]+\.[^/]+/.test(mintUrl)
      : kind === "multisig"
        ? cosigners.every((c) => c.trim().startsWith("[")) && threshold >= 1 && threshold <= total
        : descriptor.trim().length > 20);

  const create = () =>
    run(async () => {
      const spec: NewAccount =
        kind === "multisig"
          ? { kind, name: name.trim(), threshold, cosigners: cosigners.map((c) => c.trim()) }
          : kind === "ecash"
            ? { kind, name: name.trim(), mintUrl: mintUrl.trim() }
            : kind === "savings"
            ? { kind, name: name.trim() }
            : { kind, name: name.trim(), descriptor: descriptor.trim() };
      const account = await (await api()).addAccount(spec);
      await addModule({ id: `${MODULE_FOR[kind]}:${account.id}`, size: "small" });
      await refresh();
      notify("ok", `Added ${account.name}`);
      close();
    });

  return (
    <Sheet
      title={TITLES[kind]}
      left={
        <button className="text-button" onClick={close}>
          Cancel
        </button>
      }
      right={
        <button className="text-button strong" disabled={!valid || busy} onClick={create}>
          Add
        </button>
      }
    >
      <p className="group-label">Name</p>
      <div className="group">
        <label className="cell">
          <span className="visually-hidden">Name</span>
          <input value={name} onChange={(e) => setName(e.target.value)} maxLength={32} />
        </label>
      </div>

      {kind === "ecash" && (
        <>
          <div className="group">
            <SelectRow label="Mint" value={mintChoice} onChange={setMintChoice} options={MINTS} />
            {mintChoice === "custom" && (
              <label className="cell">
                <span className="visually-hidden">Mint URL</span>
                <input value={customMint} onChange={(e) => setCustomMint(e.target.value)} inputMode="url" autoCapitalize="off" spellCheck={false} autoFocus />
              </label>
            )}
          </div>
          <p className="notice">
            <strong>The mint holds these sats for you.</strong> Ecash is private and instant, but if the mint disappears so does the money. Keep amounts
            small and use mints you trust.
          </p>
        </>
      )}

      {kind === "savings" && (
        <>
          <p className="notice">
            <strong>The keys live on this phone.</strong> Good for small, everyday on-chain use. For savings, a hardware wallet or a multisig vault is
            safer: a lost or hacked phone can't spend from those.
          </p>
          <label className="cell" style={{ padding: "4px 0 18px" }}>
            <input type="checkbox" checked={understood} onChange={(e) => setUnderstood(e.target.checked)} style={{ flex: "none", width: 22, height: 22, accentColor: "var(--accent)" }} />
            <span>I understand, add a hot on-chain wallet</span>
          </label>
        </>
      )}

      {(kind === "hardware" || kind === "watchOnly") && (
        <>
          <p className="group-label">Wallet descriptor</p>
          <div className="group">
            <label className="cell">
              <span className="visually-hidden">Descriptor</span>
              <textarea
                value={descriptor}
                onChange={(e) => setDescriptor(e.target.value)}
                placeholder="wpkh([a1b2c3d4/84'/0'/0']xpub…/<0;1>/*)"
                spellCheck={false}
                autoCapitalize="off"
              />
            </label>
          </div>
          <div style={{ display: "flex", gap: 10, marginBottom: 8 }}>
            <ScanButton
              title="Scan wallet export"
              onResult={(r) =>
                scanKey(r, "single")
                  .then(setDescriptor)
                  .catch((e) => notify("error", errorText(e)))
              }
            />
          </div>
          <p className="group-note">Paste or scan a descriptor or a [fingerprint/path]xpub. This account can see the balance but can't spend.</p>
        </>
      )}

      {kind === "multisig" && (
        <>
          <p className="group-label">Signatures needed to spend</p>
          <div className="segmented">
            {Array.from({ length: total }, (_, i) => i + 1).map((n) => (
              <button key={n} aria-pressed={n === threshold} onClick={() => setThreshold(n)}>
                {n} of {total}
              </button>
            ))}
          </div>

          <p className="group-label">This device's key</p>
          {ourKey ? (
            <>
              <div className="qr-wrap" style={{ paddingBottom: 8 }}>
                <QR value={ourKey} />
              </div>
              <p className="request-text">{ourKey}</p>
              <div style={{ display: "flex", justifyContent: "center", marginBottom: 22 }}>
                <CopyButton text={ourKey} label="Copy key" />
              </div>
            </>
          ) : (
            <p className="empty">Loading…</p>
          )}

          <p className="group-label">Other keys</p>
          <div className="group">
            {cosigners.map((c, i) => (
              <label className="cell" key={i}>
                <span className="row-icon" style={{ width: 28, height: 28 }}>
                  <Icon name="key" size={14} />
                </span>
                <input
                  value={c}
                  placeholder="[fingerprint/48'/0'/0'/2']xpub…"
                  spellCheck={false}
                  autoCapitalize="off"
                  onChange={(e) => setCosigners(cosigners.map((x, j) => (j === i ? e.target.value : x)))}
                />
                <ScanButton
                  className="icon-button"
                  label=""
                  title="Scan cosigner key"
                  hint="On the device, export its multisig key (BIP48, P2WSH)."
                  onResult={(r) =>
                    scanKey(r, "multisig")
                      .then((k) => setCosigners((cs) => cs.map((x, j) => (j === i ? k : x))))
                      .catch((e) => notify("error", errorText(e)))
                  }
                />
                {cosigners.length > 1 && (
                  <button className="icon-button" aria-label="Remove key" onClick={(e) => (e.preventDefault(), setCosigners(cosigners.filter((_, j) => j !== i)))}>
                    <Icon name="minus" size={16} />
                  </button>
                )}
              </label>
            ))}
            {cosigners.length < 14 && (
              <button className="cell" style={{ color: "var(--accent-ink)" }} onClick={() => setCosigners([...cosigners, ""])}>
                <Icon name="plus" size={16} />
                Add another key
              </button>
            )}
          </div>
          <p className="group-note">
            Use keys from separate hardware wallets kept in different places. Export each one as a multisig (BIP48, P2WSH) key. Back up the vault's
            descriptor too: you need it, plus enough keys, to recover.
          </p>
        </>
      )}

      <button className="btn primary block" disabled={!valid || busy} onClick={create}>
        {busy ? "Adding…" : `Add ${TITLES[kind].replace(/^(New|Add) /, "").toLowerCase()}`}
      </button>
    </Sheet>
  );
}
