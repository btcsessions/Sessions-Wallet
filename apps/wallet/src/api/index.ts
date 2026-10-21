import type {
  AccountInfo,
  Execution,
  FeeRates,
  FeePriority,
  Layout,
  Method,
  NewAccount,
  Parsed,
  Payment,
  Plan,
  Prepared,
  ReceiveTarget,
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
  /** Our own cosigner key to share when setting up a multisig. */
  cosignerKey(account: number): Promise<string>;
  sync(): Promise<void>;
  activity(limit: number): Promise<Payment[]>;

  parse(input: string): Promise<Parsed>;
  planPayment(input: string, amountSat: number | null, from: string | null): Promise<Plan>;
  planMove(from: string, to: string, amountSat: number): Promise<Plan>;
  /** Price the first step; nothing moves yet. */
  prepare(plan: Plan, fee: FeePriority): Promise<Prepared>;
  confirm(prepared: Prepared, fee: FeePriority): Promise<Execution>;
  resume(paused: Execution, signedPsbt: string, fee: FeePriority): Promise<Execution>;

  receive(into: string, method: Method, amountSat: number | null, description: string | null): Promise<ReceiveTarget>;
  redeem(token: string, into: string | null): Promise<Payment>;
  createToken(from: string, amountSat: number, memo: string | null): Promise<Payment>;

  layout(): Promise<Layout>;
  saveLayout(layout: Layout): Promise<void>;
  btcPrice(currency: string): Promise<number | null>;
  feeRates(): Promise<FeeRates>;
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
