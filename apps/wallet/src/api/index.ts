import type {
  AccountInfo,
  DescriptorPreview,
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
  revealPhrase(passcode: string): Promise<string>;

  accounts(): Promise<AccountInfo[]>;
  addAccount(spec: NewAccount): Promise<AccountInfo>;
  /** Only empty accounts can be removed. */
  removeAccount(id: string): Promise<void>;
  /** Hot on-chain accounts only. */
  payjoinReceive(account: string): Promise<boolean>;
  setPayjoinReceive(account: string, enabled: boolean): Promise<void>;
  /** Check a hardware wallet export before adding it. */
  inspectDescriptor(descriptor: string): Promise<DescriptorPreview>;
  /** Trezor Connect signTransaction params for an unsigned PSBT. */
  trezorSignRequest(psbt: string): Promise<Record<string, unknown>>;
  /** Fold a Trezor-signed raw transaction back into the PSBT. */
  trezorMerge(psbt: string, signedTx: string): Promise<string>;
  /** Our own cosigner key to share when setting up a multisig. */
  cosignerKey(): Promise<string>;
  sync(): Promise<void>;
  activity(limit: number): Promise<Payment[]>;

  parse(input: string): Promise<Parsed>;
  planPayment(input: string, amountSat: number | null, from: string | null): Promise<Plan>;
  planMove(from: string, to: string, amountSat: number): Promise<Plan>;
  /** Price the first step; nothing moves yet. */
  prepare(plan: Plan, fee: FeePriority, style?: SpendStyle): Promise<Prepared>;
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
