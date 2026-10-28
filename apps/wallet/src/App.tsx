import { useEffect } from "react";
import { api } from "./api";
import { DesktopShell, useDesktopLayout } from "./Desktop";
import { Home } from "./Home";
import { Lock, Onboarding } from "./Onboarding";
import { Toast } from "./components/ui";
import { AccountSheet } from "./sheets/AccountSheet";
import { AddAccountSheet } from "./sheets/AddAccountSheet";
import { AddHardwareSheet } from "./sheets/AddHardwareSheet";
import { EcashTokenSheet } from "./sheets/EcashTokenSheet";
import { GallerySheet } from "./sheets/GallerySheet";
import { MoveSheet } from "./sheets/MoveSheet";
import { PaySheet } from "./sheets/PaySheet";
import { ReceiveSheet } from "./sheets/ReceiveSheet";
import { SettingsSheet } from "./sheets/SettingsSheet";
import { TxSheet } from "./sheets/TxSheet";
import { CoinsSheet } from "./sheets/CoinsSheet";
import { DiscoverSheet } from "./sheets/DiscoverSheet";
import { ContactsSheet } from "./sheets/ContactsSheet";
import { CheckinSheet } from "./components/Checkins";
import { GroupSheet } from "./sheets/GroupSheet";
import { GroupJoinSheet } from "./sheets/GroupJoinSheet";
import { NostrIdSheet } from "./sheets/NostrIdSheet";
import { DevicesSheet } from "./sheets/DevicesSheet";
import { NwcSheet } from "./sheets/NwcSheet";
import { VaultSheet } from "./sheets/VaultSheet";
import { VaultExportSheet } from "./sheets/VaultPanel";
import { ProposalSheet } from "./sheets/VaultSign";
import { useWallet } from "./state/store";

function ActiveSheet() {
  const sheet = useWallet((s) => s.sheet);
  if (!sheet) return null;
  // Keyed so each opening starts fresh.
  switch (sheet.name) {
    case "pay":
      return <PaySheet key={`pay-${sheet.from}`} prefill={sheet.prefill} from={sheet.from} />;
    case "receive":
      return <ReceiveSheet key={`r-${sheet.account}`} account={sheet.account} />;
    case "move":
      return <MoveSheet key={`m-${sheet.from}-${sheet.to}-${sheet.amount}`} from={sheet.from} to={sheet.to} amount={sheet.amount} />;
    case "gallery":
      return <GallerySheet />;
    case "add-account":
      return sheet.kind === "hardware" ? <AddHardwareSheet /> : sheet.kind === "multisig" ? <VaultSheet /> : <AddAccountSheet key={sheet.kind} kind={sheet.kind} />;
    case "vault":
      return <VaultSheet />;
    case "vault-export":
      return <VaultExportSheet account={sheet.account} />;
    case "proposal":
      return <ProposalSheet key={sheet.id} id={sheet.id} />;
    case "discover":
      return <DiscoverSheet found={sheet.found} />;
    case "coins":
      return <CoinsSheet account={sheet.account} />;
    case "tx":
      return <TxSheet key={sheet.id} id={sheet.id} back={sheet.back} />;
    case "account":
      return <AccountSheet key={sheet.id} id={sheet.id} />;
    case "settings":
      return <SettingsSheet />;
    case "nwc":
      return <NwcSheet />;
    case "group":
      return <GroupSheet key={sheet.groupId ?? "new"} groupId={sheet.groupId} />;
    case "group-join":
      return <GroupJoinSheet code={sheet.code} />;
    case "nostr-id":
      return <NostrIdSheet />;
    case "devices":
      return <DevicesSheet />;
    case "checkin":
      return <CheckinSheet />;
    case "contacts":
      return <ContactsSheet add={sheet.add} />;
    case "ecash-token":
      return <EcashTokenSheet account={sheet.account} />;
  }
}

/** Shown while the engine starts, and with the reason if it fails. */
function Splash() {
  const { bootError, boot } = useWallet();
  return (
    <main className="center-screen" style={{ textAlign: "center", alignItems: "center" }}>
      <img className="welcome-mark" src="/icon.svg" alt="" />
      {bootError ? (
        <>
          <h1 className="welcome-title" style={{ fontSize: 22 }}>
            The wallet couldn't start
          </h1>
          <p className="request-text" style={{ maxWidth: 360 }}>
            {bootError}
          </p>
          <button className="btn primary" onClick={() => void boot()}>
            Try again
          </button>
        </>
      ) : (
        <p className="balance-sub">Starting…</p>
      )}
    </main>
  );
}

export function App() {
  const { status, boot } = useWallet();
  const desktop = useWallet((s) => s.desktop);
  useDesktopLayout();

  useEffect(() => {
    void boot();
  }, [boot]);

  // Join links (sessionswallet://join/CODE) from a text message or a browser.
  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    let stop: (() => void) | undefined;
    const handle = (urls: string[] | null) => {
      const join = urls?.find((u) => u.startsWith("sessionswallet://join/"));
      if (!join) return;
      const st = useWallet.getState();
      // Group vaults are an Advanced feature.
      if (st.layout?.advanced) st.open({ name: "group-join", code: join });
      else st.notify("error", "Group vaults are in Advanced mode. Turn it on in Settings, then open the link again.");
    };
    void import("@tauri-apps/plugin-deep-link").then(async ({ onOpenUrl, getCurrent }) => {
      handle(await getCurrent().catch(() => null));
      stop = await onOpenUrl(handle);
    });
    return () => stop?.();
  }, []);

  // Auto-lock after five minutes in the background.
  useEffect(() => {
    let hiddenAt = 0;
    const onVis = async () => {
      if (document.hidden) {
        hiddenAt = Date.now();
      } else if (hiddenAt && Date.now() - hiddenAt > 5 * 60_000 && useWallet.getState().status === "unlocked") {
        await (await api()).lock();
        useWallet.setState({ status: "locked", sheet: null });
      }
    };
    document.addEventListener("visibilitychange", onVis);
    return () => document.removeEventListener("visibilitychange", onVis);
  }, []);

  return (
    <>
      {/* macOS: the title bar is overlaid; this strip moves the window. */}
      {!desktop && <div className="window-drag" data-tauri-drag-region />}
      {status === "loading" && <Splash />}
      {status === "empty" && <Onboarding />}
      {status === "locked" && <Lock />}
      {status === "unlocked" && (
        <>
          {desktop ? <DesktopShell /> : <Home />}
          <ActiveSheet />
        </>
      )}
      <Toast />
    </>
  );
}
