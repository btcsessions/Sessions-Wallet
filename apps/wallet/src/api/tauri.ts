import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { WalletApi } from "./index";
import type { JadeState } from "./types";

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
  listUtxos: (account) => invoke("list_utxos", { account }),
  setLabel: (account, kind, reference, label) => invoke("set_label", { account, kind, reference, label }),
  setFrozen: (account, outpoint, frozen) => invoke("set_frozen", { account, outpoint, frozen }),
  renameAccount: (id, name) => invoke("rename_account", { id, name }),
  discoverHotAccounts: () => invoke("discover_hot_accounts"),
  scanDescriptors: (descriptors) => invoke("scan_descriptors", { descriptors }),
  jadeQrPin: async (request) => new Uint8Array(await invoke<number[]>("jade_qr_pin", { request: Array.from(request) })),
  syncAccount: (id) => invoke("sync_account", { id }),
  checkIncoming: (account, watchId) => invoke("check_incoming", { account, watchId }),
  watchAddress: (account, address) => invoke("watch_address", { account, address }),
  txDetails: (account, txid) => invoke("tx_details", { account, txid }),
  prepareBump: (account, txid, action, fee) => invoke("prepare_bump", { account, txid, action, fee }),
  confirmQuote: (quote) => invoke("confirm_quote", { quote }),
  accountDetails: (id) => invoke("account_details", { id }),
  receiveQuote: (into, method, amountSat) => invoke("receive_quote", { into, method, amountSat }),
  inspectDescriptor: (descriptor) => invoke("inspect_descriptor", { descriptor }),
  trezorSignRequest: (psbt) => invoke("trezor_sign_request", { psbt }),
  trezorMerge: (psbt, signedTx) => invoke("trezor_merge", { psbt, signedTx }),
  bitboxDescriptors: (script, accounts) => invoke("bitbox_descriptors", { script, accounts }),
  bitboxSign: (psbt) => invoke("bitbox_sign", { psbt }),
  bitboxShowAddress: (account, index) => invoke("bitbox_show_address", { account, index }),
  onBitbox: async (cb) => {
    const offs = await Promise.all([
      listen<{ state: "connecting" | "unlock" | "confirm" }>("bitbox://status", (e) => cb({ kind: "status", state: e.payload.state })),
      listen<{ code: string }>("bitbox://pairing", (e) => cb({ kind: "pairing", code: e.payload.code })),
      listen("bitbox://paired", () => cb({ kind: "paired" })),
    ]);
    return () => offs.forEach((off) => off());
  },
  jadeDescriptors: (script, accounts) => invoke("jade_descriptors", { script, accounts }),
  jadeSign: (psbt) => invoke("jade_sign", { psbt }),
  jadeShowAddress: (account, index) => invoke("jade_show_address", { account, index }),
  onJade: async (cb) => {
    const offs = await Promise.all([
      listen<{ state: JadeState }>("jade://status", (e) => cb({ kind: "status", state: e.payload.state })),
      listen("jade://connected", () => cb({ kind: "connected" })),
    ]);
    return () => offs.forEach((off) => off());
  },
  onChange: (cb) => listen("wallet-changed", () => cb()),
  onWalletChanged: (cb) => listen<{ account: string }>("wallet://changed", (e) => cb(e.payload.account)),
};
