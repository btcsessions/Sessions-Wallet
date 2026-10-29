import { useState } from "react";
import { api } from "../api";
import { Sheet, useBusy, useSecureScreen } from "../components/ui";
import { useWallet } from "../state/store";
import { THEMES, type Appearance } from "../theme";
import { ModeSwitch } from "../components/ModeSwitch";
import { BackupSection } from "../components/BackupSection";
import { BiometricSetting } from "../components/BiometricSetting";
import { AlertsSetting } from "../components/AlertsSetting";
import { CheckinSetting } from "../components/Checkins";
import type { BackupStatus } from "../api/types";
import { Icon } from "../components/Icon";
import { NodeConnection } from "../components/NodeConnection";
import { AutoLockSetting } from "../components/AutoLockSetting";

const CURRENCIES = ["CAD", "USD", "EUR", "GBP", "AUD", "CHF", "JPY"];

export function SettingsSheet() {
  const { currencies, setCurrencies, layout, setStatus, close, setSpendStyle, open, accounts } = useWallet();
  const contacts = useWallet((s) => s.synced.contacts);
  const [phrase, setPhrase] = useState<string | null>(null);
  const [backup, setBackup] = useState<BackupStatus | null>(null);
  const [passcode, setPasscode] = useState("");
  const [busy, run] = useBusy();
  useSecureScreen(!!phrase);


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

      <NodeConnection />
      </>
      )}

      <p className="group-label">Payments</p>
      <div className="group">
        <button className="cell" onClick={() => open({ name: "contacts" })}>
          <span className="grow">Contacts</span>
          <span className="value">{contacts.length || ""}</span>
          <Icon name="chevron" size={16} />
        </button>
        {layout?.advanced && (
          <button className="cell" onClick={() => open({ name: "nwc" })}>
            <span className="grow">Connected apps (Nostr Wallet Connect)</span>
            <Icon name="chevron" size={16} />
          </button>
        )}
      </div>

      {layout?.advanced && (
        <>
          <p className="group-label">Group vaults</p>
          <div className="group">
            <button className="cell" onClick={() => open({ name: "nostr-id" })}>
              <span className="grow">Nostr identity</span>
              <Icon name="chevron" size={16} />
            </button>
          </div>
        </>
      )}

      <p className="group-label">Devices</p>
      <div className="group">
        <button className="cell" onClick={() => open({ name: "accounts" })}>
          <span className="grow">All accounts</span>
          <span className="value">{accounts.length}</span>
          <Icon name="chevron" size={16} />
        </button>
        <button className="cell" onClick={() => open({ name: "devices" })}>
          <span className="grow">Linked devices</span>
          <span className="value">Phone and computer</span>
          <Icon name="chevron" size={16} />
        </button>
      </div>

      <AlertsSetting />

      <BiometricSetting />

      <AutoLockSetting />

      <BackupSection onStatus={setBackup} />

      <CheckinSetting />

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
                autoComplete="off" data-1p-ignore data-lpignore="true"
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

/** tcp:// and http:// send everything in the clear. */
