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
  /** Accounts in the same wallet: "hot" (this seed) or "hw:<fingerprint>". */
  group?: string | null;
  /** Account number inside its wallet (BIP44 account level). */
  accountIndex?: number | null;
  /** History still loading: an on-chain account's first full scan hasn't finished (balance/activity may be incomplete). */
  scanning?: boolean;
  /** Why loading the history failed, while it retries on its own. */
  scanError?: string | null;
  /** What loading the history is doing right now, in plain words. */
  scanStatus?: string | null;
}

/** An account with history found while scanning a wallet. */
export interface FoundAccount {
  index: number;
  balanceSat: number;
  txCount: number;
  descriptor: string;
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
  /** savings: add at this exact account number (from discovery). */
  accountIndex?: number;
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
  /**
   * Your own Electrum server: `ssl://host:50002`, `tcp://host:50001`, or
   * `host:port` (TLS). Wins over `esploraUrl`; null = the public servers.
   * On save: leave out to keep it as is, null or "" to clear. Saved in its
   * full form (e.g. `ssl://host:50002`). Onion addresses are refused.
   */
  electrumUrl?: string | null;
  /** Read only: the server in use now (`ssl://host:port`, or an Esplora URL); null before the first connection. */
  chainServer?: string | null;
  /** Read only: the public Electrum servers used when none is set. */
  defaultElectrumServers?: string[];
  hasBreezKey: boolean;
  /** Write-only. */
  breezApiKey?: string | null;
}

export interface LightningAddress {
  address: string;
  username: string;
  lnurl: string;
}

/**
 * Where an on-chain deposit to a Spending account stands:
 * - `waiting`: fewer than `confirmationsNeeded` confirmations; claimed on its
 *   own once it has them (or now, for `instantFeeSat`, if offered).
 * - `claiming`: being claimed, or will be on the next sync (about a minute).
 * - `needsApproval`: claiming costs `claimFeeSat`, over the automatic limit.
 * - `failed`: the last claim failed (`error`); retried on every sync.
 * - `refunding`: being sent back on-chain (`refundTxid`).
 */
export type DepositStatus = "waiting" | "claiming" | "needsApproval" | "failed" | "refunding";

/** An on-chain deposit to a Spending account that isn't in its balance yet. */
export interface Deposit {
  txid: string;
  vout: number;
  amountSat: number;
  /** Null when the chain couldn't be read just now. */
  confirmations: number | null;
  /** 3 on mainnet. */
  confirmationsNeeded: number;
  status: DepositStatus;
  /** Network fee to claim it now (taken from the deposit), when known. */
  claimFeeSat: number | null;
  /** Fee to claim it early (before `confirmationsNeeded`), when offered. */
  instantFeeSat: number | null;
  /** Depth the early claim is offered at (often 0 or 1). */
  instantConfirmations: number | null;
  /** Why it's stuck, in plain English. */
  error: string | null;
  refundTxid: string | null;
}

/**
 * - `claimed`: in the balance now (`payment`).
 * - `submitted`: claimed early (or already being claimed); arrives in a moment.
 * - `deferred`: nothing claimed (`message`); `requiredFeeSat` when it cost
 *   more than allowed. It still claims itself later.
 */
export interface ClaimResult {
  status: "claimed" | "submitted" | "deferred";
  payment: Payment | null;
  message: string | null;
  requiredFeeSat: number | null;
}

export interface DescriptorPreview {
  descriptor: string;
  fingerprint: string;
  scriptType: string;
  firstAddress: string;
}

/** On-chain details for one transaction. */
export interface TxDetails {
  txid: string;
  confirmations: number;
  blockHeight: number | null;
  blockTime: number | null;
  feeSat: number | null;
  /** sat/vB */
  feeRate: number | null;
  vsize: number;
  canSpeedUp: boolean;
  canCancel: boolean;
  speedUpKind: "rbf" | "cpfp" | null;
}

/** Per-send extras (advanced): a label only you see, and coin control. */
export interface SpendOptions {
  label?: string | null;
  /** Outpoints ("txid:vout") to spend, exactly these. */
  coins?: string[] | null;
  /** Send everything (chosen coins or the whole account), fee taken from the amount. On-chain only. */
  sendMax?: boolean;
}

