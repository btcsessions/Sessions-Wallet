import { switchMode } from "../state/mode";
import { useWallet } from "../state/store";

/** The Basic / Advanced choice, prominent, with what Advanced adds. */
export function ModeSwitch({ compact = false }: { compact?: boolean }) {
  const advanced = useWallet((s) => s.layout?.advanced ?? false);
  return (
    <div className={`mode-switch${compact ? " compact" : ""}`}>
      <div className="mode-options" role="radiogroup" aria-label="Wallet mode">
        <button role="radio" aria-checked={!advanced} onClick={() => void switchMode(false)}>
          <strong>Basic</strong>
          <span>Simple and safe</span>
        </button>
        <button role="radio" aria-checked={advanced} onClick={() => void switchMode(true)}>
          <strong>Advanced</strong>
          <span>Every tool</span>
        </button>
      </div>
      {!compact && (
        <p className="group-note" style={{ margin: "8px 16px 0" }}>
          {advanced
            ? "Adds hot on-chain, vault, ecash and watch-only accounts, coin control and labels, custom fees, private sends, the network fees module, your own server, and technical details like xpubs."
            : "Spending, hardware wallets and the essentials. Switch to Advanced for vaults, ecash, coin control, custom fees and more."}
        </p>
      )}
    </div>
  );
}
