import type { AccountInfo } from "../api/types";
import { Sheet } from "../components/ui";
import { accountColor, accountKind, formatSats, held } from "../format";
import { resolveAccount } from "../modules/AccountModule";
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

/** The home-screen card showing `a` (its own, or its wallet's). */
export function homeCard(accounts: AccountInfo[], layout: { modules: { id: string }[] } | null | undefined, a: AccountInfo): string | null {
  for (const m of layout?.modules ?? []) {
    const shown = resolveAccount(accounts, m.id);
    if (shown && (shown.id === a.id || (!!a.group && shown.group === a.group))) return m.id;
  }
  return null;
}

/** Show or hide an account's card on this device's home screen (the account itself stays). */
export function HomeToggle({ account: a, block = false }: { account: AccountInfo; block?: boolean }) {
  const { accounts, layout, addModule, removeModule, notify } = useWallet();
  const card = homeCard(accounts, layout, a);
  const id = moduleFor(a) ?? (a.rail === "spark" ? "spending" : null);
  if (!card && !id) return null;
  const toggle = async () => {
    if (card) {
      await removeModule(card);
      notify("ok", `${a.name} is hidden from your home screen. Find it in Settings › All accounts.`);
    } else if (id) {
      await addModule({ id, size: "small" });
      notify("ok", `${a.name} is on your home screen`);
    }
  };
  return (
    <button className={block ? "btn block" : "btn small"} style={block ? { marginBottom: 12 } : undefined} onClick={() => void toggle()}>
      {card ? (block ? "Hide from home screen" : "Hide") : "Show on home"}
    </button>
  );
}

/** Settings › Accounts: every account in this wallet, on the home screen or not. */
export function AccountsSheet() {
  const { accounts, open } = useWallet();
  return (
    <Sheet title="Accounts">
      <p className="notice">Every account in this wallet, on all your linked devices. Hiding one only takes its card off this device's home screen: the account and its funds stay.</p>
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
            <HomeToggle account={a} />
          </div>
        ))}
      </div>
    </Sheet>
  );
}
