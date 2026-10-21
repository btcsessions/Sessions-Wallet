import type { AccountInfo } from "../api/types";
import { activeAccounts } from "../modules/AccountModule";
import { moduleDef } from "../modules/catalog";
import { useWallet } from "./store";

/** Account types only available in Advanced mode. */
export function isAdvancedAccount(a: AccountInfo): boolean {
  return a.custody.type === "multisig" || a.custody.type === "watchOnly" || a.custody.type === "mint" || (a.rail === "onchain" && a.custody.type === "hot");
}

/**
 * Switch Basic / Advanced. Going back to Basic is refused (with the reason)
 * while an Advanced-only account is in use, since Basic would hide it.
 */
export async function switchMode(advanced: boolean): Promise<void> {
  const { accounts, layout, setAdvanced, notify } = useWallet.getState();
  if (advanced) {
    await setAdvanced(true);
    return;
  }
  const inUse = activeAccounts(accounts, layout).filter(isAdvancedAccount);
  if (inUse.length) {
    const names = inUse.map((a) => a.name).join(", ");
    notify(
      "error",
      `Basic mode isn't available while ${names} ${inUse.length > 1 ? "are" : "is"} in use, because Basic would hide ${inUse.length > 1 ? "them" : "it"}. Move the sats out and remove ${inUse.length > 1 ? "those accounts" : "it"} from the home screen first.`,
    );
    return;
  }
  await setAdvanced(false, (id) => !moduleDef(id)?.advanced);
}
