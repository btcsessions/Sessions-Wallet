import { useState } from "react";
import { api } from "../api";
import type { AccountInfo, PaymentRequest } from "../api/types";
import { MINTS } from "../state/mints";
import { useWallet } from "../state/store";
import { errorText } from "../format";
import { Icon } from "./Icon";

export type Speed = "fast" | "private";

/**
 * The ecash account private Lightning goes through: the one chosen before,
 * else the only one. `null` until there is one (then show {@link MintPicker}).
 */
export function usePrivateHub(): AccountInfo | null {
  const accounts = useWallet((s) => s.accounts);
  const chosen = useWallet((s) => s.synced.privacy.hub);
  const ecash = accounts.filter((a) => a.rail === "ecash");
  return ecash.find((a) => a.id === chosen) ?? ecash[0] ?? null;
}

/** Fast or Private, for Lightning sends and receives (Advanced). */
export function SpeedSwitch({
  value,
  onChange,
  receive = false,
}: {
  value: Speed;
  onChange: (v: Speed) => void;
  receive?: boolean;
}) {
  return (
    <>
      <p className="group-label">Privacy</p>
      <div
        className="segmented"
        role="radiogroup"
        aria-label="Fast or private"
        style={{ marginBottom: 6 }}
      >
        <button
          role="radio"
          aria-checked={value === "fast"}
          aria-pressed={value === "fast"}
          onClick={() => onChange("fast")}
        >
          <Icon name="bolt" size={14} /> Fast
        </button>
        <button
          role="radio"
          aria-checked={value === "private"}
          aria-pressed={value === "private"}
          onClick={() => onChange("private")}
        >
          <Icon name="shield" size={14} /> Private
        </button>
      </div>
      {value === "private" && (
        <p className="group-note" style={{ marginTop: 0 }}>
          {receive
            ? "The payer pays your ecash mint, then the sats move on to this account. They see the mint, not you."
            : "Paid through your ecash mint: Spending's operator sees the mint, the mint sees the recipient, nobody sees both. A few sats more."}
        </p>
      )}
    </>
  );
}

/** Lightning request an ecash mint can pay (after resolving addresses / LNURL)? */
export function privateCapable(r: PaymentRequest | null): boolean {
  if (!r) return false;
  if (r.type === "unified") return r.options.some((o) => o.type === "bolt11");
  return (
    r.type === "bolt11" || r.type === "lightningAddress" || r.type === "lnurl"
  );
}

/** First time Private is used with no ecash account: pick a mint (one tap). */
export function MintPicker({
  onPicked,
}: {
  onPicked?: (a: AccountInfo) => void;
}) {
  const { refresh, setSynced, notify } = useWallet();
  const synced = useWallet((s) => s.synced);
  const [busy, setBusy] = useState<string | null>(null);
  const pick = async (url: string, label: string) => {
    setBusy(url);
    try {
      const account = await (
        await api()
      ).addAccount({ kind: "ecash", name: `Ecash · ${label}`, mintUrl: url });
      setSynced("privacy", { ...synced.privacy, hub: account.id });
      await refresh();
      notify("ok", `Private payments go through ${label}`);
      onPicked?.(account);
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setBusy(null);
    }
  };
  return (
    <>
      <p className="group-label">Pick a mint for private payments</p>
      <div className="group">
        {MINTS.map((m) => (
          <button
            key={m.value}
            className="cell"
            disabled={!!busy}
            onClick={() => void pick(m.value, m.label)}
          >
            <span className="grow">{m.label}</span>
            <span className="value">{busy === m.value ? "Adding…" : ""}</span>
            <Icon name="chevron" size={16} />
          </button>
        ))}
      </div>
      <p className="group-note">
        Private payments pass through a Cashu mint, which holds the sats for a
        moment. You only choose once; change it any time by adding another ecash
        account.
      </p>
    </>
  );
}
