import { useEffect, useState } from "react";
import { api } from "../api";
import type { Execution, FeePriority, Proposal, VaultSignatures } from "../api/types";
import { AnimatedPsbtQR } from "../components/AnimatedQR";
import { Icon } from "../components/Icon";
import { ScanButton, type ScanResult } from "../components/Scanner";
import { CopyButton, Sheet, useBusy } from "../components/ui";
import { errorText, formatSats } from "../format";
import { BitboxProgress, useBitboxProgress } from "../hw/BitboxProgress";
import { deviceFor } from "../hw/devices";
import { JadeModeSwitch, JadeProgress, useJadeProgress, type JadeMode } from "../hw/JadeProgress";
import { isMobile } from "../hw/trezor";
import { useWallet } from "../state/store";
import { KeyIcon, sourceText } from "./VaultSheet";

async function psbtFromScan(r: ScanResult): Promise<string> {
  if (r.kind === "ur") {
    const { psbtFromUr } = await import("../hw/ur");
    const { Buffer } = await import("buffer");
    return psbtFromUr(r.type, Buffer.from(r.cbor));
  }
  return r.text.trim();
}

type Signer = VaultSignatures["keys"][number];

/** Sign with one key: its device directly, by QR, or by file / copy-paste. */
function SignWith({ signer, account, psbt, onSigned, onBack }: { signer: Signer; account: string; psbt: string; onSigned: (signed: string) => Promise<void>; onBack: () => void }) {
  const def = signer.source.type === "device" ? deviceFor(signer.source.device) : null;
  const [mode, setMode] = useState<JadeMode>("ble");
  const [talking, setTalking] = useState(false);
  const bitbox = useBitboxProgress(talking && def?.connect === "bitbox");
  const jade = useJadeProgress(talking && def?.connect === "jade");
  const [pasted, setPasted] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, run] = useBusy();
  const notify = useWallet((s) => s.notify);
  const direct = def && (def.connect === "bitbox" || def.connect === "trezor" || (def.connect === "jade" && mode === "ble"));

  const take = (signed: string) =>
    run(async () => {
      setError(null);
      try {
        await onSigned(signed);
      } catch (e) {
        setError(errorText(e));
      }
    });

  const sign = () =>
    run(async () => {
      setError(null);
      setTalking(true);
      try {
        const a = await api();
        let signed: string;
        if (def?.connect === "bitbox") signed = await a.bitboxSign(psbt, account);
        else if (def?.connect === "jade") signed = await a.jadeSign(psbt, account);
        else {
          const { trezorMultisigSign } = await import("../hw/trezor");
          signed = await trezorMultisigSign(account, psbt);
        }
        await onSigned(signed);
      } catch (e) {
        setError(errorText(e));
      } finally {
        setTalking(false);
      }
    });

  const shareFile = (share: boolean) =>
    run(async () => {
      try {
        await (await api()).fileExport({ filename: "vault-spend.psbt", mime: "application/octet-stream", content: psbt }, share);
      } catch (e) {
        notify("error", errorText(e));
      }
    });

  return (
    <Sheet
      title={`Sign with ${signer.label}`}
      left={
        <button className="text-button" onClick={onBack}>
          Back
        </button>
      }
    >
      {def?.connect === "jade" && <JadeModeSwitch value={mode} onChange={(m) => (setMode(m), setError(null))} />}
      {direct ? (
        <>
          <p className="notice">
            {def.connect === "bitbox"
              ? "Plug in the BitBox02 and unlock it. The first time, it asks you to confirm the vault; then check the amount and address and confirm."
              : def.connect === "jade"
                ? "Turn on Jade and choose Bluetooth. Enter your PIN if asked. The first time, confirm the vault on Jade; then check the amount and address."
                : isMobile()
                  ? "This opens Trezor Suite. Check the amount and address on the Trezor and confirm."
                  : "Keep Trezor Suite open with the Trezor unlocked. Check the amount and address on its screen and confirm."}
          </p>
          {def.connect === "bitbox" && <BitboxProgress step={bitbox} signing />}
          {def.connect === "jade" && <JadeProgress state={jade} signing />}
          <button className="btn primary block" disabled={busy} onClick={sign}>
            {busy ? `Waiting for ${def.name}…` : `Sign with ${def.name}`}
          </button>
        </>
      ) : (
        <>
          <p className="notice">
            {signer.source.type === "external"
              ? `Send this to whoever holds ${signer.label}: show them the QR, or share the PSBT file. Then scan or paste what they send back.`
              : `Scan this with ${signer.label}, check the amount and address on its screen, then scan the signed transaction it shows.`}
          </p>
          <div className="qr-wrap">
            <AnimatedPsbtQR psbt={psbt} adjustable />
          </div>
          <ScanButton
            className="btn primary block"
            label="Scan signed transaction"
            title="Scan signed transaction"
            hint={`Scan the QR ${signer.label} shows after signing.`}
            onResult={(r) =>
              psbtFromScan(r)
                .then((p) => take(p))
                .catch((e) => setError(errorText(e)))
            }
          />
          <div style={{ display: "flex", gap: 10, margin: "12px 0" }}>
            <button className="btn block" disabled={busy} onClick={() => shareFile(true)}>
              Share PSBT
            </button>
            <button className="btn block" disabled={busy} onClick={() => shareFile(false)}>
              Save PSBT
            </button>
            <CopyButton text={psbt} label="Copy" />
          </div>
          <div className="group">
            <label className="cell">
              <span className="visually-hidden">Signed PSBT</span>
              <textarea value={pasted} onChange={(e) => setPasted(e.target.value)} placeholder="Paste the signed PSBT" spellCheck={false} />
            </label>
          </div>
          <button className="btn block" disabled={!pasted.trim() || busy} onClick={() => take(pasted.trim())}>
            Add signature
          </button>
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

/**
 * Collect a vault spend's signatures, one key at a time, then send. Can be saved and finished later (a
 * "proposal"); group wallets will share these with the other members.
 */
export function VaultSign({ account, paused, fee, proposal, onDone }: { account: string; paused: Execution; fee: FeePriority; proposal?: Proposal; onDone: () => void }) {
  const { refresh, notify, close } = useWallet();
  const quote = paused.awaitingSignature!;
  const [psbt, setPsbt] = useState(proposal?.psbt ?? quote.needsSignature!.psbtBase64);
  const [sigs, setSigs] = useState<VaultSignatures | null>(null);
  const [signing, setSigning] = useState<Signer | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, run] = useBusy();
  const [proposalId, setProposalId] = useState(proposal?.id ?? "");
  const dest = sigs?.destinations ?? [];
  const to = dest.length === 1 ? dest[0].address : dest.length > 1 ? `${dest.length} recipients` : "";

  useEffect(() => {
    let alive = true;
    void api()
      .then((a) => a.vaultSignatures(account, psbt))
      .then((s) => alive && setSigs(s))
      .catch((e) => alive && setError(errorText(e)));
    return () => {
      alive = false;
    };
  }, [account, psbt]);

  const save = async (p: string): Promise<string> => {
    const saved = await (
      await api()
    ).proposalSave({
      id: proposalId,
      account,
      createdAt: proposal?.createdAt ?? 0,
      updatedAt: 0,
      psbt: p,
      execution: paused,
      fee,
      amountSat: quote.amountSat,
      feeSat: quote.feeSat,
      to,
      note: proposal?.note ?? null,
    });
    setProposalId(saved.id);
    return saved.id;
  };

  const added = async (signed: string) => {
    const combined = await (await api()).vaultCombine(account, psbt, signed);
    const before = sigs?.signed ?? 0;
    const after = await (await api()).vaultSignatures(account, combined);
    if (after.signed <= before) throw new Error("No new signature in that. Make sure it's this vault's transaction, signed by a key that hasn't signed yet");
    setPsbt(combined);
    setSigs(after);
    setSigning(null);
    // Keep progress safe: a signature collected is never lost.
    if (proposalId || !after.ready) await save(combined);
  };

  const send = () =>
    run(async () => {
      setError(null);
      try {
        await (await api()).resume(paused, psbt, fee);
        if (proposalId) await (await api()).proposalDelete(proposalId);
        await refresh();
        onDone();
      } catch (e) {
        setError(errorText(e));
      }
    });

  const later = () =>
    run(async () => {
      try {
        await save(psbt);
        notify("ok", "Saved. Find it on the vault's card to finish signing.");
        close();
      } catch (e) {
        setError(errorText(e));
      }
    });

  const discard = () =>
    run(async () => {
      if (proposalId) await (await api()).proposalDelete(proposalId);
      notify("ok", "Payment cancelled");
      close();
    });

  if (signing) return <SignWith signer={signing} account={account} psbt={psbt} onSigned={added} onBack={() => setSigning(null)} />;

  const needed = sigs?.needed ?? quote.needsSignature?.signaturesNeeded ?? 2;
  return (
    <Sheet title="Collect signatures">
      <div style={{ textAlign: "center", padding: "6px 0 4px" }}>
        <div className="big-stat num" style={{ fontSize: 34 }}>
          {formatSats(quote.amountSat)} <span className="balance-unit">sats</span>
        </div>
        <p className="balance-sub">
          {to ? `to ${to.length > 28 ? `${to.slice(0, 14)}…${to.slice(-8)}` : to} · ` : ""}fee {formatSats(quote.feeSat)} sats
        </p>
      </div>
      <div className="sig-progress" aria-label={`${sigs?.signed ?? 0} of ${needed} signatures`}>
        {Array.from({ length: needed }, (_, i) => (
          <span key={i} className={i < (sigs?.signed ?? 0) ? "on" : ""} />
        ))}
      </div>
      <p className="group-note" style={{ textAlign: "center", marginTop: 0 }}>
        {sigs
          ? sigs.ready
            ? "Enough signatures. Ready to send."
            : `${sigs.signed} of ${needed} signatures. Sign with ${needed - sigs.signed} more key${needed - sigs.signed > 1 ? "s" : ""}.`
          : "Checking signatures…"}
      </p>
      <div className="group">
        {(sigs?.keys ?? []).map((k) => (
          <div className="cell key-row" key={k.fingerprint}>
            <KeyIcon source={k.source} />
            <span className="grow">
              {k.label}
              {sourceText(k.source) !== k.label && <span className="key-meta">{sourceText(k.source)}</span>}
            </span>
            {k.signed ? (
              <span className="sig-dot on" aria-label="Signed">
                <Icon name="check" size={14} stroke={3} />
              </span>
            ) : sigs?.ready ? (
              <span className="sig-dot" aria-label="Not needed" />
            ) : (
              <button className="btn small primary" onClick={() => (setError(null), setSigning(k))}>
                Sign
              </button>
            )}
          </div>
        ))}
      </div>
      {sigs?.ready ? (
        <button className="btn primary block" disabled={busy} onClick={send}>
          {busy ? "Sending…" : "Send"}
        </button>
      ) : (
        <button className="btn block" disabled={busy} onClick={later}>
          Save and finish later
        </button>
      )}
      <p style={{ textAlign: "center", margin: "14px 0 0" }}>
        <button className="text-button" style={{ color: "var(--red)" }} onClick={discard}>
          Cancel this payment
        </button>
      </p>
      {error && (
        <p className="field-error" role="alert" style={{ marginTop: 14 }}>
          <Icon name="warn" size={14} /> {error}
        </p>
      )}
    </Sheet>
  );
}

/** A saved proposal, opened from the vault's card. */
export function ProposalSheet({ id }: { id: string }) {
  const [p, setP] = useState<Proposal | null>(null);
  const [missing, setMissing] = useState(false);
  const { close, refresh } = useWallet();
  useEffect(() => {
    void api()
      .then((a) => a.proposals())
      .then((list) => {
        const found = list.find((x) => x.id === id);
        if (found) setP(found);
        else setMissing(true);
      });
  }, [id]);
  if (missing)
    return (
      <Sheet title="Payment">
        <p className="empty">This payment was already sent or cancelled.</p>
      </Sheet>
    );
  if (!p) return null;
  return (
    <VaultSign
      account={p.account}
      paused={p.execution}
      fee={p.fee}
      proposal={p}
      onDone={() => {
        void refresh();
        close();
      }}
    />
  );
}
