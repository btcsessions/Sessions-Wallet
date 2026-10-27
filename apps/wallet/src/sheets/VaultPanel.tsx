import { useEffect, useMemo, useState } from "react";
import { api } from "../api";
import type { ExportFile, Proposal, VaultDetails, VaultExportFormat } from "../api/types";
import { Icon } from "../components/Icon";
import { CopyButton, Sheet, useBusy } from "../components/ui";
import { errorText, formatSats } from "../format";
import { deviceFor } from "../hw/devices";
import { useWallet } from "../state/store";
import { KeyIcon, sourceText } from "./VaultSheet";

const FORMATS: { id: VaultExportFormat; label: string; apps: string }[] = [
  { id: "descriptor", label: "Output descriptor", apps: "Sparrow, Nunchuk, BlueWallet, Specter, Bitcoin Core" },
  { id: "coldcard", label: "Multisig setup file", apps: "Keystone, Passport, Jade, BlueWallet, Nunchuk, Sparrow" },
  { id: "bsms", label: "BSMS (BIP-129)", apps: "Nunchuk, Sparrow" },
  { id: "caravan", label: "Caravan JSON", apps: "Caravan, Unchained" },
  { id: "specter", label: "Specter JSON", apps: "Specter Desktop" },
];

function useVault(account: string) {
  const [details, setDetails] = useState<VaultDetails | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = () =>
    api()
      .then((a) => a.vaultDetails(account))
      .then(setDetails)
      .catch((e) => setError(errorText(e)));
  useEffect(() => {
    void load();
  }, [account]); // eslint-disable-line react-hooks/exhaustive-deps
  return { details, error, reload: load };
}

/** An animated QR of the vault, in the format the scanning wallet reads. */
function VaultQR({ details, account, initial = "output" }: { details: VaultDetails; account: string; initial?: "output" | "file" }) {
  const [kind, setKind] = useState<"output" | "file">(initial);
  const [file, setFile] = useState<ExportFile | null>(null);
  const [frame, setFrame] = useState<string>("");
  const [svg, setSvg] = useState("");

  useEffect(() => {
    if (kind !== "file" || file) return;
    void api()
      .then((a) => a.vaultExport(account, "coldcard"))
      .then(setFile)
      .catch((e) => setQrError(errorText(e)));
  }, [kind, file, account]);

  const [qrError, setQrError] = useState<string | null>(null);
  const frames = useMemo(() => {
    let gen: { next: () => string; count: number } | null = null;
    setQrError(null);
    return import("../hw/ur")
      .then((ur) => {
        gen =
          kind === "output"
            ? ur.multisigOutputFrames(
                details.threshold,
                details.keys.map((k) => ({ fingerprint: k.fingerprint, path: k.path, xpub: k.xpub })),
                details.script,
                details.keys.some((k) => k.xpub.startsWith("tpub")),
                details.sorted,
              )
            : file
              ? ur.textFrames(file.content)
              : null;
        return gen;
      })
      .catch((e) => {
        setQrError(errorText(e));
        return null;
      });
  }, [kind, details, file]);

  useEffect(() => {
    let alive = true;
    let t: ReturnType<typeof setInterval> | undefined;
    void frames.then((g) => {
      if (!alive || !g) return;
      setFrame(g.next());
      if (g.count > 1) t = setInterval(() => setFrame(g.next()), 450);
    });
    return () => {
      alive = false;
      if (t) clearInterval(t);
    };
  }, [frames]);

  useEffect(() => {
    if (!frame) return;
    void import("qrcode").then((QR) =>
      QR.toString(frame, { type: "svg", margin: 0, errorCorrectionLevel: "L", color: { dark: "#000000", light: "#ffffff" } })
        .then(setSvg)
        .catch(() => setSvg("")),
    );
  }, [frame]);

  return (
    <>
      <div className="segmented" role="radiogroup" aria-label="QR format" style={{ marginBottom: 10 }}>
        <button role="radio" aria-checked={kind === "output"} aria-pressed={kind === "output"} onClick={() => setKind("output")}>
          Descriptor
        </button>
        <button role="radio" aria-checked={kind === "file"} aria-pressed={kind === "file"} onClick={() => setKind("file")}>
          Setup file
        </button>
      </div>
      {qrError ? (
        <p className="field-error">{qrError}</p>
      ) : (
        <div className="qr-wrap">
          <div className="qr" dangerouslySetInnerHTML={{ __html: svg }} />
        </div>
      )}
      <p className="group-note" style={{ textAlign: "center" }}>
        {kind === "output"
          ? "For Sparrow, SeedSigner, Nunchuk and Keystone (UR output descriptor)."
          : "For Jade, Keystone, Passport and BlueWallet (the multisig setup file as UR bytes)."}
      </p>
    </>
  );
}

