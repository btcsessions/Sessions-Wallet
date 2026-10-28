import { useEffect, useState } from "react";
import { api } from "../api";
import type { GroupJoinFound } from "../api/types";
import { Icon } from "../components/Icon";
import { ScanButton } from "../components/Scanner";
import { Sheet, useBusy } from "../components/ui";
import { errorText } from "../format";
import { useWallet } from "../state/store";

const short = (npub: string) => `${npub.slice(0, 12)}…${npub.slice(-6)}`;

/** Join someone's group vault with the code (or link, or QR) they gave you. */
export function GroupJoinSheet({ code: initial }: { code?: string }) {
  const { close, open } = useWallet();
  const [code, setCode] = useState(initial ?? "");
  const [found, setFound] = useState<GroupJoinFound | null>(null);
  const [name, setName] = useState("");
  const [sent, setSent] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, run] = useBusy();

  const lookup = (c: string) =>
    run(async () => {
      setError(null);
      try {
        setFound(await (await api()).groupJoinLookup(c));
        setCode(c);
      } catch (e) {
        setError(errorText(e));
      }
    });

  const send = () =>
    run(async () => {
      setError(null);
      try {
        await (await api()).groupJoinSend(code, name);
        setSent(true);
      } catch (e) {
        setError(errorText(e));
      }
    });

  const back = (
    <button className="text-button" onClick={() => (found && !sent ? (setFound(null), setError(null)) : open({ name: "vault" }))}>
      {found && !sent ? "Back" : "Cancel"}
    </button>
  );
  const errorLine = error && (
    <p className="field-error" role="alert" style={{ marginTop: 10 }}>
      <Icon name="warn" size={14} /> {error}
    </p>
  );

  // Once they let us in, the group shows up: open it.
  useEffect(() => {
    if (!sent || !found) return;
    const t = setInterval(async () => {
      try {
        const l = await (await api()).groupList();
        const g = l.vaults.find((v) => !v.creator && v.name === found.name);
        if (g) open({ name: "group", groupId: g.groupId });
      } catch {
        /* keep waiting */
      }
    }, 6000);
    return () => clearInterval(t);
  }, [sent, found, open]);

  if (sent && found) {
    return (
      <Sheet title="Request sent" left={<span />}>
        <div className="hw-hero">
          <Icon name="people" size={48} />
        </div>
        <p className="notice">
          You asked to join <strong>{found.name}</strong>. When they let you in, it shows up under Group vaults and you'll add your key.
        </p>
        <p className="group-note">Nothing joins without your say: you'll see the invite first.</p>
        <button className="btn primary block" onClick={close}>
          Done
        </button>
      </Sheet>
    );
  }

  if (found) {
    return (
      <Sheet title="Join a vault" left={back}>
        <div className="group">
          <div className="cell">
            <span className="grow">Vault</span>
            <span className="value">{found.name}</span>
          </div>
          <div className="cell">
            <span className="grow">Created by</span>
            <span className="value mono">{short(found.hostNpub)}</span>
          </div>
        </div>
        <p className="group-note" style={{ marginTop: 0 }}>
          Check with them that this is their vault. They'll see your name and approve you.
        </p>
        <p className="group-label">Your name</p>
        <div className="group">
          <label className="cell">
            <span className="visually-hidden">Your name</span>
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="So they know it's you" maxLength={40} />
          </label>
        </div>
        <button className="btn primary block" disabled={busy || !name.trim()} onClick={send}>
          {busy ? "Sending…" : "Ask to join"}
        </button>
        {errorLine}
      </Sheet>
    );
  }

  return (
    <Sheet title="Join a vault" left={back}>
      <p className="notice">Scan the QR on their phone, or enter the code they sent you.</p>
      <ScanButton
        className="btn primary block"
        label="Scan join QR"
        title="Scan join QR"
        hint="Scan the QR on the vault creator's phone."
        onResult={(r) => {
          if (r.kind === "text") lookup(r.text);
          else setError("That QR isn't a join code");
        }}
      />
      <div style={{ height: 12 }} />
      <div className="group">
        <label className="cell">
          <span className="visually-hidden">Join code</span>
          <input
            value={code}
            onChange={(e) => setCode(e.target.value)}
            placeholder="K7QM-2XPA-9TRD"
            spellCheck={false}
            autoCapitalize="characters"
            autoCorrect="off"
            className="mono"
          />
        </label>
      </div>
      <button className="btn block" disabled={busy || !code.trim()} onClick={() => lookup(code)}>
        {busy ? "Looking it up…" : "Continue"}
      </button>
      {errorLine}
    </Sheet>
  );
}
