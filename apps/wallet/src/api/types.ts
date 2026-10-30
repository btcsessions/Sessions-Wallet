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

export interface ConnectionTest {
  ok: boolean;
  /** The full address that worked (e.g. tcp://umbrel.local:50001): save this one. */
  url?: string | null;
  height?: number | null;
  encrypted: boolean;
  millis: number;
  error?: string | null;
}

export interface ChainStatus {
  /** Your own server, not the public defaults. */
  custom: boolean;
  server: string;
  connected: boolean;
  height?: number | null;
  error?: string | null;
  /** Android 17+: the app isn't allowed on the home network yet (show Allow). */
  needsPermission?: boolean;
}

export interface PrivateQuote {
  id: string;
  amountSat: number;
  /** The most it can cost; unused mint fee reserve stays in ecash. */
  maxFeeSat: number;
  from: string;
  hub: string;
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

/** Progress from a Ledger (Tauri event `ledger://status`). */
export type LedgerState = "connecting" | "bluetooth" | "pairing" | "unlock" | "open-app" | "confirm";

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
  /** Unix seconds of the last `testBackup` that passed on this device; null = never. Nudge a re-test now and then. */
  lastTestedAt: number | null;
}

/**
 * What to test (`testBackup`):
 * - `passkey`: the passkey sign-in prompt (the user picks a passkey); checks it restores this wallet.
 * - `phrase`: checks a recovery phrase the user typed (any spacing/case, 12 or 24 words) is this wallet's.
 * - `state`: no prompt; checks the online backup of accounts, labels and settings is readable and current.
 */
export type BackupTestMethod = "passkey" | "phrase" | "state";

/**
 * `testBackup`'s answer. Nothing is changed by a test (no wallet made, nothing restored or uploaded); a pass
 * only updates `BackupStatus.lastTestedAt`. Show `message`; the rest is for detail rows.
 */
export interface BackupTest {
  /** The backup would restore this wallet with its latest accounts, labels and settings. */
  ok: boolean;
  /** The passkey/phrase restores this wallet's keys. null for `state` (or when the servers couldn't be reached). */
  seedMatches: boolean | null;
  /** The online backup of accounts, labels and settings was found and decrypted. */
  stateFound: boolean;
  stateVersion: number | null;
  /** Version this device last backed up (or restored); null = never. */
  localVersion: number | null;
  /** The servers have this device's latest backup (or a newer one from another device). */
  upToDate: boolean;
  /** Unix seconds when the backup found was made. */
  backedUpAt: number | null;
  /** Backup servers that each hold a full copy of the newest backup, of `relaysTotal`. */
  relaysWithLatest: number;
  relaysAnswered: number;
  relaysTotal: number;
  /** In the backup found (accounts include hardware wallets and Spending). */
  accounts: number;
  hardwareWallets: number;
  labels: number;
  /**
   * One plain-English summary, e.g. "Your passkey restores this wallet, with 5 accounts, 2 hardware wallets and
   * 143 labels. 5 of 6 backup servers have the latest copy." / "That passkey belongs to a different wallet." /
   * "Couldn't reach the backup servers. Check your connection and try again."
   */
  message: string;
}

/**
 * Why `testBackup` couldn't run (a result with `ok: false` is a test that ran and failed). `message` is user-facing.
 * - `cancelled`: the user backed out of the passkey prompt. Say nothing.
 * - `no_passkey`: no Sessions Wallet passkey on the phone or in the account.
 * - `unsupported`: no passkeys on this device, the passkey manager lacks PRF, or the domain isn't set up for the app.
 * - `invalid_phrase`: `phrase` missing or not a valid recovery phrase (a typo in a word, wrong length).
 * - `locked`: unlock first. `failed`: anything else.
 */
export type BackupTestErrorCode = "cancelled" | "no_passkey" | "unsupported" | "invalid_phrase" | "locked" | "failed";

/** What `testBackup` rejects with. */
export class BackupTestError extends Error {
  readonly code: BackupTestErrorCode;
  constructor(code: BackupTestErrorCode, message: string) {
    super(message);
    this.name = "BackupTestError";
    this.code = code;
  }
}

