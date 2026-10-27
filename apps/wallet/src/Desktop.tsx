import { useEffect, type ReactNode } from "react";
import { api } from "./api";
import { Icon } from "./components/Icon";
import { InlineSheet } from "./components/ui";
import { accountColor, accountKind, amountParts, held } from "./format";
import { Home } from "./Home";
import { AccountSheet } from "./sheets/AccountSheet";
import { useWallet } from "./state/store";

/** Window width from which the wallet uses the desktop layout. */
export const DESKTOP_MIN_WIDTH = 900;

/** Switch between the phone and desktop layouts as the window is resized. */
export function useDesktopLayout() {
  const setDesktop = useWallet((s) => s.setDesktop);
  useEffect(() => {
    const q = window.matchMedia(`(min-width: ${DESKTOP_MIN_WIDTH}px)`);
    const apply = () => setDesktop(q.matches);
    apply();
    q.addEventListener("change", apply);
    return () => q.removeEventListener("change", apply);
  }, [setDesktop]);
}

/** Desktop keyboard shortcuts (⌘ on macOS, Ctrl elsewhere). */
function useShortcuts() {
  const { open, sync, showPage } = useWallet();
  useEffect(() => {
    const onKey = async (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.altKey) return;
      const k = e.key.toLowerCase();
      const go = (f: () => void) => {
        e.preventDefault();
        f();
      };
      if (k === ",") go(() => open({ name: "settings" }));
      else if (k === "r") go(() => void sync(true));
      else if (k === "n" && !e.shiftKey) go(() => open({ name: "pay" }));
      else if (k === "n" && e.shiftKey) go(() => open({ name: "receive" }));
      else if (k === "1") go(() => showPage({ kind: "home" }));
      else if (k === "l" && e.shiftKey) {
        e.preventDefault();
        await (await api()).lock();
        useWallet.setState({ status: "locked", sheet: null });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, sync, showPage]);
}

function NavItem({ active, onClick, children, label }: { active: boolean; onClick: () => void; children: ReactNode; label?: string }) {
  return (
    <button className={`nav-item ${active ? "active" : ""}`} aria-current={active ? "page" : undefined} aria-label={label} onClick={onClick}>
      {children}
    </button>
  );
}

function Sidebar() {
  const { accounts, page, showPage, open, unit, price, currency, syncing } = useWallet();
  const total = accounts.reduce((s, a) => s + held(a), 0);
  const fmt = (sats: number) => {
    const p = amountParts(sats, unit, price, currency);
    return p.suffix ? `${p.value} ${p.suffix}` : p.value;
  };
  return (
    <nav className="sidebar" aria-label="Wallet">
      <div className="sidebar-brand" data-tauri-drag-region>
        <img src="/icon.svg" alt="" width={28} height={28} data-tauri-drag-region />
        <span data-tauri-drag-region>Sessions Wallet</span>
      </div>
      <NavItem active={page.kind === "home"} onClick={() => showPage({ kind: "home" })}>
        <Icon name="gauge" size={18} />
        <span className="grow">Overview</span>
        <span className="nav-amount num">{fmt(total)}</span>
      </NavItem>

      <p className="nav-label">Accounts</p>
      <div className="nav-list">
        {accounts.map((a) => (
          <NavItem key={a.id} active={page.kind === "account" && page.id === a.id} onClick={() => showPage({ kind: "account", id: a.id })}>
            <span className="nav-dot" style={{ background: accountColor(a) }} />
            <span className="grow nav-text">
              <span className="nav-name">{a.name}</span>
              <span className="nav-sub">{accountKind(a)}</span>
            </span>
            <span className="nav-amount num">{fmt(held(a))}</span>
          </NavItem>
        ))}
        <button className="nav-item add" onClick={() => open({ name: "gallery" })}>
          <Icon name="plus" size={16} />
          <span className="grow">Add account</span>
        </button>
      </div>

      <div className="sidebar-foot">
        <button className="nav-item" onClick={() => open({ name: "pay" })}>
          <Icon name="up" size={18} />
          <span className="grow">Send</span>
          <kbd>⌘N</kbd>
        </button>
        <button className="nav-item" onClick={() => open({ name: "receive" })}>
          <Icon name="down" size={18} />
          <span className="grow">Receive</span>
          <kbd>⇧⌘N</kbd>
        </button>
        <button className="nav-item" onClick={() => open({ name: "settings" })}>
          <Icon name="gear" size={18} />
          <span className="grow">Settings</span>
          <kbd>⌘,</kbd>
        </button>
        <button
          className="nav-item"
          onClick={async () => {
            await (await api()).lock();
            useWallet.setState({ status: "locked", sheet: null });
          }}
        >
          <Icon name="lock" size={18} />
          <span className="grow">Lock</span>
          <kbd>⇧⌘L</kbd>
        </button>
        <p className="sidebar-status" role="status">
          {syncing ? "Syncing…" : ""}
        </p>
      </div>
    </nav>
  );
}

/** The desktop window: a sidebar of accounts, the overview or one account beside it. */
export function DesktopShell() {
  const { page, accounts, showPage } = useWallet();
  useShortcuts();
  const account = page.kind === "account" ? accounts.find((a) => a.id === page.id) : undefined;

  // The account was removed: back to the overview.
  useEffect(() => {
    if (page.kind === "account" && accounts.length && !account) showPage({ kind: "home" });
  }, [page, accounts, account, showPage]);

  return (
    <div className="desktop">
      <Sidebar />
      <div className="desktop-main">
        <div className="desktop-drag" data-tauri-drag-region />
        {account ? (
          <InlineSheet.Provider value={{ onClose: () => showPage({ kind: "home" }) }}>
            <AccountSheet key={account.id} id={account.id} />
          </InlineSheet.Provider>
        ) : (
          <Home title="Overview" />
        )}
      </div>
    </div>
  );
}
