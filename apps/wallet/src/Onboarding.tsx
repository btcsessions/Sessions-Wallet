import { useEffect, useRef, useState } from "react";
import { api } from "./api";
import { Icon } from "./components/Icon";
import { useBusy } from "./components/ui";
import { errorText } from "./format";
import { useWallet } from "./state/store";
import { isBiometricError, type BiometricStatus } from "./api/types";

const PASSCODE_LENGTH = 6;

function Keypad({ value, onChange, onComplete, shake }: { value: string; onChange: (v: string) => void; onComplete: (v: string) => void; shake: boolean }) {
  const press = (d: string) => {
    if (value.length >= PASSCODE_LENGTH) return;
    const next = value + d;
    onChange(next);
    if (next.length === PASSCODE_LENGTH) setTimeout(() => onComplete(next), 120);
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (/^\d$/.test(e.key)) press(e.key);
      if (e.key === "Backspace") onChange(value.slice(0, -1));
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  return (
    <>
      <div className={`dots ${shake ? "shake" : ""}`} aria-label={`${value.length} of ${PASSCODE_LENGTH} digits entered`}>
        {Array.from({ length: PASSCODE_LENGTH }, (_, i) => (
          <span key={i} className={i < value.length ? "on" : ""} />
        ))}
      </div>
      <div className="keypad">
        {["1", "2", "3", "4", "5", "6", "7", "8", "9"].map((d) => (
          <button key={d} onClick={() => press(d)}>
            {d}
          </button>
        ))}
        <span />
        <button onClick={() => press("0")}>0</button>
        <button className="ghost" aria-label="Delete" onClick={() => onChange(value.slice(0, -1))}>
          Delete
        </button>
      </div>
    </>
  );
}

function Mark() {
  return <img className="welcome-mark" src="/icon.svg" alt="" />;
}

const BIO_LABEL: Record<BiometricStatus["kind"], string> = { fingerprint: "fingerprint", face: "face", biometric: "fingerprint or face" };

export function Lock() {
  const setStatus = useWallet((s) => s.setStatus);
  const [code, setCode] = useState("");
  const [shake, setShake] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [bio, setBio] = useState<BiometricStatus | null>(null);
  const [bioBusy, setBioBusy] = useState(false);

  const tryBiometric = async () => {
    setBioBusy(true);
    setError(null);
    try {
      await (await api()).unlockBiometric();
      setStatus("unlocked");
    } catch (e) {
      if (!isBiometricError(e, "cancelled")) setError(errorText(e));
      if (isBiometricError(e, "invalidated") || isBiometricError(e, "stale")) setBio((b) => (b ? { ...b, enabled: false } : b));
    } finally {
      setBioBusy(false);
    }
  };

  // Offer the fingerprint / face prompt straight away when it's set up.
  useEffect(() => {
    let alive = true;
    api()
      .then((a) => a.biometricStatus())
      .then((s) => {
        if (!alive) return;
        setBio(s);
        if (s.invalidated) {
          setError("Your fingerprints or face changed, so biometric unlock was turned off. Enter your passcode, then turn it on again in Settings.");
          void api().then((a) => a.disableBiometric().catch(() => {}));
        } else if (s.available && s.enabled) void tryBiometric();
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const tryUnlock = async (c: string) => {
    try {
      await (await api()).unlock(c);
      setStatus("unlocked");
    } catch (e) {
      setError(errorText(e));
      setShake(true);
      setTimeout(() => (setShake(false), setCode("")), 450);
    }
  };

  return (
    <main className="center-screen" style={{ textAlign: "center" }}>
      <div style={{ margin: "0 auto" }}>
        <Mark />
      </div>
      <h1 className="welcome-title" style={{ fontSize: 22 }}>
        Enter passcode
      </h1>
      <p className="balance-sub" style={{ minHeight: 20 }} role="alert">
        {error}
      </p>
      <Keypad value={code} onChange={setCode} onComplete={tryUnlock} shake={shake} />
      {bio?.available && bio.enabled && (
        <button className="text-button" style={{ marginTop: 18 }} disabled={bioBusy} onClick={() => void tryBiometric()}>
          Use {BIO_LABEL[bio.kind]}
        </button>
      )}
    </main>
  );
}

type Step = "welcome" | "setup" | "backup" | "restore-choice" | "restore" | "passcode" | "confirm" | "phrase" | "biometric";
type Backup = "passkey" | "phrase" | "both";

/** A big tappable choice card. */
function Choice({ title, body, badge, selected, onClick }: { title: string; body: string; badge?: string; selected: boolean; onClick: () => void }) {
  return (
    <button className={`choice-card${selected ? " selected" : ""}`} aria-pressed={selected} onClick={onClick}>
      <span className="choice-head">
        <strong>{title}</strong>
        {badge && <span className="choice-badge">{badge}</span>}
      </span>
      <span>{body}</span>
    </button>
  );
}

export function Onboarding() {
  const setStatus = useWallet((s) => s.setStatus);
  const [step, setStep] = useState<Step>("welcome");
  const [mode, setMode] = useState<"create" | "restore">("create");
  const [restoreWith, setRestoreWith] = useState<"passkey" | "phrase">("passkey");
  const [advanced, setAdvanced] = useState(false);
  const [backup, setBackup] = useState<Backup>("passkey");
  const [passkeys, setPasskeys] = useState<boolean | null>(null);
  const [restorePhrase, setRestorePhrase] = useState("");
  const [code, setCode] = useState("");
  const [first, setFirst] = useState("");
  const [shake, setShake] = useState(false);
  const [phrase, setPhrase] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, run] = useBusy();
  const [bio, setBio] = useState<BiometricStatus | null>(null);
  // A ref, not state: it's read in the same async call that sets it.
  const passcodeRef = useRef("");
  const setPasscode = (v: string) => {
    passcodeRef.current = v;
  };
  /** Where to go once the biometric question is answered. */
  const [after, setAfter] = useState<{ note?: string } | null>(null);

  useEffect(() => {
    api()
      .then((a) => a.biometricStatus())
      .then(setBio)
      .catch(() => {});
  }, []);

  useEffect(() => {
    api()
      .then((a) => a.passkeySupported())
      .then(setPasskeys)
      .catch(() => setPasskeys(false));
  }, []);

  const words = restorePhrase.trim().split(/\s+/).filter(Boolean);
  // Basic mode backs up with a passkey; without passkey support it falls back to words.
  const plan: Backup = advanced ? backup : passkeys ? "passkey" : "phrase";

  /** Offer fingerprint / face unlock first (when the phone has it), then into the wallet. */
  const open = async (note?: string) => {
    if (bio?.available && !bio.enabled && passcodeRef.current) {
      setAfter({ note });
      setStep("biometric");
      return;
    }
    await enter(note);
  };

  /** Into the wallet, in the mode chosen during setup. */
  const enter = async (note?: string) => {
    setStatus("unlocked");
    const st = useWallet.getState();
    if (advanced) {
      await st.refresh();
      await useWallet.getState().setAdvanced(true);
    }
    if (note) useWallet.getState().notify("ok", note);
  };

  const restoredNote = (r: { found: boolean; accounts: number; error?: string | null }) =>
    r.found ? `Restored ${r.accounts} account${r.accounts === 1 ? "" : "s"} and your settings from backup.` : r.error ? `Wallet restored. Couldn't reach the backup servers, so accounts and labels may be missing: ${r.error}` : "Wallet restored. No saved accounts or labels were found.";

  const finish = (passcode: string) =>
    run(async () => {
      setError(null);
      setPasscode(passcode);
      const a = await api();
      try {
        if (mode === "restore") {
          if (restoreWith === "passkey") {
            const r = await a.restoreWithPasskey(passcode);
            useWallet.setState({ restored: true });
            await open(restoredNote(r));
          } else {
            await a.restoreWallet(restorePhrase, passcode);
            useWallet.setState({ restored: true });
            const r = await a.restoreState().catch((e) => ({ found: false, accounts: 0, error: errorText(e) }));
            await open(restoredNote(r));
          }
          return;
        }
        if (plan === "passkey") {
          await a.createPasskeyWallet(passcode);
          await open("Wallet created and backed up with your passkey.");
          return;
        }
        setPhrase(await a.createWallet(passcode, 12));
        setStep("phrase");
      } catch (e) {
        setError(errorText(e));
        setCode("");
        setFirst("");
        setStep("passcode");
      }
    });

  // After the words are written down: confirm, add the passkey too if chosen, open.
  const afterPhrase = () =>
    run(async () => {
      const a = await api();
      await a.setPhraseBackedUp(true).catch(() => {});
      if (plan === "both") {
        try {
          await a.enablePasskeyBackup();
          await open("Wallet created. Backed up with your recovery phrase and your passkey.");
        } catch (e) {
          await open(`Wallet created. The passkey backup didn't work (${errorText(e)}); you can add it later in Settings.`);
        }
        return;
      }
      await open();
    });

  if (step === "biometric" && bio) {
    const what = BIO_LABEL[bio.kind];
    const turnOn = () =>
      run(async () => {
        try {
          await (await api()).enableBiometric(passcodeRef.current);
          setPasscode("");
          await enter(after?.note);
        } catch (e) {
          if (!isBiometricError(e, "cancelled")) setError(errorText(e));
        }
      });
    return (
      <main className="center-screen" style={{ textAlign: "center" }}>
        <Mark />
        <h1 className="welcome-title" style={{ fontSize: 28 }}>
          Unlock with your {what}?
        </h1>
        <p className="welcome-copy">Skip the passcode most of the time. Your passcode still works, and it's needed if your {what} changes.</p>
        {error && (
          <p className="field-error" role="alert">
            {error}
          </p>
        )}
        <div className="stack">
          <button className="btn primary block" disabled={busy} onClick={turnOn}>
            {busy ? `Confirm with your ${what}…` : `Use ${what}`}
          </button>
          <button className="btn block" disabled={busy} onClick={() => (setPasscode(""), void enter(after?.note))}>
            Not now
          </button>
        </div>
      </main>
    );
  }

  if (step === "welcome") {
    return (
      <main className="center-screen">
        <Mark />
        <h1 className="welcome-title">Your bitcoin, built your way.</h1>
        <p className="welcome-copy">
          Start with a simple wallet for spending and saving. Add vaults, hardware wallets, Lightning, Liquid and ecash whenever you want them.
          Your keys stay on your devices.
        </p>
        <div className="stack">
          <button className="btn primary block" onClick={() => (setMode("create"), setStep("setup"))}>
            Create a new wallet
          </button>
          <button className="btn block" onClick={() => (setMode("restore"), setStep(passkeys ? "restore-choice" : "restore"), setRestoreWith(passkeys ? "passkey" : "phrase"))}>
            Restore my wallet
          </button>
        </div>
      </main>
    );
  }

  if (step === "setup") {
    return (
      <main className="center-screen">
        <button className="text-button" style={{ alignSelf: "flex-start", marginLeft: -8 }} onClick={() => setStep("welcome")}>
          Back
        </button>
        <h1 className="welcome-title" style={{ fontSize: 30 }}>
          How do you want to start?
        </h1>
        <div className="choice-list">
          <Choice
            title="Basic"
            badge="Recommended"
            selected={!advanced}
            onClick={() => setAdvanced(false)}
            body={passkeys === false ? "The essentials, kept simple. You'll write down 12 words as your backup (passkeys aren't available on this device)." : "The essentials, kept simple. Backed up with a passkey: no words to write down."}
          />
          <Choice
            title="Advanced"
            selected={advanced}
            onClick={() => setAdvanced(true)}
            body="Every tool: coin control, vaults, custom fees, your own server. Back up with a recovery phrase, a passkey, or both."
          />
        </div>
        <p className="group-note">You can switch between Basic and Advanced any time in Settings.</p>
        <button className="btn primary block" onClick={() => setStep(advanced ? "backup" : "passcode")}>
          Continue
        </button>
      </main>
    );
  }

  if (step === "backup") {
    return (
      <main className="center-screen">
        <button className="text-button" style={{ alignSelf: "flex-start", marginLeft: -8 }} onClick={() => setStep("setup")}>
          Back
        </button>
        <h1 className="welcome-title" style={{ fontSize: 30 }}>
          How should we back it up?
        </h1>
        <div className="choice-list">
          <Choice title="Recovery phrase" selected={backup === "phrase"} onClick={() => setBackup("phrase")} body="12 words you write down on paper. Works with any bitcoin wallet, forever." />
          {passkeys !== false && (
            <>
              <Choice title="Passkey" selected={backup === "passkey"} onClick={() => setBackup("passkey")} body="Saved by your password manager and synced to your other devices. Restore with your fingerprint or face." />
              <Choice title="Both" badge="Most resilient" selected={backup === "both"} onClick={() => setBackup("both")} body="Write down the words, and also keep a passkey backup. Either one restores your wallet." />
            </>
          )}
        </div>
        <p className="group-note">Your accounts, hardware wallets, labels and layout are backed up too, encrypted so only your phrase or passkey can open them.</p>
        <button className="btn primary block" onClick={() => setStep("passcode")}>
          Continue
        </button>
      </main>
    );
  }

  if (step === "restore-choice") {
    return (
      <main className="center-screen">
        <button className="text-button" style={{ alignSelf: "flex-start", marginLeft: -8 }} onClick={() => setStep("welcome")}>
          Back
        </button>
        <h1 className="welcome-title" style={{ fontSize: 30 }}>
          Restore your wallet
        </h1>
        <div className="stack">
          <button className="btn primary block" onClick={() => (setRestoreWith("passkey"), setStep("passcode"))}>
            Use my passkey
          </button>
          <button className="btn block" onClick={() => (setRestoreWith("phrase"), setStep("restore"))}>
            Use a recovery phrase
          </button>
        </div>
        <p className="group-note">Your accounts, hardware wallets, labels and layout come back too, if they were backed up.</p>
      </main>
    );
  }

  if (step === "restore") {
    return (
      <main className="center-screen">
        <button className="text-button" style={{ alignSelf: "flex-start", marginLeft: -8 }} onClick={() => setStep(passkeys ? "restore-choice" : "welcome")}>
          Back
        </button>
        <h1 className="welcome-title" style={{ fontSize: 30 }}>
          Enter your recovery phrase
        </h1>
        <p className="welcome-copy" style={{ marginBottom: 20 }}>
          12 or 24 words, separated by spaces. Restores your spending, ecash and on-chain accounts.
        </p>
        <div className="group">
          <label className="cell">
            <span className="visually-hidden">Recovery phrase</span>
            <textarea
              autoFocus
              value={restorePhrase}
              onChange={(e) => setRestorePhrase(e.target.value)}
              spellCheck={false}
              autoCapitalize="off"
              autoCorrect="off"
              style={{ minHeight: 140, fontFamily: "inherit", fontSize: 17 }}
            />
          </label>
        </div>
        <p className="group-note">{words.length} words</p>
        <button className="btn primary block" disabled={words.length !== 12 && words.length !== 24} onClick={() => setStep("passcode")}>
          Continue
        </button>
      </main>
    );
  }

  if (step === "passcode" || step === "confirm") {
    const working =
      mode === "restore"
        ? restoreWith === "passkey"
          ? "Choose your passkey when asked, then we'll restore your wallet…"
          : "Restoring your wallet…"
        : plan === "passkey"
          ? "Create your passkey when asked…"
          : "Setting up your wallet…";
    return (
      <main className="center-screen" style={{ textAlign: "center" }}>
        <h1 className="welcome-title" style={{ fontSize: 26 }}>
          {step === "passcode" ? "Choose a passcode" : "Enter it again"}
        </h1>
        <p className="balance-sub">{busy ? working : "Six digits. It unlocks your wallet on this device."}</p>
        {error && (
          <p className="field-error" role="alert" style={{ margin: "0 auto 8px", maxWidth: 320 }}>
            {error}
          </p>
        )}
        <Keypad
          value={code}
          onChange={setCode}
          shake={shake}
          onComplete={(c) => {
            if (step === "passcode") {
              setFirst(c);
              setCode("");
              setStep("confirm");
            } else if (c === first) {
              finish(c);
            } else {
              setShake(true);
              setTimeout(() => (setShake(false), setCode(""), setStep("passcode")), 450);
            }
          }}
        />
      </main>
    );
  }

  // step === "phrase": show the new recovery phrase once.
  return (
    <main className="center-screen">
      <h1 className="welcome-title" style={{ fontSize: 30 }}>
        Write down these words
      </h1>
      <p className="welcome-copy" style={{ marginBottom: 20 }}>
        In order, on paper. {plan === "both" ? "Together with your passkey, they" : "They're the only way to"} get your bitcoin back if you lose this device.
      </p>
      <ol className="phrase-grid">
        {phrase?.split(" ").map((w, i) => (
          <li key={i}>{w}</li>
        ))}
      </ol>
      <p className="notice">
        <Icon name="warn" size={16} /> Never type them into a website or share a photo of them. Nobody legitimate will ever ask for them.
      </p>
      <label className="cell" style={{ padding: "8px 0 18px" }}>
        <input type="checkbox" checked={saved} onChange={(e) => setSaved(e.target.checked)} style={{ flex: "none", width: 22, height: 22, accentColor: "var(--accent)" }} />
        <span>I've written them down somewhere safe</span>
      </label>
      <button className="btn primary block" disabled={!saved || busy} onClick={afterPhrase}>
        {busy && plan === "both" ? "Create your passkey when asked…" : plan === "both" ? "Next: add the passkey" : "Open my wallet"}
      </button>
    </main>
  );
}
