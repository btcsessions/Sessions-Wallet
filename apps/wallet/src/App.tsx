import { useEffect } from "react";
import { api } from "./api";
import { Home } from "./Home";
import { Lock, Onboarding } from "./Onboarding";
import { Toast } from "./components/ui";
import { AccountSheet } from "./sheets/AccountSheet";
import { AddAccountSheet } from "./sheets/AddAccountSheet";
import { EcashTokenSheet } from "./sheets/EcashTokenSheet";
import { GallerySheet } from "./sheets/GallerySheet";
import { MoveSheet } from "./sheets/MoveSheet";
import { PaySheet } from "./sheets/PaySheet";
import { ReceiveSheet } from "./sheets/ReceiveSheet";
import { SettingsSheet } from "./sheets/SettingsSheet";
import { useWallet } from "./state/store";

function ActiveSheet() {
  const sheet = useWallet((s) => s.sheet);
  if (!sheet) return null;
  // Keyed so each opening starts fresh.
  switch (sheet.name) {
    case "pay":
      return <PaySheet key="pay" prefill={sheet.prefill} />;
    case "receive":
      return <ReceiveSheet key={`r-${sheet.account}`} account={sheet.account} />;
    case "move":
      return <MoveSheet key={`m-${sheet.from}-${sheet.to}`} from={sheet.from} to={sheet.to} />;
    case "gallery":
      return <GallerySheet />;
    case "add-account":
      return <AddAccountSheet key={sheet.kind} kind={sheet.kind} />;
    case "account":
      return <AccountSheet key={sheet.id} id={sheet.id} />;
    case "settings":
      return <SettingsSheet />;
    case "ecash-token":
      return <EcashTokenSheet account={sheet.account} />;
  }
}

export function App() {
  const { status, boot } = useWallet();

  useEffect(() => {
    void boot();
  }, [boot]);

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
      {status === "empty" && <Onboarding />}
      {status === "locked" && <Lock />}
      {status === "unlocked" && (
        <>
          <Home />
          <ActiveSheet />
        </>
      )}
      <Toast />
    </>
  );
}