export function isBackupTestError(e: unknown, code?: BackupTestErrorCode): e is BackupTestError {
  return e instanceof BackupTestError && (code === undefined || e.code === code);
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

/** Payment alerts (`alerts_status`). Android only for now: elsewhere `supported: false`. */
export interface AlertsStatus {
  supported: boolean;
  /** "prompt": asking shows the system prompt; "denied": only system settings can turn them on. */
  notifications: "granted" | "prompt" | "denied";
  /** The "Watching for payments" service is running (keeps the wallet connected with the app closed). */
  watching: boolean;
  /** The phone lets the app use the network while dozing (instant alerts with the screen off). */
  unrestricted: boolean;
}

export type NwcPermission = "pay" | "receive" | "balance" | "history" | "notifications";
export type NwcBudgetPeriod = "daily" | "weekly" | "monthly" | "yearly" | "total";

/** A Nostr Wallet Connect connection (an app allowed to use Spending). */
export interface NwcConnection {
  id: string;
  name: string;
  createdAt: number;
  lastUsedAt: number | null;
  enabled: boolean;
  permissions: NwcPermission[];
  /** Spending limit per `budgetPeriod`; null = no limit. */
  budgetSat: number | null;
  budgetPeriod: NwcBudgetPeriod;
  maxPerPaymentSat: number | null;
  /** Spent this period, fees included. */
  spentSat: number;
  relay: string;
}

export interface NwcNew {
  name: string;
  budgetSat: number | null;
  budgetPeriod: NwcBudgetPeriod;
  maxPerPaymentSat: number | null;
  permissions: NwcPermission[];
}

/** `wallet://payment`: a payment just arrived. */
export interface Arrival {
  account: string;
  accountName: string;
  /** Payment id (txid on-chain), or `txid:vout` for a deposit into Spending. */
  id: string;
  amountSat: number;
  /** received: spendable now; incoming: seen on-chain, unconfirmed; confirmed: an incoming one confirmed; deposit: paid to Spending's bitcoin address, added after confirmations. */
  kind: "received" | "incoming" | "confirmed" | "deposit";
  onchain: boolean;
}

/** Which biometric the phone most likely uses (a guess from its sensors): word the UI ("Unlock with fingerprint"). */
export type BiometricKind = "fingerprint" | "face" | "biometric";

/** Fingerprint / face unlock (`biometric_status`). Android only for now; desktop/iOS: `available: false`. */
export interface BiometricStatus {
  /** A strong (Class 3) fingerprint or face is set up on the phone. Weak (camera-only) face unlock doesn't count. */
  available: boolean;
  /** Biometric unlock is on: show the button on the lock screen when `available && enabled`. */
  enabled: boolean;
  /**
   * It was switched off because the phone's fingerprints or face changed. Show the `invalidated` message once,
   * then call `disableBiometric()` to clear it (turning it on again clears it too).
   */
  invalidated: boolean;
  kind: BiometricKind;
  /** Why it's unavailable, in plain English ("Add a fingerprint in your phone's Settings first."); null when available. */
  reason: string | null;
}

/**
 * Why a biometric call failed. `message` is always user-facing.
 * - `cancelled`: "Use passcode" / "Cancel" / back / timed out / app backgrounded. Show the passcode field quietly.
 * - `lockout`: too many failed fingerprint/face tries (message says for how long). Use the passcode.
 * - `invalidated`: fingerprints or face changed; it's been turned off. Show the message, ask for the passcode.
 * - `stale`: the saved passcode no longer opens the wallet; it's been turned off. Show the message.
 * - `not_enabled`: nothing saved. `unavailable`: no strong biometric here (message says why).
 * - `wrong_passcode` / `too_many_attempts`: from `enableBiometric`'s passcode check (`too_many_attempts` also from
 *   `unlockBiometric` when the passcode rate limit is active).
 * - `busy`: a prompt is already up. `failed`: anything else.
 */
export type BiometricErrorCode =
  | "unavailable"
  | "not_enabled"
  | "cancelled"
  | "lockout"
  | "invalidated"
  | "stale"
  | "wrong_passcode"
  | "too_many_attempts"
  | "busy"
  | "failed";

/** What `enableBiometric`, `disableBiometric` and `unlockBiometric` reject with. */
export class BiometricError extends Error {
  readonly code: BiometricErrorCode;
  constructor(code: BiometricErrorCode, message: string) {
    super(message);
    this.name = "BiometricError";
    this.code = code;
  }
}

export function isBiometricError(e: unknown, code?: BiometricErrorCode): e is BiometricError {
  return e instanceof BiometricError && (code === undefined || e.code === code);
}

// ---- Multisig vaults ----------------------------------------------------

/** Where a vault key lives. */
export type KeySource = { type: "phone"; account: number } | { type: "device"; device: string } | { type: "external" };

/** A vault key (or one just read, before it's in a vault). */
export interface VaultKey {
  fingerprint: string;
  /** `m/48'/0'/0'/2'` style. */
  path: string;
  xpub: string;
  /** `[fp/48h/0h/0h/2h]xpub`. */
  key: string;
  label: string;
  source: KeySource;
  /** This phone can sign with it. */
  ours: boolean;
}

export interface NewVault {
  name: string;
  threshold: number;
  keys: { key: string; label: string; source: KeySource }[];
}

export interface VaultImportPreview {
  name: string;
  threshold: number;
  total: number;
  script: string;
  keys: VaultKey[];
  notes: string[];
  /** Pass back to `vaultImport` as is. */
  descriptor: string;
}

export interface VaultImport {
  name: string;
  descriptor: string;
  keys: { xpub: string; label: string; source: KeySource }[];
}

export interface VaultDetails {
  name: string;
  threshold: number;
  total: number;
  script: string;
  keys: VaultKey[];
  /** Solo today; group wallets later. */
  coordination: { type: "solo" } | { type: "group"; group_id: string };
  descriptor: string;
  firstAddress: string;
  /** sortedmulti (BIP67); false for an imported fixed-order `multi` vault. */
  sorted: boolean;
}

/**
 * Wallet file formats: `descriptor` (Sparrow, Nunchuk, BlueWallet, Specter, Bitcoin Core), `coldcard` (the
 * Name/Policy/Derivation text file: Keystone, Passport, Jade, BlueWallet, Nunchuk, Sparrow), `bsms` (BIP-129:
 * Nunchuk, Sparrow), `caravan` (Caravan JSON), `specter` (Specter JSON backup).
 */
export type VaultExportFormat = "descriptor" | "coldcard" | "bsms" | "caravan" | "specter";

export interface ExportFile {
  filename: string;
  mime: string;
  content: string;
}

export interface VaultSignatures {
  keys: { fingerprint: string; label: string; source: KeySource; signed: boolean }[];
  signed: number;
  needed: number;
  ready: boolean;
  /** Where the money goes (not the vault's change). */
  destinations: { address: string; amountSat: number }[];
}

/** A vault spend waiting for signatures (finish it later; shared with a group's members later on). */
export interface Proposal {
  id: string;
  account: string;
  createdAt: number;
  updatedAt: number;
  psbt: string;
  execution: Execution;
  fee: FeePriority;
  amountSat: number;
  feeSat: number;
  to: string;
  note: string | null;
}

/** Who may send a vault invite without a code or link. */
export type InviteFilter = "follows" | "mutual" | "codeOnly";

/** The wallet's own Nostr identity for group vaults (from the seed, BIP85). */
export interface NostrIdentity {
  npub: string;
  pubkey: string;
  index: number;
  /** The user's social npub, linked so we can read who they follow. */
  socialNpub: string | null;
  invites: InviteFilter;
  discoverable: boolean;
}

/** What a join code is for. */
export interface GroupJoinFound {
  name: string;
  host: string;
  hostNpub: string;
}

/** A group vault this wallet takes part in. */
export interface GroupVault {
  groupId: string;
  name: string;
  threshold: number;
  total: number;
  creator: boolean;
  proposal: string | null;
  account: string | null;
  names: Record<string, string>;
  created: number;
}

export interface GroupSlotState {
  holder: string;
  label: string;
  key: string | null;
  fingerprint: string | null;
  device: string | null;
  confirmed: boolean;
}

export interface GroupSetupState {
  proposal: { id: string; name: string; threshold: number; slots: { holder: string; label: string }[]; network: string };
  proposer: string;
  slots: GroupSlotState[];
  descriptor: string | null;
  hash: string | null;
  firstAddress: string | null;
  finalized: boolean;
  ready: boolean;
  problems: string[];
}

export interface GroupMember {
  pubkey: string;
  npub: string;
  name: string;
  admin: boolean;
}

export interface GroupCode {
  /** K7QM-2XPA-9TRD */
  code: string;
  /** sessionswallet://join/… for the QR and texting */
  link: string;
  expires: number;
}

export interface GroupView {
  vault: GroupVault;
  me: string;
  members: GroupMember[];
  code: GroupCode | null;
  setup: GroupSetupState | null;
  mySlots: number[];
  shortCode: string | null;
}

export interface GroupJoiner {
  pubkey: string;
  name: string;
  at: number;
}

export interface GroupList {
  vaults: GroupVault[];
  invites: { groupId: string; name: string; fromNpub: string | null }[];
}

/** Linking a new device: the QR it shows. */
export interface LinkOffer {
  qr: string;
  expiresAt: number;
}

/** New device, while linking. */
export type LinkStatus =
  | { state: "waiting" }
  | { state: "code"; code: string; from: string }
  | { state: "ready"; from: string }
  | { state: "expired" };

/** A phone or computer linked to this wallet. */
export interface LinkedDevice {
  id: string;
  name: string;
  kind: "phone" | "desktop" | string;
  seenAt: number | null;
  thisDevice: boolean;
}

export interface SyncStatus {
  devices: LinkedDevice[];
  lastSyncAt: number | null;
}

/** `both`: merge; `sendMine`: this device's setup replaces the others'; `takeTheirs`: the reverse. */
export type SyncDirection = "both" | "sendMine" | "takeTheirs";
