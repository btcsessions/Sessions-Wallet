// Mirrors crates/sessions-core/src/types.rs (serde camelCase).

export type RailKind = "onchain" | "spark" | "ecash";

export type Method =
  | "bitcoinAddress"
  | "bolt11"
  | "bolt12"
  | "lightningAddress"
  | "lnurl"
  | "sparkAddress"
  | "liquidAddress"
  | "cashuToken";

export type Custody =
  | { type: "hot" }
  | { type: "hardware"; device: string; fingerprint: string }
  | { type: "multisig"; threshold: number; total: number; localSigners: number }
  | { type: "watchOnly" }
  | { type: "mint"; url: string };

export interface Balance {
  confirmed: number;
  pendingIn: number;
  pendingOut: number;
}

export interface AccountInfo {
  id: string;
  name: string;
  rail: RailKind;
  custody: Custody;
  balance: Balance;
  receives: Method[];
  pays: Method[];
}

export type PaymentRequest =
  | { type: "bitcoinAddress"; address: string; amountSat?: number | null; label?: string | null }
  | { type: "bolt11"; invoice: string }
  | { type: "bolt12"; offer: string }
  | { type: "lightningAddress"; address: string }
  | { type: "lnurl"; lnurl: string }
  | { type: "sparkAddress"; address: string }
  | { type: "liquidAddress"; address: string; amountSat?: number | null }
  | { type: "cashuToken"; token: string }
  | { type: "unified"; options: PaymentRequest[] };

/** A decoded request plus the amount it asks for, if any. */
export interface Parsed {
  request: PaymentRequest;
  amountSat: number | null;
}

export type Leg =
  | { type: "toAccount"; from: string; to: string; method: Method }
  | { type: "toExternal"; from: string; request: PaymentRequest }
  | { type: "viaSwap"; from: string; provider: string; payMethod: Method; request: PaymentRequest };

export interface Plan {
  legs: Leg[];
  amountSat: number;
}

export interface ReceiveTarget {
  method: Method;
  request: string;
  amountSat?: number | null;
  feeSat?: number | null;
  expiresAt?: number | null;
  watchId?: string | null;
}

export type FeePriority = "fast" | "normal" | "slow" | number;

export interface SendQuote {
  account: string;
  method: Method;
  amountSat: number;
  feeSat: number;
  handle: string;
  needsSignature?: { psbtBase64: string; signaturesNeeded: number } | null;
  /** Built as a private (fake coinjoin) transaction. */
  private?: boolean;
  /** Will try payjoin first, with this transaction as the fallback. */
  payjoin?: boolean;
  notes?: string[];
}

export interface Prepared {
  quote: SendQuote;
  leg: Leg;
  remaining: Leg[];
  hopsAfter: number;
  amountSat: number;
  swapFeeSat: number;
}

export interface Payment {
  id: string;
  account: string;
  direction: "incoming" | "outgoing" | "internal";
  amountSat: number;
  feeSat: number;
  status: "pending" | "complete" | "failed";
  method: Method;
  timestamp: number;
  description?: string | null;
  token?: string | null;
}

export interface Execution {
  payments: Payment[];
  awaitingSignature?: SendQuote | null;
  remaining: Leg[];
  amountSat: number;
}

export interface InstalledModule {
  id: string;
  size: "small" | "medium" | "large";
  config?: unknown;
}

export type SpendStyle = "efficient" | "private";

export interface Layout {
  version: number;
  advanced: boolean;
  modules: InstalledModule[];
  /** Default for on-chain sends. */
  spendStyle?: SpendStyle;
}

export type WalletStatus = "empty" | "locked" | "unlocked";

export interface NewAccount {
  kind: "savings" | "multisig" | "hardware" | "watchOnly" | "ecash";
  name: string;
  /** multisig: threshold + cosigner key expressions; hardware/watchOnly: descriptor; ecash: mint url */
  threshold?: number;
  cosigners?: string[];
  descriptor?: string;
  device?: string;
  mintUrl?: string;
}

/** sat/vB for each confirmation target. */
export interface FeeRates {
  fast: number;
  normal: number;
  slow: number;
}

export interface Settings {
  esploraUrl: string | null;
  hasBreezKey: boolean;
  /** Write-only. */
  breezApiKey?: string | null;
}

export interface LightningAddress {
  address: string;
  username: string;
  lnurl: string;
}

export interface DescriptorPreview {
  descriptor: string;
  fingerprint: string;
  scriptType: string;
  firstAddress: string;
}
