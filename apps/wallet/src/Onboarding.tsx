import { useEffect, useState } from "react";
import { api } from "./api";
import { Icon } from "./components/Icon";
import { useBusy } from "./components/ui";
import { errorText } from "./format";
import { useWallet } from "./state/store";

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

export function Lock() {
  const setStatus = useWallet((s) => s.setStatus);
  const [code, setCode] = useState("");
  const [shake, setShake] = useState(false);
  const [error, setError] = useState<string | null>(null);

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
    </main>
  );
}

type Step = "welcome" | "restore" | "passcode" | "confirm" | "phrase";

export function Onboarding() {
  const setStatus = useWallet((s) => s.setStatus);
  const [step, setStep] = useState<Step>("welcome");
  const [mode, setMode] = useState<"create" | "restore">("create");
  const [restorePhrase, setRestorePhrase] = useState("");
  const [code, setCode] = useState("");
  const [first, setFirst] = useState("");
  const [shake, setShake] = useState(false);
  const [phrase, setPhrase] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [busy, run] = useBusy();

  const words = restorePhrase.trim().split(/\s+/).filter(Boolean);

  const finish = (passcode: string) =>
    run(async () => {
      const a = await api();
      if (mode === "create") {
        setPhrase(await a.createWallet(passcode, 12));
        setStep("phrase");
      } else {
        await a.restoreWallet(restorePhrase, passcode);
        setStatus("unlocked");
      }
    });

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
          <button className="btn primary block" onClick={() => (setMode("create"), setStep("passcode"))}>
            Create a new wallet
          </button>
          <button className="btn block" onClick={() => (setMode("restore"), setStep("restore"))}>
            I have a recovery phrase
          </button>
        </div>
      </main>
    );
  }

  if (step === "restore") {
    return (
      <main className="center-screen">
        <button className="text-button" style={{ alignSelf: "flex-start", marginLeft: -8 }} onClick={() => setStep("welcome")}>
          Back
        </button>
        <h1 className="welcome-title" style={{ fontSize: 30 }}>
          Enter your recovery phrase
        </h1>
        <p className="welcome-copy" style={{ marginBottom: 20 }}>
          12 or 24 words, separated by spaces. Restores your savings, spending and ecash accounts.
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
    return (
      <main className="center-screen" style={{ textAlign: "center" }}>
        <h1 className="welcome-title" style={{ fontSize: 26 }}>
          {step === "passcode" ? "Choose a passcode" : "Enter it again"}
        </h1>
        <p className="balance-sub">{busy ? "Setting up your wallet…" : "Six digits. It encrypts your keys on this device."}</p>
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
        In order, on paper. They're the only way to get your bitcoin back if you lose this device.
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
      <button className="btn primary block" disabled={!saved} onClick={() => setStatus("unlocked")}>
        Open my wallet
      </button>
    </main>
  );
}
