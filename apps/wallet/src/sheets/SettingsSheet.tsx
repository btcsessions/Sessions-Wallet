import { useEffect, useState } from "react";
import { api } from "../api";
import type { Settings } from "../api/types";
import { Sheet, useBusy } from "../components/ui";
import { useWallet } from "../state/store";
import { THEMES, type Appearance } from "../theme";
import { ModeSwitch } from "../components/ModeSwitch";
import { BackupSection } from "../components/BackupSection";
import type { BackupStatus } from "../api/types";

const CURRENCIES = ["CAD", "USD", "EUR", "GBP", "AUD", "CHF", "JPY"];

export function SettingsSheet() {
  const { currencies, setCurrencies, layout, setStatus, close, setSpendStyle } = useWallet();
  const [phrase, setPhrase] = useState<string | null>(null);
  const [backup, setBackup] = useState<BackupStatus | null>(null);
  const [passcode, setPasscode] = useState("");
  const [busy, run] = useBusy();
  const [settings, setSettings] = useState<Settings | null>(null);
  const [server, setServer] = useState("");
  const [electrum, setElectrum] = useState("");
  const [breezKey, setBreezKey] = useState("");
  const notify = useWallet((s) => s.notify);

  useEffect(() => {
    api()
      .then((a) => a.settings())
      .then((s) => {
        setSettings(s);
        setServer(s.esploraUrl ?? "");
        setElectrum(s.electrumUrl ?? "");
      })
      .catch(() => {});
  }, []);

  const saveConnection = () =>
    run(async () => {
      await (await api()).saveSettings({ esploraUrl: server.trim() || null, electrumUrl: electrum.trim() || null, hasBreezKey: false, breezApiKey: breezKey.trim() || null });
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
      <p className="group-label">Mode</p>
      <ModeSwitch />

      <ThemePicker />

      <p className="group-label">Currencies</p>
      <div className="chips" style={{ flexWrap: "wrap" }}>
        {CURRENCIES.map((c) => {
          const on = currencies.includes(c);
          return (
            <button
              key={c}
              className="chip"
              aria-pressed={on}
              onClick={() => setCurrencies(on ? currencies.filter((x) => x !== c) : CURRENCIES.filter((x) => x === c || currencies.includes(x)))}
            >
              {c}
            </button>
          );
        })}
      </div>
      <p className="group-note">Pick one or more. Tap your balance to switch between sats, BTC and each of these.</p>

      <div style={{ height: 8 }} />
      {layout?.advanced && (
      <>
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
          <span className="grow">Electrum server</span>
          <input
            value={electrum}
            onChange={(e) => setElectrum(e.target.value)}
            placeholder="Public servers"
            inputMode="url"
            autoCapitalize="off"
            spellCheck={false}
            style={{ textAlign: "right" }}
          />
        </label>
        <label className="cell">
          <span className="grow">Esplora server</span>
          <input
            value={server}
            onChange={(e) => setServer(e.target.value)}
            placeholder="Optional"
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
      <p className="group-note">
        Point at your own Electrum server (Umbrel, Start9), e.g. ssl://umbrel.local:50002 or tcp://192.168.1.10:50001, so no third party sees your addresses. Leave empty to use public servers, with automatic fallback.
        {settings?.chainServer ? ` Connected to ${settings.chainServer}.` : ""}
      </p>
      <button
        className="btn block"
        style={{ marginBottom: 22 }}
        disabled={busy || (server === (settings?.esploraUrl ?? "") && electrum === (settings?.electrumUrl ?? "") && !breezKey.trim())}
        onClick={saveConnection}
      >
        Save connection
      </button>
      </>
      )}

      <BackupSection onStatus={setBackup} />

      {/* Passkey wallets keep their words out of sight in Basic mode. */}
      {(layout?.advanced || backup?.seedOrigin !== "passkey") && (
      <>
      <p className="group-label">Recovery phrase</p>
      {phrase ? (
        <>
          <ol className="phrase-grid">
            {phrase.split(" ").map((w, i) => (
              <li key={i}>{w}</li>
            ))}
          </ol>
          {backup && !backup.phrase && (
            <button
              className="btn primary block"
              style={{ marginBottom: 10 }}
              onClick={async () => setBackup(await (await api()).setPhraseBackedUp(true))}
            >
              I've written them down
            </button>
          )}
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