/** One coin (UTXO) in an on-chain account. */
export interface Utxo {
  outpoint: string;
  txid: string;
  vout: number;
  amountSat: number;
  address: string;
  label: string | null;
  confirmations: number;
  frozen: boolean;
  change: boolean;
}

export type LabelKind = "address" | "tx" | "output";

export type BumpAction = "speedUp" | "cancel";

export interface DetailRow {
  label: string;
  value: string;
  /** Copyable data, shown in monospace. */
  mono: boolean;
}

/** What it costs to receive into an account through a swap. */
export interface ReceiveQuote {
  /** Who does the swap, e.g. "Boltz". */
  provider: string;
  minSat: number;
  maxSat: number;
  /** Percentage fee, e.g. 0.25 for 0.25%. */
  feePercent: number;
  /** Network fees the swap pays (lockup + claim), in sats. */
  networkFeeSat: number;
  /** Total fee for the requested amount, when one was given. */
  totalFeeSat: number | null;
  /** What arrives for the requested amount, when one was given. */
  receiveSat: number | null;
}

/** Progress from a BitBox02 over USB (Tauri events `bitbox://status`, `bitbox://pairing`, `bitbox://paired`). */
export type BitboxEvent =
  /** "connecting": opening the USB device; "unlock": enter the password on the BitBox02; "confirm": check and confirm on its screen. */
  | { kind: "status"; state: "connecting" | "unlock" | "confirm" }
  /** First connection to this BitBox02: show the code; the user confirms on the device if it matches. */
  | { kind: "pairing"; code: string }
  /** Connected, unlocked and paired. */
  | { kind: "paired" };

/** What Jade is doing, in `jade://status` events. */
export type JadeState = "scanning" | "reconnecting" | "connecting" | "pairing" | "unlock" | "confirm";

/** Progress from a Blockstream Jade over Bluetooth (Tauri events `jade://status`, `jade://connected`). */
export type JadeEvent =
  /**
   * "scanning": looking for Jade nearby; "reconnecting": the Bluetooth link dropped since the last use (Jade
   * turned off or out of range), finding it again (no new pairing needed); "connecting": found it, connecting;
   * "pairing": the phone asks to pair (first time, or again because the old pairing went stale): confirm the
   * same number on Jade and the phone; "unlock": enter the PIN on Jade (the PIN server is contacted
   * automatically); "confirm": check and confirm on Jade's screen.
   */
  | { kind: "status"; state: JadeState }
  /** Connected and unlocked. */
  | { kind: "connected" };

/** Where the wallet's seed came from: a random recovery phrase, or derived from a passkey. */
export type SeedOrigin = "phrase" | "passkey";

/**
 * How the wallet is backed up (`backup_status`, and Tauri event `backup://status` whenever it changes).
 * The wallet's state (accounts, hardware wallets, vaults, labels, frozen coins, layout, settings, prefs) is
 * backed up encrypted to Nostr relays in every case; `passkey`/`phrase` say what brings the seed back.
 */
export interface BackupStatus {
  /** A passkey restores this wallet (a passkey wallet, or the phrase sealed under a passkey). */
  passkey: boolean;
  /** The user confirmed writing the recovery phrase down (or restored from it). */
  phrase: boolean;
  seedOrigin: SeedOrigin;
  /** Unix seconds of the last state backup that reached a relay; null = never. */
  lastBackupAt: number | null;
  /** Why the last backup failed, or a note (e.g. labels left out for size); null when fine. */
  lastError: string | null;
  /** Changes not backed up yet (also true while a restored wallet still looks for its old backup). */
  pending: boolean;
  /** Relays that took the last backup. */
  relays: string[];
}

/** What restoring the wallet's state found (`restore_state`, `restore_with_passkey`, event `backup://restored`). */
export interface RestoreResult {
  /** A backup was found (and applied unless `error`). */
  found: boolean;
  version: number | null;
  /** Unix seconds when that backup was made. */
  backedUpAt: number | null;
  /** Accounts restored (including Spending). */
  accounts: number;
  /** Labels restored (addresses, transactions, coins). */
  labels: number;
  /** e.g. "Couldn't reach the backup servers" (the app keeps trying in the background). */
  error: string | null;
}

/** The UI's own preferences (theme, currencies, unit…). Stored with the wallet so they're backed up. */
export type Prefs = Record<string, unknown>;
