import type { AccountInfo } from "../api/types";
import { Sheet } from "../components/ui";
import { accountColor, accountKind, formatSats, held } from "../format";
import { resolveAccount, walletHasCard } from "../modules/AccountModule";
import { useWallet } from "../state/store";

/** The home-screen module that shows an account. */
function moduleFor(a: AccountInfo): string | null {
  switch (a.custody.type) {
    case "hot":
      return a.rail === "spark" ? null : `savings:${a.id}`;
    case "hardware":
      return `hardware:${a.id}`;
    case "multisig":
      return `vault:${a.id}`;
    case "watchOnly":
      return `watch:${a.id}`;
    case "mint":
      return `ecash:${a.id}`;
  }
}

/** Settings › Accounts: every account in this wallet, on the home screen or not. */
export function AccountsSheet() {
  const { accounts, layout, open, addModule, notify } = useWallet();
  const onHome = new Set((layout?.modules ?? []).map((m) => resolveAccount(accounts, m.id)?.id).filter(Boolean));
  const shown = (a: AccountInfo) => a.rail === "spark" || onHome.has(a.id) || walletHasCard(accounts, layout, a);
  const show = async (a: AccountInfo) => {
    const id = moduleFor(a);
    if (!id) return;
    await addModule({ id, size: "small" });
    notify("ok", `${a.name} is on your home screen`);
  };
  return (
    <Sheet title="Accounts">
      <p className="notice">Every account in this wallet, on all your linked devices. Each device chooses which ones get a card on its home screen.</p>
      <div className="group">
        {accounts.map((a) => (
          <div className="cell" key={a.id}>
            <button className="grow" onClick={() => open({ name: "account", id: a.id })} style={{ textAlign: "left", background: "none", border: 0, padding: 0, color: "inherit", font: "inherit", cursor: "pointer" }}>
              <span style={{ display: "inline-block", width: 10, height: 10, borderRadius: 5, background: accountColor(a), marginRight: 8 }} />
              {a.name}
              <div className="row-sub">
                {accountKind(a)} · {formatSats(held(a))} sats
              </div>
            </button>
            {shown(a) ? (
              <small className="muted">On home</small>
            ) : (
              <button className="btn small" onClick={() => void show(a)}>
                Show on home
              </button>
            )}
          </div>
        ))}
      </div>
    </Sheet>
  );
}
