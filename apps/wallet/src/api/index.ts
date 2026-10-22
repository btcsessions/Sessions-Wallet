import type {
  AccountInfo,
  BitboxEvent,
  JadeEvent,
  BumpAction,
  DescriptorPreview,
  SendQuote,
  FoundAccount,
  Utxo,
  LabelKind,
  SpendOptions,
  TxDetails,
  DetailRow,
  ReceiveQuote,
  Execution,
  FeeRates,
  FeePriority,
  Layout,
  LightningAddress,
  Method,
  NewAccount,
  Parsed,
  Payment,
  Plan,
  Prepared,
  ReceiveTarget,
  Settings,
  SpendStyle,
  WalletStatus,
  BackupStatus,
  RestoreResult,
  Prefs,
} from "./types";

/**
 * Everything the UI can ask of the wallet. In the native app this is backed
 * by Tauri commands into the Rust core; in a plain browser (design preview)
 * it falls back to an in-memory mock with fake sats.
 */
export interface WalletApi {
  readonly isMock: boolean;
  status(): Promise<WalletStatus>;
  createWallet(passcode: string, words: 12 | 24): Promise<string>;
  restoreWallet(phrase: string, passcode: string): Promise<void>;
  unlock(passcode: string): Promise<void>;
  lock(): Promise<void>;
  /** Also works for passkey wallets (their phrase is derived from the passkey). */
  revealPhrase(passcode: string): Promise<string>;

  /** Whether this device can make passkeys (Android 9+). Desktop and iOS: false for now. */
  passkeySupported(): Promise<boolean>;
  /**
   * Basic setup: make a passkey and a wallet whose seed comes from it (no phrase to write down). The passcode
   * still unlocks the app day to day. Rejects with a plain message if the passkey manager can't do it
   * ("Your passkey manager doesn't support passkey wallets yet; use a recovery phrase"), or the user cancels.
   */
  createPasskeyWallet(passcode: string): Promise<void>;
  /**
   * Sign in with a passkey on a new phone: brings back its wallet (a passkey wallet, or a phrase backed up with
   * that passkey) and the wallet's state. Rejects if the backup servers can't be reached (nothing is created).
   */
  restoreWithPasskey(passcode: string): Promise<RestoreResult>;
  /** Call right after `restoreWallet`: finds the wallet's state backup and applies it (accounts, labels, layout…). Never rejects. */
  restoreState(): Promise<RestoreResult>;
  /** Back up this phrase wallet's recovery phrase with a new passkey too (Advanced "both", or later in Settings). */
  enablePasskeyBackup(): Promise<BackupStatus>;
  backupStatus(): Promise<BackupStatus>;
  /** Back up the state now and wait for the result. */
  backupNow(): Promise<BackupStatus>;
  /** The user confirmed they wrote the recovery phrase down. */
  setPhraseBackedUp(done: boolean): Promise<BackupStatus>;
  /** `backup://status` events. Returns an unsubscribe. */
  onBackupStatus(cb: (s: BackupStatus) => void): Promise<() => void>;
  /** `backup://restored`: the app restored the wallet's state on its own (a retry after being offline). Reload everything. */
  onStateRestored(cb: (r: RestoreResult) => void): Promise<() => void>;
  /** UI preferences, kept with the wallet (and in its backup) instead of localStorage. Unlocked only. */
  prefs(): Promise<Prefs>;
  savePrefs(prefs: Prefs): Promise<void>;