/** Save, share or show the vault's setup in formats other wallets open. */
export function VaultExport({ account }: { account: string }) {
  const [format, setFormat] = useState<VaultExportFormat>("descriptor");
  const [qr, setQr] = useState(false);
  const { details } = useVault(account);
  const [busy, run] = useBusy();
  const notify = useWallet((s) => s.notify);
  const [file, setFile] = useState<ExportFile | null>(null);

  useEffect(() => {
    setFile(null);
    void api()
      .then((a) => a.vaultExport(account, format))
      .then(setFile)
      .catch((e) => notify("error", errorText(e)));
  }, [account, format]); // eslint-disable-line react-hooks/exhaustive-deps

  const out = (share: boolean) =>
    run(async () => {
      if (!file) return;
      try {
        const done = await (await api()).fileExport(file, share);
        if (done && !share) notify("ok", `Saved ${file.filename}`);
      } catch (e) {
        notify("error", errorText(e));
      }
    });

  return (
    <>
      <p className="group-label">Export vault</p>
      <div className="segmented" role="radiogroup" aria-label="How" style={{ marginBottom: 12 }}>
        <button role="radio" aria-checked={!qr} aria-pressed={!qr} onClick={() => setQr(false)}>
          File
        </button>
        <button role="radio" aria-checked={qr} aria-pressed={qr} onClick={() => setQr(true)}>
          QR code
        </button>
      </div>
      {qr ? (
        details && <VaultQR details={details} account={account} />
      ) : (
        <>
          <div className="group">
            {FORMATS.map((f) => (
              <button key={f.id} className={`cell${format === f.id ? " selected" : ""}`} onClick={() => setFormat(f.id)} aria-pressed={format === f.id}>
                <span className="grow">
                  {f.label}
                  <span className="key-meta">{f.apps}</span>
                </span>
                {format === f.id && <Icon name="check" size={18} />}
              </button>
            ))}
          </div>
          <div style={{ display: "flex", gap: 10, marginBottom: 22 }}>
            <button className="btn block" disabled={!file || busy} onClick={() => out(false)}>
              Save file
            </button>
            <button className="btn block" disabled={!file || busy} onClick={() => out(true)}>
              Share
            </button>
            {file && <CopyButton text={file.content} label="Copy" />}
          </div>
        </>
      )}
    </>
  );
}

/** BitBox02 and Jade must know the vault before they'll sign or show its addresses. */
export function VaultRegister({ account }: { account: string }) {
  const { details } = useVault(account);
  const [done, setDone] = useState<Record<string, boolean>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const notify = useWallet((s) => s.notify);
  const [showQr, setShowQr] = useState(false);
  const devices = (details?.keys ?? []).filter((k) => k.source.type === "device" && ["bitbox", "jade"].includes(deviceFor(k.source.device).connect));
  // Passport Prime learns the vault by scanning its setup file.
  const prime = (details?.keys ?? []).filter((k) => k.source.type === "device" && deviceFor(k.source.device).id === "passport-prime");
  if (!devices.length && !prime.length) return null;
  const register = async (fp: string, connect: string) => {
    setBusy(fp);
    try {
      const a = await api();
      await (connect === "bitbox" ? a.bitboxRegisterMultisig(account) : a.jadeRegisterMultisig(account));
      setDone((d) => ({ ...d, [fp]: true }));
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setBusy(null);
    }
  };
  return (
    <>
      <p className="group-label">Register on your devices</p>
      <div className="group">
        {devices.map((k) => {
          const connect = k.source.type === "device" ? deviceFor(k.source.device).connect : "";
          return (
            <div className="cell key-row" key={k.fingerprint}>
              <KeyIcon source={k.source} />
              <span className="grow">{k.label}</span>
              {done[k.fingerprint] ? (
                <Icon name="check" size={18} />
              ) : (
                <button className="btn small" disabled={busy !== null} onClick={() => void register(k.fingerprint, connect)}>
                  {busy === k.fingerprint ? "Confirm on device…" : "Register"}
                </button>
              )}
            </div>
          );
        })}
        {prime.map((k) => (
          <div className="cell key-row" key={k.xpub}>
            <KeyIcon source={k.source} />
            <span className="grow">{k.label}</span>
            <button className="btn small" onClick={() => setShowQr(!showQr)}>
              {showQr ? "Hide QR" : "Show QR"}
            </button>
          </div>
        ))}
      </div>
      {showQr && details && (
        <>
          <p className="notice">On Prime: open Bitcoin Wallet, tap ⋯ on the Accounts page, choose Import Multisig and scan this. Check the details on Prime and give it a name.</p>
          <VaultQR details={details} account={account} initial="file" />
        </>
      )}
      <p className="group-note">So the device can check change and show vault addresses. You confirm the vault on its screen once. It's also done the first time it signs.</p>
    </>
  );
}

