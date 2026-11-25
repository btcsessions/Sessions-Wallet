import { api } from "../api";
import { relativeTime } from "../format";
import { useWallet } from "../state/store";

/** Checked with the chain status, every minute while unlocked. Costs the mint nothing. */
export async function refreshEcashContact() {
  try {
    useWallet.setState({ ecashContact: await (await api()).ecashContact() });
  } catch {
    /* locked: keep the last one */
  }
}

function hostOf(url: string): string {
  return url.replace(/^\w+:\/\//, "").replace(/\/.*$/, "");
}

/** The ecash accounts whose mint this device can't reach right now. */
function useUnreachable() {
  const contact = useWallet((s) => s.ecashContact);
  const accounts = useWallet((s) => s.accounts);
  return accounts.filter((a) => a.rail === "ecash" && contact[a.id]?.error);
}

function since(lastOkAt: number | null | undefined): string {
  if (!lastOkAt) return "";
  const r = relativeTime(lastOkAt);
  return ` Last reached ${r === "Just now" ? "just now" : /ago$/.test(r) ? r : `on ${r}`}.`;
}

/** Home: an ecash balance may be out of date because its mint can't be reached. */
export function EcashWarning() {
  const down = useUnreachable();
  const contact = useWallet((s) => s.ecashContact);
  const open = useWallet((s) => s.open);
  if (!down.length) return null;
  const a = down[0];
  const url = a.custody.type === "mint" ? a.custody.url : "";
  return (
    <p className="notice" role="alert">
      <strong>Can't reach {hostOf(url) || "your ecash mint"}.</strong> {a.name}'s balance may be out of date: spending on another device won't show here until it can.
      {since(contact[a.id]?.lastOkAt)}{" "}
      <button className="text-button strong" style={{ padding: 0 }} onClick={() => open({ name: "account", id: a.id })}>
        Details
      </button>
    </p>
  );
}

/** Ecash account sheet: the same, with what went wrong. */
export function EcashContactNotice({ account }: { account: string }) {
  const c = useWallet((s) => s.ecashContact[account]);
  if (!c?.error) return null;
  return (
    <p className="notice" role="status">
      <strong>Can't reach this mint from here.</strong> The balance may be out of date until it can. If it works on another connection (like mobile data), the mint may be blocking this one.
      {since(c.lastOkAt)}
      <br />
      <small className="muted">{c.error}</small>
    </p>
  );
}