  accounts(): Promise<AccountInfo[]>;
  addAccount(spec: NewAccount): Promise<AccountInfo>;
  /** Only empty accounts can be removed. */
  removeAccount(id: string): Promise<void>;
  /** Hot on-chain accounts only. */
  payjoinReceive(account: string): Promise<boolean>;
  setPayjoinReceive(account: string, enabled: boolean): Promise<void>;
  /** Coins in an on-chain account (advanced: coin control). */
  listUtxos(account: string): Promise<Utxo[]>;
  /** Label an address, transaction or coin; null removes it. */
  setLabel(account: string, kind: LabelKind, reference: string, label: string | null): Promise<void>;
  /** Frozen coins are never picked automatically. */
  setFrozen(account: string, outpoint: string, frozen: boolean): Promise<void>;
  /** Rename (label) an account. */
  renameAccount(id: string, name: string): Promise<AccountInfo>;
  /** Hot on-chain accounts from this seed that have history but aren't added. */
  discoverHotAccounts(): Promise<FoundAccount[]>;
  /** Which of these descriptors (e.g. accounts 1…n from a hardware wallet) have history. */
  scanDescriptors(descriptors: string[]): Promise<FoundAccount[]>;
  /** Jade QR unlock: CBOR of Jade's `jade-pin` QR in, CBOR of the reply QR out (relayed via Jade's PIN server). */
  jadeQrPin(request: Uint8Array): Promise<Uint8Array>;
  /** Sync one account now (the Receive screen polls this while it waits). */
  syncAccount(id: string): Promise<void>;
  /** Quick check for a payment to a receive target (its `watchId`); true when something new arrived. */
  checkIncoming(account: string, watchId: string): Promise<boolean>;
  /** The Receive screen shows this on-chain address: watch it for payments (pushed over the chain server's websocket when it's a mempool server; no-op otherwise). Rejects if the address isn't the account's. */
  watchAddress(account: string, address: string): Promise<void>;
  /** On-chain transaction details; null for other rails. */
  txDetails(account: string, txid: string): Promise<TxDetails | null>;
  /** Speed up (RBF or CPFP) or cancel (RBF back to ourselves) an unconfirmed transaction. */
  prepareBump(account: string, txid: string, action: BumpAction, fee: FeePriority): Promise<SendQuote>;
  /** Send a prepared quote from an account that signs on this device. */
  confirmQuote(quote: SendQuote): Promise<Payment>;
  /** Technical details (descriptor, xpub, fingerprint…) for an account. */
  accountDetails(id: string): Promise<DetailRow[]>;
  /** Limits and fees when receiving with `method` needs a swap into this account; null when it doesn't. */
  receiveQuote(into: string, method: Method, amountSat: number | null): Promise<ReceiveQuote | null>;
  /** Check a hardware wallet export before adding it. */
  inspectDescriptor(descriptor: string): Promise<DescriptorPreview>;
  /** Trezor Connect signTransaction params for an unsigned PSBT. */
  trezorSignRequest(psbt: string): Promise<Record<string, unknown>>;
  /** Fold a Trezor-signed raw transaction back into the PSBT. */
  trezorMerge(psbt: string, signedTx: string): Promise<string>;
  /** BitBox02 over USB: one descriptor per account number, `wpkh([fp/84h/0h/{n}h]xpub…/<0;1>/*)` (`tr`/86h for Taproot). Unlocks and pairs first if needed. */
  bitboxDescriptors(script: "wpkh" | "tr", accounts: number[]): Promise<string[]>;
  /** Sign an unsigned PSBT (base64) on a BitBox02 over USB; returns the signed PSBT for `resume`. */
  bitboxSign(psbt: string): Promise<string>;
  /** Show receive address `index` of a BitBox02 account on the device (checked against ours); returns it. */
  bitboxShowAddress(account: string, index: number): Promise<string>;
  /** BitBox02 progress (status, pairing code, paired). Returns an unsubscribe. */
  onBitbox(cb: (e: BitboxEvent) => void): Promise<() => void>;
  /** Jade over Bluetooth: one descriptor per account number, `wpkh([fp/84h/0h/{n}h]xpub…/<0;1>/*)` (`tr`/86h for Taproot, firmware 1.0.34+). Connects and unlocks first if needed. */
  jadeDescriptors(script: "wpkh" | "tr", accounts: number[]): Promise<string[]>;
  /** Sign an unsigned PSBT (base64) on Jade over Bluetooth; returns the signed PSBT for `resume`. */
  jadeSign(psbt: string): Promise<string>;
  /** Show receive address `index` of a hardware account on Jade over Bluetooth (checked against ours); returns it. */
  jadeShowAddress(account: string, index: number): Promise<string>;
  /** Jade Bluetooth progress (scanning, connecting, unlock, confirm, connected). Returns an unsubscribe. */
  onJade(cb: (e: JadeEvent) => void): Promise<() => void>;
  /** Our own cosigner key to share when setting up a multisig. */
  cosignerKey(): Promise<string>;
  sync(): Promise<void>;
  activity(limit: number): Promise<Payment[]>;

  parse(input: string): Promise<Parsed>;
  planPayment(input: string, amountSat: number | null, from: string | null): Promise<Plan>;
  planMove(from: string, to: string, amountSat: number): Promise<Plan>;
  /** Price the first step; nothing moves yet. */
  prepare(plan: Plan, fee: FeePriority, style?: SpendStyle, spend?: SpendOptions): Promise<Prepared>;
  confirm(prepared: Prepared, fee: FeePriority): Promise<Execution>;
  resume(paused: Execution, signedPsbt: string, fee: FeePriority): Promise<Execution>;

  receive(into: string, method: Method, amountSat: number | null, description: string | null): Promise<ReceiveTarget>;
  redeem(token: string, into: string | null): Promise<Payment>;
  createToken(from: string, amountSat: number, memo: string | null): Promise<Payment>;

  layout(): Promise<Layout>;
  saveLayout(layout: Layout): Promise<void>;
  btcPrice(currency: string): Promise<number | null>;
  feeRates(): Promise<FeeRates>;
  settings(): Promise<Settings>;
  /** Applied the next time the wallet unlocks. */
  saveSettings(s: Settings): Promise<void>;
  /** Accounts that couldn't open at unlock, with the reason. */
  unavailableAccounts(): Promise<{ id: string; reason: string }[]>;

  lightningAddress(account: string): Promise<LightningAddress | null>;
  lightningAddressAvailable(account: string, username: string): Promise<boolean>;
  registerLightningAddress(account: string, username: string): Promise<LightningAddress>;
  /** Called when an account changes (payment in or out). Returns an unsubscribe. */
  onChange(cb: () => void): Promise<() => void>;
  /** Called with the account id when that account's balance or history changes (payment seen, confirmation, history finished loading). Returns an unsubscribe. */
  onWalletChanged(cb: (account: string) => void): Promise<() => void>;
}

function inTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

let instance: Promise<WalletApi> | null = null;

export function api(): Promise<WalletApi> {
  if (!instance) {
    instance = inTauri()
      ? import("./tauri").then((m) => m.tauriApi)
      : import("./mock").then((m) => m.mockApi);
  }
  return instance;
}
