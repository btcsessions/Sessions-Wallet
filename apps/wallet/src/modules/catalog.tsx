import type { ComponentType } from "react";
import type { InstalledModule } from "../api/types";
import type { IconName } from "../components/Icon";
import { ActivityModule } from "./ActivityModule";
import { AccountModule } from "./AccountModule";
import { BalanceModule } from "./BalanceModule";
import { ContactsModule } from "./ContactsModule";
import { FeesModule } from "./FeesModule";
import { LiquidModule } from "./LiquidModule";
import { MoveModule } from "./MoveModule";
import { PayReceiveModule } from "./PayReceiveModule";
import { PriceModule } from "./PriceModule";

export interface ModuleProps {
  module: InstalledModule;
}

export type SetupFlow = "multisig" | "hardware" | "watchOnly" | "ecash" | "savings";

export interface ModuleDef {
  type: string;
  name: string;
  /** One line, plain words: what it does for you. */
  blurb: string;
  icon: IconName;
  tint: string;
  section: "essentials" | "accounts" | "move" | "tools";
  sizes: InstalledModule["size"][];
  component: ComponentType<ModuleProps>;
  /** Adding this module first creates an account with this flow. */
  setup?: SetupFlow;
  /** Only shown when Advanced mode is on. */
  advanced?: boolean;
  /** Several copies allowed (one per account). */
  multiple?: boolean;
}

const defs: ModuleDef[] = [
  {
    type: "balance",
    name: "Balance",
    blurb: "Everything you hold, and where it sits.",
    icon: "chart",
    tint: "#FF9500",
    section: "essentials",
    sizes: ["large", "medium"],
    component: BalanceModule,
  },
  {
    type: "pay-receive",
    name: "Send & Receive",
    blurb: "Send, receive and transfer between accounts. The wallet picks the cheapest way.",
    icon: "swap",
    tint: "#FF9500",
    section: "essentials",
    sizes: ["medium", "small"],
    component: PayReceiveModule,
  },
  {
    type: "activity",
    name: "Activity",
    blurb: "Recent payments across all your accounts.",
    icon: "list",
    tint: "#8E8E93",
    section: "essentials",
    sizes: ["large", "medium"],
    component: ActivityModule,
  },
  {
    type: "spending",
    name: "Spending",
    blurb: "Instant, low-fee Lightning and Spark payments for everyday amounts.",
    icon: "bolt",
    tint: "#FF9500",
    section: "accounts",
    sizes: ["small", "medium"],
    component: AccountModule,
  },
  {
    type: "savings",
    name: "Hot on-chain wallet",
    blurb: "On-chain bitcoin with keys on this phone. Off by default; hardware wallets and vaults are safer for savings.",
    icon: "key",
    tint: "#5856D6",
    section: "accounts",
    sizes: ["small", "medium"],
    component: AccountModule,
    setup: "savings",
    multiple: true,
    advanced: true,
  },
  {
    type: "vault",
    name: "Vault",
    blurb: "Multisig for serious savings. Needs several keys to spend.",
    icon: "vault",
    tint: "#30B0C7",
    section: "accounts",
    sizes: ["small", "medium"],
    component: AccountModule,
    setup: "multisig",
    multiple: true,
    advanced: true,
  },
  {
    type: "hardware",
    name: "Hardware wallet",
    blurb: "Trezor, BitBox02, Jade, Passport Prime or SeedSigner. Keys stay on the device; you sign each payment there.",
    icon: "device",
    tint: "#A2845E",
    section: "accounts",
    sizes: ["small", "medium"],
    component: AccountModule,
    setup: "hardware",
    multiple: true,
  },
  {
    type: "ecash",
    name: "Ecash",
    blurb: "Private Cashu tokens from a mint you choose. Send sats as a link.",
    icon: "coins",
    tint: "#34C759",
    section: "accounts",
    sizes: ["small", "medium"],
    component: AccountModule,
    setup: "ecash",
    multiple: true,
    advanced: true,
  },
  {
    type: "move",
    name: "Transfer",
    blurb: "Shift sats between any two of your accounts in one step.",
    icon: "swap",
    tint: "#007AFF",
    section: "move",
    sizes: ["medium", "small"],
    component: MoveModule,
  },
  {
    type: "contacts",
    name: "Contacts",
    blurb: "People you pay often. Tap a face to pay their Lightning address.",
    icon: "user",
    tint: "#AF52DE",
    section: "move",
    sizes: ["medium", "small"],
    component: ContactsModule,
  },
  {
    type: "liquid",
    name: "Liquid",
    blurb: "Send to or receive from a Liquid address. Swapped automatically, no L-BTC to manage.",
    icon: "drop",
    tint: "#32ADE6",
    section: "move",
    sizes: ["small", "medium"],
    component: LiquidModule,
    advanced: true,
  },
  {
    type: "price",
    name: "Price",
    blurb: "Bitcoin's price in your currency.",
    icon: "chart",
    tint: "#FF9500",
    section: "tools",
    sizes: ["small", "medium"],
    component: PriceModule,
  },
  {
    type: "fees",
    name: "Network fees",
    blurb: "What an on-chain payment costs right now.",
    icon: "gauge",
    tint: "#FF2D55",
    section: "tools",
    sizes: ["small", "medium"],
    component: FeesModule,
    advanced: true,
  },
  {
    type: "watch",
    name: "Watch-only",
    blurb: "Track any wallet's balance from its descriptor. Can't spend.",
    icon: "eye",
    tint: "#8E8E93",
    section: "tools",
    sizes: ["small", "medium"],
    component: AccountModule,
    setup: "watchOnly",
    multiple: true,
    advanced: true,
  },
];

export const CATALOG: Record<string, ModuleDef> = Object.fromEntries(defs.map((d) => [d.type, d]));

export const SECTIONS: { id: ModuleDef["section"]; title: string; lede: string }[] = [
  { id: "essentials", title: "Essentials", lede: "The basics. Start here." },
  { id: "accounts", title: "Add accounts", lede: "Different account types to use and store bitcoin." },
  { id: "move", title: "Transfer", lede: "Get sats where they need to be." },
  { id: "tools", title: "Tools", lede: "Handy readouts." },
];

/** Module ids look like `vault:vault-3` when tied to one account. */
export function moduleType(id: string): string {
  return id.split(":")[0];
}

export function moduleDef(id: string): ModuleDef | undefined {
  return CATALOG[moduleType(id)];
}

export function moduleAccount(m: InstalledModule): string | undefined {
  const [, account] = m.id.split(":");
  return account;
}
