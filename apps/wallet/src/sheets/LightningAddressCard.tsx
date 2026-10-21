import { useEffect, useState } from "react";
import { api } from "../api";
import type { LightningAddress } from "../api/types";
import { CopyButton, useBusy } from "../components/ui";
import { errorText } from "../format";
import { useWallet } from "../state/store";

const DOMAIN = "breez.tips";

/** Show the account's Lightning address, or let the user claim one. */
export function LightningAddressCard({ account, onChange }: { account: string; onChange?: (a: LightningAddress) => void }) {
  const notify = useWallet((s) => s.notify);
  const [address, setAddress] = useState<LightningAddress | null | undefined>(undefined);
  const [username, setUsername] = useState("");
  const [available, setAvailable] = useState<boolean | null>(null);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [busy, run] = useBusy();

  useEffect(() => {
    api()
      .then((a) => a.lightningAddress(account))
      .then(setAddress)
      .catch(() => setAddress(null));
  }, [account]);

  // Check availability once typing pauses.
  useEffect(() => {
    setAvailable(null);
    setCheckError(null);
    const name = username.trim();
    if (name.length < 2) return;
    let alive = true;
    const t = setTimeout(async () => {
      try {
        const ok = await (await api()).lightningAddressAvailable(account, name);
        if (alive) setAvailable(ok);
      } catch (e) {
        if (alive) setCheckError(errorText(e));
      }
    }, 600);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [username, account]);

  const claim = () =>
    run(async () => {
      const a = await (await api()).registerLightningAddress(account, username.trim());
      setAddress(a);
      onChange?.(a);
      notify("ok", `${a.address} is yours`);
    });

  if (address === undefined) return <p className="empty">Checking your Lightning address…</p>;

  if (address) {
    return (
      <>
        <div className="group">
          <div className="cell">
            <span className="grow">
              <div className="num" style={{ fontWeight: 600 }}>
                {address.address}
              </div>
              <div className="row-sub">Anyone can pay this, any time, even when your phone is off.</div>
            </span>
          </div>
        </div>
        <div style={{ display: "flex", gap: 10, marginTop: -10, marginBottom: 22 }}>
          <CopyButton text={address.address} />
        </div>
      </>
    );
  }

  return (
    <>
      <div className="group">
        <label className="cell">
          <span className="visually-hidden">Username</span>
          <input
            value={username}
            onChange={(e) => setUsername(e.target.value.replace(/\s/g, "").toLowerCase())}
            placeholder="yourname"
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            maxLength={32}
          />
          <span className="value">@{DOMAIN}</span>
        </label>
      </div>
      <p className="group-note" role="status" style={available === false || checkError ? { color: "var(--red)" } : undefined}>
        {checkError ??
          (available === true
            ? `${username.trim()}@${DOMAIN} is available.`
            : available === false
              ? "That name is taken. Try another."
              : "Pick a name people can pay, like an email address for bitcoin.")}
      </p>
      <button className="btn primary block" style={{ marginBottom: 22 }} disabled={!available || busy} onClick={claim}>
        {busy ? "Claiming…" : "Claim Lightning address"}
      </button>
    </>
  );
}