/** Account sheet: the vault's keys (with labels), proposals waiting for signatures, export. */
export function VaultPanel({ account }: { account: string }) {
  const { details, error, reload } = useVault(account);
  const [editing, setEditing] = useState<string | null>(null);
  const [label, setLabel] = useState("");
  const [proposals, setProposals] = useState<Proposal[]>([]);
  const [verify, setVerify] = useState<string | null>(null);
  const notify = useWallet((s) => s.notify);
  const open = useWallet((s) => s.open);

  useEffect(() => {
    void api()
      .then((a) => a.proposals(account))
      .then(setProposals)
      .catch(() => {});
  }, [account]);

  if (error) return <p className="field-error">{error}</p>;
  if (!details) return null;

  const saveLabel = async (xpub: string) => {
    try {
      await (await api()).vaultSetKeyLabel(account, xpub, label);
      setEditing(null);
      await reload();
    } catch (e) {
      notify("error", errorText(e));
    }
  };

  const showOnDevice = async (fp: string, connect: string) => {
    setVerify(fp);
    try {
      const a = await api();
      let addr: string;
      if (connect === "bitbox") addr = await a.bitboxShowMultisigAddress(account, 0);
      else if (connect === "jade") addr = await a.jadeShowMultisigAddress(account, 0);
      else {
        const { trezorShowMultisigAddress } = await import("../hw/trezor");
        addr = await trezorShowMultisigAddress(account, 0);
      }
      notify("ok", `Matches: ${addr.slice(0, 10)}…${addr.slice(-6)}`);
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setVerify(null);
    }
  };

  return (
    <>
      {proposals.length > 0 && (
        <>
          <p className="group-label">Waiting for signatures</p>
          <div className="group">
            {proposals.map((p) => (
              <button key={p.id} className="cell" onClick={() => open({ name: "proposal", id: p.id })}>
                <span className="grow">
                  {formatSats(p.amountSat)} sats{p.to ? ` to ${p.to.length > 24 ? `${p.to.slice(0, 12)}…${p.to.slice(-6)}` : p.to}` : ""}
                  <span className="key-meta">Saved {new Date(p.updatedAt * 1000).toLocaleDateString()}</span>
                </span>
                <Icon name="chevron" size={16} />
              </button>
            ))}
          </div>
        </>
      )}

      <p className="group-label">
        Keys · {details.threshold} of {details.total} to spend
      </p>
      <div className="group">
        {details.keys.map((k) => {
          const connect = k.source.type === "device" ? deviceFor(k.source.device).connect : null;
          return (
            <div className="cell key-row" key={k.fingerprint}>
              <KeyIcon source={k.source} />
              <span className="grow">
                {editing === k.xpub ? (
                  <input value={label} onChange={(e) => setLabel(e.target.value)} maxLength={32} autoFocus onKeyDown={(e) => e.key === "Enter" && void saveLabel(k.xpub)} />
                ) : (
                  k.label
                )}
                <span className="key-meta">
                  {sourceText(k.source) !== k.label ? `${sourceText(k.source)} · ` : ""}
                  {k.fingerprint}
                </span>
              </span>
              {editing === k.xpub ? (
                <button className="btn small" onClick={() => void saveLabel(k.xpub)}>
                  Save
                </button>
              ) : (
                <>
                  {connect && connect !== "qr" && (
                    <button
                      className="icon-button"
                      aria-label={`Check an address on ${k.label}`}
                      disabled={verify !== null}
                      onClick={() => void showOnDevice(k.fingerprint, connect)}
                    >
                      <Icon name={verify === k.fingerprint ? "clock" : "eye"} size={18} />
                    </button>
                  )}
                  <button className="icon-button" aria-label={`Rename ${k.label}`} onClick={() => (setEditing(k.xpub), setLabel(k.label))}>
                    <Icon name="edit" size={18} />
                  </button>
                </>
              )}
            </div>
          );
        })}
      </div>
      <p className="group-note">{details.script}. Tap the eye to check a vault address on that device.</p>
      <button className="btn block" style={{ marginBottom: 22 }} onClick={() => open({ name: "vault-export", account })}>
        <Icon name="up" size={16} /> Export vault (file or QR)
      </button>
    </>
  );
}

export function VaultExportSheet({ account }: { account: string }) {
  const name = useWallet((s) => s.accounts.find((a) => a.id === account)?.name ?? "vault");
  const open = useWallet((s) => s.open);
  return (
    <Sheet
      title={`Export ${name}`}
      left={
        <button className="text-button" onClick={() => open({ name: "account", id: account })}>
          Back
        </button>
      }
    >
      <p className="group-note" style={{ marginTop: 0 }}>
        This file holds the vault's public keys: anyone with it can see your balance and history, but not spend. Keep it with your backups.
      </p>
      <VaultExport account={account} />
      <VaultRegister account={account} />
    </Sheet>
  );
}
