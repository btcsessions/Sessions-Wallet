import { api } from "../api";
import { useWallet } from "../state/store";

/** Shown when a device has said nothing for a while: what to try, a way out, and a log for support. */
export function DeviceStuck({ text, onStop }: { text: string; onStop: () => void }) {
  const notify = useWallet((s) => s.notify);
  return (
    <div className="device-progress">
      <p>{text}</p>
      <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
        <button className="btn small" onClick={onStop}>
          Stop waiting
        </button>
        <button
          className="btn small"
          onClick={async () => {
            try {
              const log = await (await api()).diagnosticLog();
              await navigator.clipboard.writeText(log || "(empty)");
              notify("ok", "Diagnostic log copied. It has no keys or amounts.");
            } catch {
              notify("error", "Couldn't copy the log");
            }
          }}
        >
          Copy diagnostic log
        </button>
      </div>
    </div>
  );
}
