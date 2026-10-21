import { useState } from "react";
import { api } from "../api";
import { Sheet, useBusy } from "../components/ui";
import { useWallet } from "../state/store";

const CURRENCIES = ["CAD", "USD", "EUR", "GBP", "AUD", "CHF", "JPY"];

export function SettingsSheet() {
  const { currency, layout, setAdvanced, setStatus, close, refresh } = useWallet();
  const [phrase, setPhrase] = useState<string | null>(null);
  const [passcode, setPasscode] = useState("");
  const [busy, run] = useBusy();

  const reveal = () =>
    run(async () => {
      setPhrase(await (await api()).revealPhrase(passcode));
      setPasscode("");
    });

  return (
    <Sheet title="Settings">
      <p className="group-label">Currency</p>
      <div className="chips" style={{ flexWrap: "wrap" }}>
        {CURRENCIES.map((c) => (
          <button
            key={c}
            className="chip"
            aria-pressed={c === currency}
            onClick={() => {
              try {
                localStorage.setItem("currency", JSON.stringify(c));
              } catch {
                /* best effort */
              }
              useWallet.setState({ currency: c });
              void refresh();
            }}
          >
            {c}
          </button>
        ))}
      </div>

      <div className="group" style={{ marginTop: 8 }}>
        <label className="cell">
          <span className="grow">
            <div>Advanced mode</div>
            <div className="row-sub">Extra detail and expert modules.</div>
          </span>
          <input type="checkbox" role="switch" checked={layout?.advanced ?? false} onChange={(e) => setAdvanced(e.target.checked)} style={{ width: 22, height: 22, accentColor: "var(--accent)" }} />
        </label>
      </div>

      <p className="group-label">Recovery phrase</p>
      {phrase ? (
        <>
          <ol className="phrase-grid">
            {phrase.split(" ").map((w, i) => (
              <li key={i}>{w}</li>
            ))}
          </ol>
          <button className="btn block" onClick={() => setPhrase(null)}>
            Hide
          </button>
        </>
      ) : (
        <>
          <div className="group">
            <label className="cell">
              <span className="grow">Passcode</span>
              <input
                type="password"
                inputMode="numeric"
                autoComplete="current-password"
                value={passcode}
                onChange={(e) => setPasscode(e.target.value)}
                style={{ textAlign: "right" }}
              />
            </label>
          </div>
          <p className="group-note">These words restore every account in this wallet. Anyone who sees them can take your bitcoin.</p>
          <button className="btn block" disabled={passcode.length < 6 || busy} onClick={reveal}>
            Show recovery phrase
          </button>
        </>
      )}

      <div style={{ height: 22 }} />
      <button
        className="btn block"
        onClick={async () => {
          await (await api()).lock();
          close();
          setStatus("locked");
        }}
      >
        Lock wallet
      </button>
    </Sheet>
  );
}
