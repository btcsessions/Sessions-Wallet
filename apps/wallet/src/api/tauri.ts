import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { WalletApi } from "./index";

// Command names match apps/wallet/src-tauri/src/commands.rs.
export const tauriApi: WalletApi = {
  isMock: false,
  status: () => invoke("status"),
  createWallet: (passcode, words) => invoke("create_wallet", { passcode, words }),
  restoreWallet: (phrase, passcode) => invoke("restore_wallet", { phrase, passcode }),
  unlock: (passcode) => invoke("unlock", { passcode }),
  lock: () => invoke("lock"),
  revealPhrase: (passcode) => invoke("reveal_phrase", { passcode }),

  accounts: () => invoke("accounts"),
  addAccount: (spec) => invoke("add_account", { spec }),
  removeAccount: (id) => invoke("remove_account", { id }),
  payjoinReceive: (account) => invoke("payjoin_receive", { account }),
  setPayjoinReceive: (account, enabled) => invoke("set_payjoin_receive", { account, enabled }),
  cosignerKey: () => invoke("cosigner_key"),
  sync: () => invoke("sync"),
  activity: (limit) => invoke("activity", { limit }),

  parse: (input) => invoke("parse", { input }),
  planPayment: (input, amountSat, from) => invoke("plan_payment", { input, amountSat, from }),
  planMove: (from, to, amountSat) => invoke("plan_move", { from, to, amountSat }),
  prepare: (plan, fee, style, spend) => invoke("prepare", { plan, fee, style: style ?? null, spend: spend ?? null }),
  confirm: (prepared, fee) => invoke("confirm", { prepared, fee }),
  resume: (paused, signedPsbt, fee) => invoke("resume", { paused, signedPsbt, fee }),

  receive: (into, method, amountSat, description) => invoke("receive", { into, method, amountSat, description }),
  redeem: (token, into) => invoke("redeem", { token, into }),
  createToken: (from, amountSat, memo) => invoke("create_token", { from, amountSat, memo }),

  layout: () => invoke("layout"),
  saveLayout: (layout) => invoke("save_layout", { layout }),
  btcPrice: (currency) => invoke("btc_price", { currency }),
  feeRates: () => invoke("fee_rates"),
  settings: () => invoke("settings"),
  saveSettings: (settings) => invoke("save_settings", { settings }),
  unavailableAccounts: () => invoke("unavailable_accounts"),
  lightningAddress: (account) => invoke("lightning_address", { account }),
  lightningAddressAvailable: (account, username) => invoke("lightning_address_available", { account, username }),
  registerLightningAddress: (account, username) => invoke("register_lightning_address", { account, username }),
  syncAccount: (id) => invoke("sync_account", { id }),
  txDetails: (account, txid) => invoke("tx_details", { account, txid }),
  prepareBump: (account, txid, action, fee) => invoke("prepare_bump", { account, txid, action, fee }),
  confirmQuote: (quote) => invoke("confirm_quote", { quote }),
  accountDetails: (id) => invoke("account_details", { id }),
  receiveQuote: (into, method, amountSat) => invoke("receive_quote", { into, method, amountSat }),
  inspectDescriptor: (descriptor) => invoke("inspect_descriptor", { descriptor }),
  trezorSignRequest: (psbt) => invoke("trezor_sign_request", { psbt }),
  trezorMerge: (psbt, signedTx) => invoke("trezor_merge", { psbt, signedTx }),
  onChange: (cb) => listen("wallet-changed", () => cb()),
};
