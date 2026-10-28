import type {
  LinkOffer,
  LinkStatus,
  SyncStatus,
  SyncDirection,
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
  Deposit,
  ClaimResult,
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
  BackupTest,
  BackupTestMethod,
  RestoreResult,
  Prefs,
  BiometricStatus,
  AlertsStatus,
  Arrival,
  NostrIdentity,
  GroupJoinFound,
  GroupView,
  GroupJoiner,
  GroupList,
  InviteFilter,
  NwcConnection,
  NwcNew,
  VaultKey,
  NewVault,
  VaultImportPreview,
  VaultImport,
  VaultDetails,
  VaultExportFormat,
  ExportFile,
  VaultSignatures,
  Proposal,
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

  /** Fingerprint / face unlock: sensor, enrolment and whether it's on. Never prompts, never rejects. */
  biometricStatus(): Promise<BiometricStatus>;
  /**
   * Turn on fingerprint / face unlock: checks the passcode (wallet stays as it is), then shows the
   * "Turn on … unlock" prompt and saves the passcode behind it. Rejects with a `BiometricError`.
   */
  enableBiometric(passcode: string): Promise<BiometricStatus>;
  /** Turn it off: deletes the saved passcode and its key (also clears `invalidated`). */
  disableBiometric(): Promise<BiometricStatus>;
  /**
   * Lock screen: shows the "Unlock Sessions Wallet" prompt ("Use passcode" to back out) and unlocks like
   * `unlock(passcode)`. Rejects with a `BiometricError`; see `BiometricErrorCode` for what to do with each.
   */
  unlockBiometric(): Promise<void>;

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
  /** Stop backing up the phrase with a passkey (phrase wallets only). Replace = this, then enablePasskeyBackup. */
  disablePasskeyBackup(): Promise<BackupStatus>;
  backupStatus(): Promise<BackupStatus>;
  /** Back up the state now and wait for the result. */
  backupNow(): Promise<BackupStatus>;
  /** The user confirmed they wrote the recovery phrase down. */
  setPhraseBackedUp(done: boolean): Promise<BackupStatus>;
  /**
   * "Test my backup": checks, changing nothing, that the passkey / typed phrase / online backup would restore this
   * wallet (see `BackupTestMethod`). Unlocked only. Resolves even when the test fails (`ok: false`, `message` says
   * why); rejects with a `BackupTestError` when it couldn't run (`cancelled` passkey prompt: say nothing).
   * A pass updates `BackupStatus.lastTestedAt` (and emits `backup://status`).
   */
  testBackup(method: BackupTestMethod, phrase?: string): Promise<BackupTest>;
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
  /** `account`: a multisig vault (registers it on the device first if needed; partial signing is fine). */
  bitboxSign(psbt: string, account?: string): Promise<string>;
  /** Show receive address `index` of a BitBox02 account on the device (checked against ours); returns it. */
  bitboxShowAddress(account: string, index: number): Promise<string>;
  /** Which receive index `address` is, for a single-sig hardware account (to show it on the device). */
  hardwareAddressIndex(account: string, address: string): Promise<number>;
  /** BitBox02 progress (status, pairing code, paired). Returns an unsubscribe. */
  onBitbox(cb: (e: BitboxEvent) => void): Promise<() => void>;
  /** Jade over Bluetooth: one descriptor per account number, `wpkh([fp/84h/0h/{n}h]xpub…/<0;1>/*)` (`tr`/86h for Taproot, firmware 1.0.34+). Connects and unlocks first if needed. */
  jadeDescriptors(script: "wpkh" | "tr", accounts: number[]): Promise<string[]>;
  /** Sign an unsigned PSBT (base64) on Jade over Bluetooth; returns the signed PSBT for `resume`. */
  jadeSign(psbt: string, account?: string): Promise<string>;
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
  /** On-chain deposits to a Spending account not yet in its balance (usually none). */
  spendingDeposits(account: string): Promise<Deposit[]>;
  /**
   * Claim a deposit, paying up to `maxFeeSat` from it. `instant`: claim before
   * it has its confirmations, for `instantFeeSat` (pass at least that); without
   * it a deposit short of confirmations is refused with an error.
   */
  claimDeposit(account: string, txid: string, vout: number, maxFeeSat: number, instant?: boolean): Promise<ClaimResult>;
  /** Send a deposit back to an on-chain address instead (needs its confirmations). Returns the refund txid. */
  refundDeposit(account: string, txid: string, vout: number, address: string, feeRateSatVb: number): Promise<string>;
  /** Called when an account changes (payment in or out). Returns an unsubscribe. */
  onChange(cb: () => void): Promise<() => void>;
  /** Multisig: this phone's key for a new vault (next unused BIP48 account). */
  vaultPhoneKey(): Promise<VaultKey>;
  /** A cosigner key from a scanned / pasted / opened export (`[fp/48h/0h/0h/2h]xpub`, a device's multisig file). */
  vaultReadKey(text: string): Promise<VaultKey>;
  /** BitBox02 / Jade over Bluetooth: the device's multisig key (`account`: BIP48 account, usually 0). */
  bitboxMultisigKey(account: number): Promise<string>;
  /** Stop a BitBox02 request in progress (the screen was closed, or it's stuck). */
  bitboxCancel(): Promise<void>;
  /** Stop a Jade request in progress. */
  jadeCancel(): Promise<void>;
  /** Recent wallet log lines for support (no keys; addresses and amounts taken out). */
  diagnosticLog(): Promise<string>;
  jadeMultisigKey(account: number): Promise<string>;
  /** Register the vault on the device if it isn't yet (the user confirms there). True when it registered now. */
  bitboxRegisterMultisig(account: string): Promise<boolean>;
  jadeRegisterMultisig(account: string): Promise<boolean>;
  /** Show vault receive address `index` on the device (checked against ours). */
  bitboxShowMultisigAddress(account: string, index: number): Promise<string>;
  jadeShowMultisigAddress(account: string, index: number): Promise<string>;
  vaultCreate(vault: NewVault): Promise<AccountInfo>;
  /** Read a multisig wallet file or QR (descriptor, Coldcard text, BSMS, Caravan / Specter JSON) for review. */
  vaultImportPreview(text: string): Promise<VaultImportPreview>;
  vaultImport(vault: VaultImport): Promise<AccountInfo>;
  vaultDetails(account: string): Promise<VaultDetails>;
  vaultSetKeyLabel(account: string, xpub: string, label: string): Promise<void>;
  vaultExport(account: string, format: VaultExportFormat): Promise<ExportFile>;
  /** Which keys have signed this PSBT. */
  vaultSignatures(account: string, psbt: string): Promise<VaultSignatures>;
  /** Add another signer's signatures (a PSBT of the same transaction) to `psbt`. */
  vaultCombine(account: string, psbt: string, other: string): Promise<string>;
  proposals(account?: string): Promise<Proposal[]>;
  /** Save or update (by id; empty id = new). */
  proposalSave(p: Proposal): Promise<Proposal>;
  proposalDelete(id: string): Promise<void>;
  /** Save (save-as screen; Downloads on desktop) or share a file. False if the user backed out. */
  fileExport(file: ExportFile, share: boolean): Promise<boolean>;
  /** Group vaults: make one (the group plus a join code). */
  groupCreate(req: { name: string; threshold: number; total: number; myName: string }): Promise<GroupView>;
  groupView(groupId: string): Promise<GroupView>;
  /** Creator: a fresh join code. */
  groupCode(groupId: string): Promise<GroupView>;
  groupRequests(groupId: string): Promise<GroupJoiner[]>;
  groupApprove(groupId: string, joiner: GroupJoiner): Promise<GroupView>;
  groupIgnore(groupId: string, pubkey: string): Promise<void>;
  groupList(): Promise<GroupList>;
  /** Account ids of vaults run as group vaults. */
  groupAccounts(): Promise<string[]>;
  /** Abandon a group vault that isn't set up yet. */
  groupDelete(groupId: string): Promise<void>;

  // Linked devices.
  /** New device (no wallet yet): the QR to scan from the wallet device. */
  linkOffer(): Promise<LinkOffer>;
  /** New device: has the wallet device scanned it / sent the wallet yet? */
  linkPoll(): Promise<LinkStatus>;
  /** New device: create the wallet that arrived, with this device's passcode. Call `restoreState` next. */
  linkFinish(passcode: string): Promise<void>;
  linkCancel(): Promise<void>;
  /** Wallet device: read the new device's QR; returns the check code both screens show. */
  linkScan(qr: string): Promise<{ code: string }>;
  /** Wallet device: codes match: send the wallet (checks the passcode). */
  linkSend(passcode: string): Promise<void>;
  syncStatus(): Promise<SyncStatus>;
  /** A secret is on screen: block screenshots, recording, previews and overlay taps. */
  setSecureScreen(on: boolean): Promise<void>;
  syncNow(direction: SyncDirection): Promise<SyncStatus>;
  groupRespond(groupId: string, accept: boolean): Promise<void>;
  groupSetName(name: string): Promise<void>;
  groupPropose(groupId: string, slots: { holder: string; label: string }[]): Promise<GroupView>;
  groupSubmitKey(groupId: string, slot: number, key: string, device: string | null): Promise<GroupView>;
  groupFinalize(groupId: string): Promise<GroupView>;
  /** Checked address #0 on my device: adds the vault here and tells the group. */
  groupConfirm(groupId: string): Promise<AccountInfo>;
  /** Group vaults: what a join code (or link) is for. */
  groupJoinLookup(code: string): Promise<GroupJoinFound>;
  /** Ask to join; the creator approves, then the invite arrives. */
  groupJoinSend(code: string, name: string): Promise<void>;
  /** The wallet's Nostr identity for group vaults. */
  nostrIdentity(): Promise<NostrIdentity>;
  /** `socialNpub: ""` unlinks. */
  nostrUpdate(update: { socialNpub?: string; invites?: InviteFilter; discoverable?: boolean }): Promise<NostrIdentity>;
  /** Nostr Wallet Connect: apps allowed to use Spending within a budget. */
  nwcList(): Promise<NwcConnection[]>;
  /** Returns the connection link (nostr+walletconnect://…) to give the app. */
  nwcCreate(req: NwcNew): Promise<{ connection: NwcConnection; uri: string }>;
  /** `budgetSat: null` removes the limit. */
  nwcUpdate(id: string, patch: { name?: string; enabled?: boolean; budgetSat?: number | null; budgetPeriod?: NwcConnection["budgetPeriod"] }): Promise<NwcConnection>;
  nwcDelete(id: string): Promise<void>;
  nwcUri(id: string): Promise<string>;
  /** Payment alerts: what the phone allows, whether the watch service runs. Never rejects. */
  alertsStatus(): Promise<AlertsStatus>;
  /** The notification permission prompt (or its system settings page when refused before). */
  alertsRequestNotifications(): Promise<AlertsStatus>;
  /** Ask to use the network while the phone dozes (a system dialog). */
  alertsRequestUnrestricted(): Promise<AlertsStatus>;
  /** `wallet://payment`: a payment just arrived (the Receive screen's tick, toasts). Returns an unsubscribe. */
  onPayment(cb: (a: Arrival) => void): Promise<() => void>;
  /** Called with the account id when that account's balance or history changes (payment seen, confirmation, history finished loading). Returns an unsubscribe. */
  onWalletChanged(cb: (account: string) => void): Promise<() => void>;
}

declare const __APP_BUILD__: boolean;

function inTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

let instance: Promise<WalletApi> | null = null;

export function api(): Promise<WalletApi> {
  if (!instance) {
    // App builds (`tauri build` sets TAURI_ENV_PLATFORM) never include the
    // mock: the constant lets the bundler drop it.
    if (__APP_BUILD__) {
      instance = import("./tauri").then((m) => m.tauriApi);
    } else {
      instance = inTauri() ? import("./tauri").then((m) => m.tauriApi) : import("./mock").then((m) => m.mockApi);
    }
  }
  return instance;
}
