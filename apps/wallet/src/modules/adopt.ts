import type { AccountInfo, Layout } from "../api/types";
import { moduleAccount, moduleType } from "./catalog";
import { resolveAccount, walletHasCard } from "./AccountModule";

/** The card type for an account a person adds (hot accounts have their own). */
function cardFor(a: AccountInfo): string | null {
  switch (a.custody.type) {
    case "hardware":
      return "hardware";
    case "multisig":
      return "vault";
    case "watchOnly":
      return "watch";
    case "mint":
      return "ecash";
    default:
      return null;
  }
}

/**
 * Give accounts a card on this device's home screen. Each kind of device
 * keeps its own layout, so an account added on a linked phone arrives here
 * without one: a "Set up" card of the same kind takes it, else it gets a new
 * card. Accounts this device has already seen are left alone, so removing a
 * card sticks. `known` is null the first time (nothing is added then, only
 * "Set up" cards filled). Returns the layout if it changed, and the accounts
 * now known.
 */
export function adoptAccounts(
  accounts: AccountInfo[],
  layout: Layout,
  known: string[] | null,
): { layout: Layout | null; known: string[] } {
  const modules = [...layout.modules];
  const shown = new Set(modules.map((m) => resolveAccount(accounts, m.id)?.id).filter(Boolean));
  let changed = false;
  for (const a of accounts) {
    const card = cardFor(a);
    if (!card || shown.has(a.id) || walletHasCard(accounts, { modules }, a)) continue;
    const empty = modules.findIndex((m) => moduleType(m.id) === card && !moduleAccount(m));
    if (empty >= 0) {
      modules[empty] = { ...modules[empty], id: `${card}:${a.id}` };
    } else if (known && !known.includes(a.id)) {
      modules.push({ id: `${card}:${a.id}`, size: "small" });
    } else {
      continue;
    }
    shown.add(a.id);
    changed = true;
  }
  const all = new Set([...(known ?? []), ...accounts.map((a) => a.id)]);
  return { layout: changed ? { ...layout, modules } : null, known: [...all] };
}
