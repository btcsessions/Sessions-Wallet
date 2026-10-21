import { useEffect, useState } from "react";
import { api } from "../api";
import type { Settings } from "../api/types";
import { Sheet, useBusy } from "../components/ui";
import { useWallet } from "../state/store";

const CURRENCIES = ["CAD", "USD", "EUR", "GBP", "AUD", "CHF", "JPY"];

export function SettingsSheet() {
  const { currency, layout, setAdvanced, setStatus, close, refresh } = useWallet();
  const [phrase, setPhrase] = useState<string | null>(null);
  const [passcode, setPasscode] = useState("");
  const [busy, run] = useBusy();
  const [settings, setSettings] = useState<Settings | null>(null);
  const [server, setServer] = useState("");
  const [breezKey, setBreezKey] = useState("");
  const notify = useWallet((s) => s.notify);

  useEffect(() => {
    api()
      .then((a) => a.settings())
      .then((s) => {
        setSettings(s);
        setServer(s.esploraUrl ?? "");
      })
      .catch(() => {});
  }, []);

  const saveConnection = () =>
    run(async () => {
      await (await api()).saveSettings({ esploraUrl: server.trim() || null, hasBreezKey: false, breezApiKey: breezKey.trim() || null });
      setBreezKey("");
      setSettings(await (await api()).settings());
      notify("ok", "Saved. Takes effect next time you unlock.");
    });

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

      <p className="group-label">Connection</p>
      <div className="group">
        <label className="cell">
          <span className="grow">Your server</span>
          <input
            value={server}
            onChange={(e) => setServer(e.target.value)}
            placeholder="mempool.space"
            inputMode="url"
            autoCapitalize="off"
            spellCheck={false}
            style={{ textAlign: "right" }}
          />
        </label>
        <label className="cell">
          <span className="grow">Breez API key</span>
          <input
            type="password"
            value={breezKey}
            onChange={(e) => setBreezKey(e.target.value)}
            placeholder={settings?.hasBreezKey ? "Saved" : "Needed for Spending"}
            autoCapitalize="off"
            spellCheck={false}
            style={{ textAlign: "right" }}
          />
        </label>
      </div>
      <p className="group-note">Point at your own Esplora or mempool instance (Umbrel, Start9) so no third party sees your addresses.</p>
      <button className="btn block" style={{ marginBottom: 22 }} disabled={busy || (server === (settings?.esploraUrl ?? "") && !breezKey.trim())} onClick={saveConnection}>
        Save connection
      </button>

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
