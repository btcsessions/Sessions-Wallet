import { useState } from "react";
import type { AccountInfo } from "../api/types";
import { formatSats, held } from "../format";
import { useWallet } from "../state/store";
import type { SweepRule } from "../state/synced";
import { activeAccounts } from "../modules/AccountModule";
import { AccountPicker } from "./Dropdown";
import { Icon } from "./Icon";

/** Sweeps smaller than this lose too much to the on-chain fee to be worth offering. */
export const MIN_SWEEP_SAT = 50_000;

const SWITCH = { flex: "none", width: 22, height: 22, accentColor: "var(--accent)" } as const;

/** Where excess can go: your on-chain accounts (cold storage first). */
export function sweepTargets(accounts: AccountInfo[]): AccountInfo[] {
  const rank = (a: AccountInfo) => (a.custody.type === "hardware" ? 0 : a.custody.type === "multisig" ? 1 : a.custody.type === "watchOnly" ? 2 : 3);
  return accounts.filter((a) => a.rail === "onchain").sort((a, b) => rank(a) - rank(b));
}

/** The amount to offer now, or null when there's nothing to offer. */
export function sweepDue(rule: SweepRule, from: AccountInfo | undefined, to: AccountInfo | undefined): number | null {
  if (!rule.enabled || !from || !to) return null;
  const balance = held(from);
  if (balance <= rule.thresholdSat) return null;
  // "Not now": quiet until the balance has grown another tenth of the limit.
  if (rule.snoozedAtSat !== undefined && balance < rule.snoozedAtSat + rule.thresholdSat / 10) return null;
  const excess = balance - rule.keepSat;
  return excess >= MIN_SWEEP_SAT ? excess : null;
}

function SatsField({ label, value, onChange }: { label: string; value: number; onChange: (v: number) => void }) {
  const [text, setText] = useState(value ? value.toLocaleString("en-US") : "");
  return (
    <label className="cell">
      <span className="grow">{label}</span>
      <input
        className="num"
        inputMode="numeric"
        value={text}
        onChange={(e) => {
          const digits = e.target.value.replace(/\D/g, "").slice(0, 10);
          setText(digits ? Number(digits).toLocaleString("en-US") : "");
          onChange(Number(digits || 0));
        }}
        style={{ textAlign: "right", maxWidth: 150 }}
      />
      <span className="value">sats</span>
    </label>
  );
}

/** Spending account sheet: set up auto-sweep. */
export function SweepSetting({ account }: { account: AccountInfo }) {
  const { accounts, layout, synced, setSynced } = useWallet();
  const targets = sweepTargets(activeAccounts(accounts, layout));
  const saved = synced.sweep.find((r) => r.from === account.id);
  const [rule, setRule] = useState<SweepRule>(
    saved ?? { enabled: false, from: account.id, to: targets[0]?.id ?? "", thresholdSat: 500_000, keepSat: 100_000 },
  );
  const [error, setError] = useState<string | null>(null);

  const save = (next: SweepRule) => {
    setRule(next);
    if (next.enabled) {
      if (!next.to) return setError("Add a hardware wallet or other on-chain account to sweep to first");
      if (next.keepSat >= next.thresholdSat) return setError("Keep less than the limit, or there's nothing to move");
      if (next.thresholdSat - next.keepSat < MIN_SWEEP_SAT)
        return setError(`Leave at least ${formatSats(MIN_SWEEP_SAT)} sats between the two, so a sweep is worth its network fee`);
    }
    setError(null);
    const { snoozedAtSat: _drop, ...clean } = next;
    void _drop;
    setSynced("sweep", [...synced.sweep.filter((r) => r.from !== account.id), clean]);
  };

  if (!targets.length) {
    return (
      <>
        <p className="group-label">Auto-sweep to cold storage</p>
        <p className="group-note" style={{ marginTop: 0 }}>
          Add a hardware wallet (or another on-chain account) and you can have Spending offer to move anything over a limit there.
        </p>
      </>
    );
  }

  return (
    <>
      <p className="group-label">Auto-sweep to cold storage</p>
      <div className="group">
        <label className="cell">
          <span className="grow">Offer to sweep the excess</span>
          <input type="checkbox" role="switch" checked={rule.enabled} onChange={(e) => save({ ...rule, enabled: e.target.checked })} style={SWITCH} />
        </label>
        {rule.enabled && (
          <>
            <AccountPicker label="Sweep to" accounts={targets} value={rule.to} onChange={(to) => save({ ...rule, to })} />
            <SatsField label="When Spending is over" value={rule.thresholdSat} onChange={(thresholdSat) => save({ ...rule, thresholdSat })} />
            <SatsField label="Keep in Spending" value={rule.keepSat} onChange={(keepSat) => save({ ...rule, keepSat })} />
          </>
        )}
      </div>
      {error ? (
        <p className="field-error">{error}</p>
      ) : (
        <p className="group-note">
          {rule.enabled
            ? `Once Spending holds more than ${formatSats(rule.thresholdSat)} sats, you'll be offered a one-tap move of everything above ${formatSats(rule.keepSat)} sats. It's an on-chain payment, so a network fee applies.`
            : "Keep Spending topped up for everyday payments and move the rest to cold storage."}
        </p>
      )}
    </>
  );
}

/** Home: "Spending is over your limit. Move the excess?" */
export function SweepOffers() {
  const { accounts, synced, setSynced, open } = useWallet();
  const offers = synced.sweep
    .map((rule) => {
      const from = accounts.find((a) => a.id === rule.from);
      const to = accounts.find((a) => a.id === rule.to);
      const amount = sweepDue(rule, from, to);
      return amount && from && to ? { rule, from, to, amount } : null;
    })
    .filter((o) => o !== null);
  if (!offers.length) return null;
  return (
    <>
      {offers.map(({ rule, from, to, amount }) => (
        <div className="sweep-offer" key={rule.from} role="status">
          <div className="sweep-offer-text">
            <Icon name="vault" size={18} />
            <span>
              {from.name} is over {formatSats(rule.thresholdSat)} sats. Move <b className="num">{formatSats(amount)} sats</b> to {to.name}?
            </span>
          </div>
          <div className="sweep-offer-actions">
            <button
              className="text-button"
              onClick={() => setSynced("sweep", synced.sweep.map((r) => (r.from === rule.from ? { ...r, snoozedAtSat: held(from) } : r)))}
            >
              Not now
            </button>
            <button className="btn primary small" onClick={() => open({ name: "move", from: from.id, to: to.id, amount })}>
              Move
            </button>
          </div>
        </div>
      ))}
    </>
  );
}
