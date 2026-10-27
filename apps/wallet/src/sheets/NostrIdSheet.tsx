import { useEffect, useState } from "react";
import { api } from "../api";
import type { InviteFilter, NostrIdentity } from "../api/types";
import { Icon } from "../components/Icon";
import { ScanButton } from "../components/Scanner";
import { CopyButton, QR, Sheet, useBusy } from "../components/ui";
import { errorText } from "../format";

const FILTERS: { id: InviteFilter; label: string; note: string }[] = [
  { id: "follows", label: "People I follow", note: "Invites from people your npub follows, your contacts and people you already share a vault with. Anyone else needs a code or link." },
  { id: "mutual", label: "Mutual follows", note: "Only people who follow you back, plus contacts and co-signers. Anyone else needs a code or link." },
  { id: "codeOnly", label: "Code or link only", note: "Nobody can invite you without a code, link or QR from you." },
];

const short = (npub: string) => `${npub.slice(0, 12)}…${npub.slice(-6)}`;

/** The wallet's Nostr identity for group vaults: its own key, the linked social npub, who may invite. */
export function NostrIdSheet() {
  const [id, setId] = useState<NostrIdentity | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [paste, setPaste] = useState("");
  const [busy, run] = useBusy();

  useEffect(() => {
    void (async () => {
      try {
        setId(await (await api()).nostrIdentity());
      } catch (e) {
        setError(errorText(e));
      }
    })();
  }, []);

  const update = (u: Parameters<Awaited<ReturnType<typeof api>>["nostrUpdate"]>[0]) =>
    run(async () => {
      setError(null);
      try {
        setId(await (await api()).nostrUpdate(u));
        if (u.socialNpub) setPaste("");
      } catch (e) {
        setError(errorText(e));
      }
    });

  const errorLine = error && (
    <p className="field-error" role="alert" style={{ marginTop: 10 }}>
      <Icon name="warn" size={14} /> {error}
    </p>
  );

  if (!id) {
    return (
      <Sheet title="Nostr identity">
        <p className="empty">{error ? "" : "Loading…"}</p>
        {errorLine}
      </Sheet>
    );
  }

  const filter = FILTERS.find((f) => f.id === id.invites) ?? FILTERS[0];
  return (
    <Sheet title="Nostr identity">
      <p className="group-note" style={{ marginTop: 0 }}>
        Group vaults talk over Nostr with this wallet's own key. It comes from your recovery phrase, so your backup covers it. It's
        separate from the npub you use on Nostr, so vault chats never show up in White Noise or other apps.
      </p>

      <p className="group-label">This wallet</p>
      <QR value={`nostr:${id.npub}`} />
      <div className="group" style={{ marginTop: 12 }}>
        <div className="cell">
          <span className="grow mono" style={{ wordBreak: "break-all", fontSize: 13 }}>
            {id.npub}
          </span>
          <CopyButton text={id.npub} />
        </div>
      </div>

      <p className="group-label">Your npub</p>
      {id.socialNpub ? (
        <div className="group">
          <div className="cell">
            <span className="grow mono">{short(id.socialNpub)}</span>
            <button className="text-button" disabled={busy} onClick={() => update({ socialNpub: "" })}>
              Unlink
            </button>
          </div>
        </div>
      ) : (
        <>
          <p className="group-note" style={{ marginTop: 0 }}>
            Link the npub you use on Nostr so people you follow can invite you. The wallet only reads who it follows. It never asks for
            your nsec.
          </p>
          <div className="group">
            <label className="cell">
              <span className="visually-hidden">npub</span>
              <input value={paste} onChange={(e) => setPaste(e.target.value)} placeholder="npub1…" spellCheck={false} autoCapitalize="off" autoCorrect="off" />
            </label>
          </div>
          <div style={{ display: "flex", gap: 10 }}>
            <button className="btn block" disabled={busy || !paste.trim()} onClick={() => update({ socialNpub: paste })}>
              Link npub
            </button>
            <ScanButton
              className="btn block"
              label="Scan"
              title="Scan your npub"
              hint="Scan the npub QR from your Nostr app's profile."
              onResult={(r) => {
                if (r.kind === "text") update({ socialNpub: r.text });
                else setError("That QR isn't an npub");
              }}
            />
          </div>
        </>
      )}

      <p className="group-label">Who can invite you to a vault</p>
      <div className="segmented" role="radiogroup" aria-label="Who can invite you">
        {FILTERS.map((f) => (
          <button key={f.id} role="radio" aria-pressed={id.invites === f.id} aria-checked={id.invites === f.id} disabled={busy} onClick={() => update({ invites: f.id })}>
            {f.label}
          </button>
        ))}
      </div>
      <p className="group-note">
        {filter.note}
        {filter.id !== "codeOnly" && !id.socialNpub ? " Link your npub for follows to count." : ""} You always choose whether to join.
      </p>

      <div className="group">
        <label className="cell">
          <span className="grow">Findable from my npub</span>
          <input
            type="checkbox"
            role="switch"
            checked={id.discoverable}
            disabled={busy || !id.socialNpub}
            onChange={(e) => update({ discoverable: e.target.checked })}
            style={{ flex: "none", width: 22, height: 22, accentColor: "var(--accent)" }}
          />
        </label>
      </div>
      <p className="group-note">
        {id.socialNpub
          ? "Lets people you follow invite you without swapping a code first. It shows publicly that your npub uses Sessions Wallet vaults."
          : "Link your npub first."}
      </p>
      {errorLine}
    </Sheet>
  );
}
