import { useEffect, useState } from "react";
import { api } from "../api";
import type { FoundAccount } from "../api/types";
import { errorText, formatSats } from "../format";
import { switchMode } from "../state/mode";
import { useWallet } from "../state/store";

/**
 * Hot on-chain accounts from this recovery phrase that have history but
 * aren't in the wallet yet, with one tap to add each.
 */
export function DiscoverHot({ auto = false, initial }: { auto?: boolean; initial?: FoundAccount[] }) {
  const { accounts, layout, addModule, refresh, notify } = useWallet();
  const [found, setFound] = useState<FoundAccount[] | null>(initial ?? null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState<number | null>(null);

  const scan = async () => {
    setScanning(true);
    setError(null);
    try {
      setFound(await (await api()).discoverHotAccounts());
    } catch (e) {
      setError(errorText(e));
    } finally {
      setScanning(false);
    }
  };

  useEffect(() => {
    if (auto && !initial) void scan();
  }, [auto]);

  const add = async (f: FoundAccount) => {
    setAdding(f.index);
    try {
      if (!layout?.advanced) await switchMode(true);
      const a = await (await api()).addAccount({ kind: "savings", name: f.index === 0 ? "On-Chain" : `On-Chain ${f.index + 1}`, accountIndex: f.index });
      // One card per wallet: only add it to the home screen if the wallet isn't there yet.
      const hasCard = (useWallet.getState().layout?.modules ?? []).some((m) => m.id.startsWith("savings"));
      if (!hasCard) await addModule({ id: `savings:${a.id}`, size: "small" });
      await refresh();
      setFound((fs) => fs?.filter((x) => x.index !== f.index) ?? null);
      notify("ok", `Added ${a.name}`);
    } catch (e) {
      notify("error", errorText(e));
    } finally {
      setAdding(null);
    }
  };

  const hotCount = accounts.filter((a) => a.group === "hot").length;

  return (
    <div>
      {!found && !scanning && !auto && (
        <button className="btn block" onClick={() => void scan()}>
          Find used accounts
        </button>
      )}
      {scanning && <p className="empty">Checking accounts on this recovery phrase…</p>}
      {error && <p className="field-error">{error}</p>}
      {found && found.length === 0 && <p className="group-note">No other accounts with history{hotCount ? "" : " on this recovery phrase"}.</p>}
      {found && found.length > 0 && (
        <div className="group">
          {found.map((f) => (
            <div className="cell" key={f.index}>
              <span className="grow">
                <div>Account {f.index + 1}</div>
                <div className="row-sub num">
                  {formatSats(f.balanceSat)} sats · {f.txCount} transaction{f.txCount === 1 ? "" : "s"}
                </div>
              </span>
              <button className="btn small primary" disabled={adding !== null} onClick={() => void add(f)}>
                {adding === f.index ? "Adding…" : "Add"}
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
