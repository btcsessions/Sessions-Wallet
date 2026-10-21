import { useEffect, useState } from "react";
import { api } from "../api";
import type { Settings } from "../api/types";
import { Sheet, useBusy } from "../components/ui";
import { useWallet } from "../state/store";
import { THEMES, type Appearance } from "../theme";

const CURRENCIES = ["CAD", "USD", "EUR", "GBP", "AUD", "CHF", "JPY"];

export function SettingsSheet() {
  const { currency, layout, setAdvanced, setStatus, close, refresh, setSpendStyle } = useWallet();
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
      <ThemePicker />

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

      <p className="group-label">On-chain sends</p>
      <div className="segmented" style={{ marginBottom: 6 }}>
        <button aria-pressed={(layout?.spendStyle ?? "efficient") === "efficient"} onClick={() => setSpendStyle("efficient")}>
          Efficient
        </button>
        <button aria-pressed={layout?.spendStyle === "private"} onClick={() => setSpendStyle("private")}>
          Private
        </button>
      </div>
      <p className="group-note" style={{ margin: "0 16px 22px" }}>
        The default for sends from on-chain accounts. You can switch on each send. Private looks like a coinjoin and costs more in fees.
      </p>

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

const APPEARANCES: { id: Appearance; label: string }[] = [
  { id: "auto", label: "Automatic" },
  { id: "light", label: "Light" },
  { id: "dark", label: "Dark" },
];

function ThemePicker() {
  const { theme, appearance, setTheme, setAppearance } = useWallet();
  return (
    <>
      <p className="group-label">Theme</p>
      <div className="theme-grid" role="radiogroup" aria-label="Theme">
        {THEMES.map((t) => {
          const [ground, card, accent, second] = t.swatch.dark;
          const [lg, lc] = t.swatch.light;
          return (
            <button key={t.id} role="radio" aria-checked={t.id === theme} className="theme-card" data-theme-preview={t.id} onClick={() => setTheme(t.id)}>
              <span className="theme-preview" aria-hidden>
                <span style={{ background: lg }}>
                  <i style={{ background: lc }} />
                  <b style={{ background: accent }} />
                </span>
                <span style={{ background: ground }}>
                  <i style={{ background: card }} />
                  <b style={{ background: accent }} />
                  <u style={{ background: second }} />
                </span>
              </span>
              <span className="theme-name">{t.name}</span>
              <span className="theme-blurb">{t.blurb}</span>
            </button>
          );
        })}
      </div>
      <p className="group-label">Appearance</p>
      <div className="segmented" role="radiogroup" aria-label="Appearance">
        {APPEARANCES.map((a) => (
          <button key={a.id} role="radio" aria-checked={a.id === appearance} aria-pressed={a.id === appearance} onClick={() => setAppearance(a.id)}>
            {a.label}
          </button>
        ))}
      </div>
    </>
  );
}
