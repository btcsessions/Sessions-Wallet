import { invoke } from "@tauri-apps/api/core";
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
  cosignerKey: (account) => invoke("cosigner_key", { account }),
  sync: () => invoke("sync"),
  activity: (limit) => invoke("activity", { limit }),

  parse: (input) => invoke("parse", { input }),
  planPayment: (input, amountSat, from) => invoke("plan_payment", { input, amountSat, from }),
  planMove: (from, to, amountSat) => invoke("plan_move", { from, to, amountSat }),
  prepare: (plan, fee) => invoke("prepare", { plan, fee }),
  confirm: (prepared, fee) => invoke("confirm", { prepared, fee }),
  resume: (paused, signedPsbt, fee) => invoke("resume", { paused, signedPsbt, fee }),

  receive: (into, method, amountSat, description) => invoke("receive", { into, method, amountSat, description }),
  redeem: (token, into) => invoke("redeem", { token, into }),
  createToken: (from, amountSat, memo) => invoke("create_token", { from, amountSat, memo }),

  layout: () => invoke("layout"),
  saveLayout: (layout) => invoke("save_layout", { layout }),
  btcPrice: (currency) => invoke("btc_price", { currency }),
  feeRates: () => invoke("fee_rates"),
};
