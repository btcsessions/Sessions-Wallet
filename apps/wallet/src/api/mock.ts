// Browser-only stand-in for the Rust core so the UI can be designed and
// reviewed without a native build. Never ships in the Tauri app.

import type { WalletApi } from "./index";
import { BackupTestError, BiometricError } from "./types";
import type { AlertsStatus, Arrival, BiometricStatus, KeySource, NwcConnection, Proposal, VaultKey } from "./types";
import type { AccountInfo, BackupStatus, BackupTest, BitboxEvent, ClaimResult, Deposit, JadeEvent, Layout, Leg, LightningAddress, Method, Payment, PaymentRequest, Plan, Prefs, RestoreResult, Settings } from "./types";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const bitboxListeners = new Set<(e: BitboxEvent) => void>();
let bitboxPaired = false;
/** Stand-in for a real unsigned PSBT, about the size of a one-input payment. */
const MOCK_PSBT = "cHNidP8LMFV6n8TpDjNYfaLH7BE2W4Clyu8UOV6DqM3yFzxhhqvQ9Ro/ZImu0/gdQmeMsdb7IEVqj7TZ/iNIbZK33AEmS3CVut8EKU5zmL3iByxRdpvA5QovVHmew+gNMld8ocbrEDVaf6TJ7hM4XYKnzPEWO2CFqs/0GT5jiK3S9xxBZouw1fofRGmOs9j9IkdskbbbACVKb5S53gMoTXKXvOEGK1B1mr/kCS5TeJ3C5wwxVnugxeoPNFl+o8jtEjdcgabL8BU6X4SpzvMYPWKHrNH2G0Bliq/U+R5DaI2y1/whRmuQtdr/JEluk7jdAidMcZa74AUqT3SZvuMILVJ3nMHmCzBVep/E6Q4zWH2ix+wRNluApcrvFDleg6jN8hc8YYar0PUaP2SJrtP4HUJnjLHW+yBFao+02f4jSG2St9wBJktwlbrfBClOc5i94gcsUXabwOUKL1R5nsPoDTJXfKHG6xA1Wn+kye4TOF2Cp8zxFjtgharP9Bk+Y4it0vccQWaLsNX6H0RpjrPY/SJHbJG22wAlSm+Uud4DKE1yl7zhBitQdZq/5AkuU3idwucMMVZ7oMXqDzRZfqPI7RI3XIGmy/AVOl+Eqc7zGD1ih6zR9htAZYqv1PkeQ2iNstf8IUZrkLXa/yRJbpO43QInTHGWu+AFKk90mb7jCC1Sd5zB5gswVXqfxOkOM1h9osfsETZbgKXK7xQ5XoOozfIXPGGGq9D1Gj9kia7T+B1CZ4yx1g==";
const mockBitbox = (e: BitboxEvent) => bitboxListeners.forEach((cb) => cb(e));
const jadeListeners = new Set<(e: JadeEvent) => void>();
const mockJade = (e: JadeEvent) => jadeListeners.forEach((cb) => cb(e));
let jadeConnected = false;
/** Like the real thing: scan, connect and unlock once, then reuse the connection. */
async function mockJadeConnect() {
  if (jadeConnected) return;
  mockJade({ kind: "status", state: "scanning" });
  await sleep(900);
  mockJade({ kind: "status", state: "connecting" });
  await sleep(700);
  mockJade({ kind: "status", state: "unlock" });
  await sleep(2000);
  jadeConnected = true;
  mockJade({ kind: "connected" });
}
const now = () => Math.floor(Date.now() / 1000);

const DEMO_PHRASE =
  "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

let status: "empty" | "locked" | "unlocked" = "empty";
/** Like the real thing: what `unlock` does once the passcode is right. */
function markUnlocked() {
  status = "unlocked";
}

let mockBiometric = false;
const alerts: AlertsStatus = { supported: true, notifications: "prompt", watching: false, unrestricted: false };
const paymentListeners = new Set<(a: Arrival) => void>();
let nwc: { c: NwcConnection; uri: string }[] = [];

// ---- Multisig (mock) ----
const MOCK_XPUB = "xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8";
function fakeKey(fp: string, account = 0): string {
  return `[${fp}/48h/0h/${account}h/2h]${MOCK_XPUB}`;
}
function mkKey(key: string, label: string, source: KeySource): VaultKey {
  const fp = key.slice(1, 9);
  return { fingerprint: fp, path: "m/48'/0'/0'/2'", xpub: key.split("]")[1], key, label, source, ours: source.type === "phone" };
}
const vaults = new Map<string, { keys: VaultKey[]; threshold: number }>();
let proposalsMock: Proposal[] = [];
const randFp = () => Math.floor(Math.random() * 0xffffffff).toString(16).padStart(8, "0");

let lastReceive: { account: string; method: Method; amountSat: number | null } | null = null;

/** Design preview / tests: pretend a payment to the last Receive request just arrived. */
function mockArrive(amountSat?: number) {
  if (!lastReceive) return;
  const a = find(lastReceive.account);
  const sat = amountSat ?? lastReceive.amountSat ?? 21_000;
  const deposit = a.rail === "spark" && lastReceive.method === "bitcoinAddress";
  const onchain = lastReceive.method === "bitcoinAddress";
  const id = `m${Date.now()}`;
  if (!deposit) {
    activity.unshift(p(id, a.id, "incoming", sat, 0, lastReceive.method, now(), null, onchain ? "pending" : "complete"));
    if (onchain) a.balance.pendingIn += sat;
    else a.balance.confirmed += sat;
  }
  const arrival: Arrival = { account: a.id, accountName: a.name, id, amountSat: sat, kind: deposit ? "deposit" : onchain ? "incoming" : "received", onchain };
  paymentListeners.forEach((cb) => cb(arrival));
}
if (typeof window !== "undefined") (window as unknown as { __mockArrive?: typeof mockArrive }).__mockArrive = mockArrive;

function biometricStatus(): BiometricStatus {
  return { available: true, enabled: mockBiometric && status !== "empty", invalidated: false, kind: "biometric", reason: null };
}

// Backup: succeeds after a short delay, like a good connection to the relays.
const MOCK_RELAYS = ["wss://relay.damus.io", "wss://nos.lol", "wss://relay.primal.net", "wss://nostr.mom", "wss://relay.snort.social", "wss://nostr-pub.wellorder.net"];
let backup: BackupStatus = { passkey: false, phrase: false, seedOrigin: "phrase", lastBackupAt: null, lastError: null, pending: false, relays: [], lastTestedAt: null };
const backupListeners = new Set<(s: BackupStatus) => void>();
let mockPrefs: Prefs = {};
function setBackup(patch: Partial<BackupStatus>) {
  backup = { ...backup, ...patch };
  backupListeners.forEach((cb) => cb({ ...backup }));
}
/** A change to the wallet: back up a moment later. */
function mockBackupSoon() {
  if (status !== "unlocked") return;
  setBackup({ pending: true });
  setTimeout(() => setBackup({ pending: false, lastBackupAt: now(), lastError: null, relays: MOCK_RELAYS }), 1500);
}
const MOCK_RESTORED: RestoreResult = { found: true, version: 1_761_000_000, backedUpAt: now() - 3 * 86_400, accounts: 4, labels: 12, error: null };

let accounts: AccountInfo[] = [];
let activity: Payment[] = [];
const mockUtxos = new Map<string, import("./types").Utxo[]>();

function seedDemo() {
  accounts = [
    {
      id: "spending",
      name: "Spending",
      rail: "spark",
      custody: { type: "hot" },
      balance: { confirmed: 184_210, pendingIn: 0, pendingOut: 0 },
      receives: ["sparkAddress", "bolt11", "bitcoinAddress"],
      pays: ["sparkAddress", "bolt11", "bolt12", "lightningAddress", "lnurl", "bitcoinAddress"],
    },
    {
      id: "hw-1",
      name: "Cold storage",
      rail: "onchain",
      custody: { type: "hardware", device: "SeedSigner", fingerprint: "a1b2c3d4" },
      balance: { confirmed: 4_250_000, pendingIn: 120_000, pendingOut: 0 },
      receives: ["bitcoinAddress"],
      pays: ["bitcoinAddress"],
      group: "hw:a1b2c3d4",
      accountIndex: 0,
    },
    {
      id: "hw-2",
      name: "Kids' savings",
      rail: "onchain",
      custody: { type: "hardware", device: "SeedSigner", fingerprint: "a1b2c3d4" },
      balance: { confirmed: 610_000, pendingIn: 0, pendingOut: 0 },
      receives: ["bitcoinAddress"],
      pays: ["bitcoinAddress"],
      group: "hw:a1b2c3d4",
      accountIndex: 1,
    },
  ];
  const t = now();
  activity = [
    p("a1", "spending", "outgoing", 4_800, 0, "bolt11", t - 3_600, "Coffee at Analog"),
    p("a2", "hw-1", "incoming", 120_000, 0, "bitcoinAddress", t - 7_200, null, "pending"),
    p("a3", "spending", "incoming", 21_000, 0, "lightningAddress", t - 86_400, "Zap from Bitcast listener"),
    p("a4", "hw-1", "internal", 150_000, 412, "bitcoinAddress", t - 3 * 86_400, "Transferred to Spending"),
    p("a5", "spending", "outgoing", 63_000, 12, "bolt11", t - 6 * 86_400, "Sat Market vendor"),
  ];
}

function p(
  id: string,
  account: string,
  direction: Payment["direction"],
  amountSat: number,
  feeSat: number,
  method: Method,
  timestamp: number,
  description: string | null,
  status: Payment["status"] = "complete",
): Payment {
  return { id, account, direction, amountSat, feeSat, method, timestamp, description, status };
}

let mockLnAddress: LightningAddress | null = null;
/** One deposit stuck over the automatic claim-fee limit, so the UI can be built. */
let mockDeposits: Deposit[] = [
  {
    txid: "5e3b7c1f0a9d2e4b6c8a1f3e5d7b9c0a2e4f6a8c0e2d4b6a8c0e2f4a6c8e0d2b",
    vout: 1,
    amountSat: 85_000,
    confirmations: 4,
    confirmationsNeeded: 3,
    status: "needsApproval",
    claimFeeSat: 1_200,
    instantFeeSat: null,
    instantConfirmations: null,
    error: "Claiming it costs 1200 sats in network fees right now, more than the automatic limit. Approve the fee to claim it, or wait for fees to drop.",
    refundTxid: null,
  },
];
const mockPayjoin = new Set<string>();

const mockElectrumDefaults = [
  "ssl://electrum.blockstream.info:50002",
  "ssl://fulcrum.bullbitcoin.com:50002",
  "ssl://electrum2.bluewallet.io:443",
  "ssl://electrum.acinq.co:50002",
];
let mockSettings: Settings = {
  esploraUrl: null,
  electrumUrl: null,
  chainServer: mockElectrumDefaults[0],
  defaultElectrumServers: mockElectrumDefaults,
  hasBreezKey: true,
};

let layout: Layout = {
  version: 1,
  advanced: false,
  modules: [
    { id: "balance", size: "large" },
    { id: "pay-receive", size: "medium" },
    { id: "activity", size: "large" },
    { id: "hardware:hw-1", size: "small" },
    { id: "spending", size: "medium" },
  ],
};

function parseMock(input: string): PaymentRequest {
  const s = input.trim();
  const l = s.toLowerCase();
  if (l.startsWith("bitcoin:")) {
    const [addr, q = ""] = s.slice(8).split("?");
    const params = new URLSearchParams(q);
    const amount = params.get("amount");
    const opts: PaymentRequest[] = [];
    const ln = params.get("lightning");
    if (ln) opts.push({ type: "bolt11", invoice: ln.toLowerCase() });
    if (addr) opts.push({ type: "bitcoinAddress", address: addr, amountSat: amount ? Math.round(parseFloat(amount) * 1e8) : null });
    if (opts.length === 1) return opts[0];
    if (opts.length > 1) return { type: "unified", options: opts };
  }
  if (l.startsWith("lightning:")) return parseMock(s.slice(10));
  if (/^cashu[ab]/.test(l)) return { type: "cashuToken", token: s };
  if (/^ln(bc|tb|tbs|bcrt)/.test(l)) return { type: "bolt11", invoice: l };
  if (l.startsWith("lno1")) return { type: "bolt12", offer: l };
  if (l.startsWith("lnurl1")) return { type: "lnurl", lnurl: l };
  if (/^(sp|spt|sprt)1/.test(l)) return { type: "sparkAddress", address: l };
  if (/^(lq1|ex1|vjl)/.test(l)) return { type: "liquidAddress", address: s };
  if (/^(bc1|[13])[a-z0-9]{20,}/i.test(s)) return { type: "bitcoinAddress", address: s };
  if (/^[^@\s]+@[^@\s]+\.[a-z]{2,}$/.test(l)) return { type: "lightningAddress", address: l };
  throw "Couldn't recognise that payment request";
}

function method(r: PaymentRequest): Method | null {
  return r.type === "unified" ? null : r.type;
}

const RANK: Method[] = ["sparkAddress", "bolt11", "bolt12", "lightningAddress", "lnurl", "cashuToken", "liquidAddress", "bitcoinAddress"];

function planPayment(from: AccountInfo, req: PaymentRequest, amount: number): Plan | null {
  const options = req.type === "unified" ? req.options : [req];
  const sorted = [...options].sort((a, b) => RANK.indexOf(method(a)!) - RANK.indexOf(method(b)!));
  for (const o of sorted) {
    const m = method(o);
    if (m && from.pays.includes(m)) return { legs: [{ type: "toExternal", from: from.id, request: o }], amountSat: amount };
  }
  for (const o of sorted) {
    if (o.type === "liquidAddress") {
      const payMethod: Method | null = from.pays.includes("bolt11") ? "bolt11" : from.pays.includes("bitcoinAddress") ? "bitcoinAddress" : null;
      if (payMethod) return { legs: [{ type: "viaSwap", from: from.id, provider: "boltz", payMethod, request: o }], amountSat: amount };
    }
  }
  return null;
}

function bolt11Amount(inv: string): number | null {
  const m = /^ln(?:bcrt|tbs|tb|bc)(\d+)([munp]?)1/.exec(inv);
  if (!m) return null;
  const n = Number(m[1]);
  const mult = { "": 1e8, m: 1e5, u: 100, n: 0.1, p: 0.0001 }[m[2] as "" | "m" | "u" | "n" | "p"];
  return Math.floor(n * mult);
}

function requestAmount(r: PaymentRequest): number | null {
  if (r.type === "bitcoinAddress" || r.type === "liquidAddress") return r.amountSat ?? null;
  if (r.type === "bolt11") return bolt11Amount(r.invoice);
  if (r.type === "unified") return r.options.map(requestAmount).find((a) => a != null) ?? null;
  return null;
}

function legFrom(l: Leg) {
  return l.from;
}

function settle(first: Leg, rest: Leg[], paid: number, fee: number, amount: number) {
  const from = find(first.from);
  from.balance.confirmed -= paid + fee;
  const last = rest.length ? rest[rest.length - 1] : first;
  if (last.type === "toAccount") find(last.to).balance.confirmed += amount;
  const pay = p(
    `x${Date.now()}`,
    from.id,
    last.type === "toAccount" ? "internal" : "outgoing",
    amount,
    fee,
    first.type === "toAccount" ? first.method : first.type === "viaSwap" ? "liquidAddress" : (method(first.request) ?? "bolt11"),
    now(),
    last.type === "toAccount" ? `Transferred to ${find(last.to).name}` : null,
    from.rail === "onchain" ? "pending" : "complete",
  );
  activity.unshift(pay);
  return { payments: [pay], awaitingSignature: null, remaining: [], amountSat: amount };
}

function find(id: string) {
  const a = accounts.find((x) => x.id === id);
  if (!a) throw `Account not found: ${id}`;
  return a;
}

export const mockApi: WalletApi = {
  isMock: true,
  async status() {
    return status;
  },
  async createWallet() {
    await sleep(400);
    seedDemo();
    // A new wallet starts with Spending only.
    accounts = accounts.filter((a) => a.rail === "spark");
    accounts.forEach((a) => (a.balance = { confirmed: 0, pendingIn: 0, pendingOut: 0 }));
    activity = [];
    status = "unlocked";
    setBackup({ passkey: false, phrase: false, seedOrigin: "phrase", lastBackupAt: null, lastError: null, relays: [], lastTestedAt: null });
    mockBackupSoon();
    return DEMO_PHRASE;
  },
  async restoreWallet(phrase) {
    await sleep(600);
    if (phrase.trim().split(/\s+/).length < 12) throw "That recovery phrase is too short. It should be 12 or 24 words.";
    seedDemo();
    status = "unlocked";
    setBackup({ passkey: false, phrase: true, seedOrigin: "phrase", lastBackupAt: null, lastError: null, pending: true, relays: [], lastTestedAt: null });
  },
  async passkeySupported() {
    return true;
  },
  async createPasskeyWallet(passcode) {
    if (passcode.length < 6) throw "Use a passcode of at least 6 digits";
    await sleep(900);
    seedDemo();
    accounts = accounts.filter((a) => a.rail === "spark");
    accounts.forEach((a) => (a.balance = { confirmed: 0, pendingIn: 0, pendingOut: 0 }));
    activity = [];
    status = "unlocked";
    setBackup({ passkey: true, phrase: false, seedOrigin: "passkey", lastBackupAt: null, lastError: null, relays: [], lastTestedAt: null });
    mockBackupSoon();
  },
  async restoreWithPasskey(passcode) {
    if (passcode.length < 6) throw "Use a passcode of at least 6 digits";
    await sleep(1200);
    seedDemo();
    status = "unlocked";
    setBackup({ passkey: true, phrase: false, seedOrigin: "passkey", lastBackupAt: MOCK_RESTORED.backedUpAt, lastError: null, pending: false, relays: MOCK_RELAYS, lastTestedAt: null });
    return { ...MOCK_RESTORED };
  },
  async restoreState() {
    await sleep(900);
    setBackup({ pending: false, lastBackupAt: MOCK_RESTORED.backedUpAt, relays: MOCK_RELAYS });
    return { ...MOCK_RESTORED };
  },
  async enablePasskeyBackup() {
    await sleep(900);
    setBackup({ passkey: true });
    return { ...backup };
  },
  async disablePasskeyBackup() {
    await sleep(300);
    setBackup({ passkey: false });
    return { ...backup };
  },
  async backupStatus() {
    return { ...backup };
  },
  async backupNow() {
    setBackup({ pending: true });
    await sleep(800);
    setBackup({ pending: false, lastBackupAt: now(), lastError: null, relays: MOCK_RELAYS });
    return { ...backup };
  },
  async testBackup(method, phrase) {
    if (status !== "unlocked") throw new BackupTestError("locked", "the wallet is locked");
    let seedMatches: boolean | null = null;
    if (method === "phrase") {
      const words = (phrase ?? "").trim().toLowerCase().split(/\s+/).filter(Boolean);
      if (words.length !== 12 && words.length !== 24) {
        throw new BackupTestError("invalid_phrase", "That isn't a valid recovery phrase. Check each word and try again");
      }
      await sleep(400);
      seedMatches = words.join(" ") === DEMO_PHRASE;
    } else if (method === "passkey") {
      await sleep(900);
      seedMatches = true;
    } else {
      await sleep(600);
    }
    const hardwareWallets = accounts.filter((a) => a.custody.type === "hardware").length;
    const base: BackupTest = {
      ok: false, seedMatches, stateFound: false, stateVersion: null, localVersion: MOCK_RESTORED.version, upToDate: false,
      backedUpAt: null, relaysWithLatest: 0, relaysAnswered: 0, relaysTotal: MOCK_RELAYS.length,
      accounts: 0, hardwareWallets: 0, labels: 0, message: "",
    };
    if (seedMatches === false) {
      return { ...base, message: "That recovery phrase is for a different wallet. Check each word against what you wrote down." };
    }
    const lead = method === "passkey" ? "Your passkey restores this wallet" : method === "phrase" ? "Your recovery phrase restores this wallet" : "Your online backup is up to date";
    const result: BackupTest = {
      ...base, ok: true, stateFound: true, stateVersion: MOCK_RESTORED.version, upToDate: true,
      backedUpAt: backup.lastBackupAt ?? MOCK_RESTORED.backedUpAt, relaysWithLatest: 5, relaysAnswered: 6,
      accounts: accounts.length, hardwareWallets, labels: 143,
      message: `${lead}, with ${accounts.length} accounts, ${hardwareWallets} hardware wallets and 143 labels. 5 of 6 backup servers have the latest copy.`,
    };
    setBackup({ lastTestedAt: now() });
    return result;
  },
  async setPhraseBackedUp(done) {
    setBackup({ phrase: done });
    mockBackupSoon();
    return { ...backup };
  },
  async onBackupStatus(cb) {
    backupListeners.add(cb);
    return () => backupListeners.delete(cb);
  },
  async onStateRestored() {
    return () => {};
  },
  async prefs() {
    return structuredClone(mockPrefs);
  },
  async savePrefs(p) {
    mockPrefs = structuredClone(p);
    mockBackupSoon();
  },
  async unlock(passcode) {
    await sleep(300);
    if (passcode.length < 6) throw "Wrong passcode";
    markUnlocked();
  },
  async biometricStatus() {
    return biometricStatus();
  },
  async enableBiometric(passcode) {
    await sleep(300);
    if (status === "empty") throw new BiometricError("failed", "There's no wallet on this device yet");
    if (passcode.length < 6) throw new BiometricError("wrong_passcode", "wrong passcode");
    await sleep(600);
    mockBiometric = true;
    return biometricStatus();
  },
  async disableBiometric() {
    mockBiometric = false;
    return biometricStatus();
  },
  async unlockBiometric() {
    if (!biometricStatus().enabled) throw new BiometricError("not_enabled", "Biometric unlock isn't turned on. Enter your passcode");
    await sleep(600);
    markUnlocked();
  },
  async lock() {
    status = "locked";
  },
  async revealPhrase() {
    return DEMO_PHRASE;
  },

  async accounts() {
    return structuredClone(accounts);
  },
  async addAccount(spec) {
    mockBackupSoon();
    await sleep(500);
    const base = { balance: { confirmed: 0, pendingIn: 0, pendingOut: 0 } };
    let a: AccountInfo;
    const id = `${spec.kind}-${accounts.length + 1}`;
    switch (spec.kind) {
      case "ecash":
        a = { ...base, id, name: spec.name, rail: "ecash", custody: { type: "mint", url: spec.mintUrl ?? "" }, receives: ["bolt11"], pays: ["bolt11"] };
        break;
      case "multisig":
        a = {
          ...base, id, name: spec.name, rail: "onchain",
          custody: { type: "multisig", threshold: spec.threshold ?? 2, total: (spec.cosigners?.length ?? 2) + 1, localSigners: 1 },
          receives: ["bitcoinAddress"], pays: ["bitcoinAddress"],
        };
        break;
      case "hardware":
        a = { ...base, id, name: spec.name, rail: "onchain", custody: { type: "hardware", device: spec.device ?? "Hardware wallet", fingerprint: "a1b2c3d4" }, receives: ["bitcoinAddress"], pays: ["bitcoinAddress"], group: "hw:a1b2c3d4", accountIndex: accounts.filter((x) => x.group === "hw:a1b2c3d4").length };
        break;
      case "watchOnly":
        a = { ...base, id, name: spec.name, rail: "onchain", custody: { type: "watchOnly" }, receives: ["bitcoinAddress"], pays: [] };
        break;
      default:
        a = { ...base, id, name: spec.name, rail: "onchain", custody: { type: "hot" }, receives: ["bitcoinAddress"], pays: ["bitcoinAddress"], group: "hot", accountIndex: spec.accountIndex ?? accounts.filter((x) => x.group === "hot").length };
    }
    accounts.push(a);
    return a;
  },
  async syncAccount() {},
  async checkIncoming() {
    await sleep(150);
    return false;
  },
  async watchAddress() {},
  async renameAccount(id, name) {
    mockBackupSoon();
    const a = find(id);
    a.name = name.trim();
    return a;
  },
  async discoverHotAccounts() {
    await sleep(1200);
    const have = new Set(accounts.filter((a) => a.group === "hot").map((a) => a.accountIndex));
    return [
      { index: 0, balanceSat: 312_000, txCount: 4, descriptor: "wpkh([73c5da0a/84h/0h/0h]xpub…/<0;1>/*)" },
      { index: 1, balanceSat: 0, txCount: 2, descriptor: "wpkh([73c5da0a/84h/0h/1h]xpub…/<0;1>/*)" },
    ].filter((f) => !have.has(f.index));
  },
  async jadeQrPin(request) {
    await sleep(700);
    return request.slice(0, 16);
  },
  async scanDescriptors(descriptors) {
    await sleep(800);
    return descriptors.slice(0, 1).map((descriptor, i) => ({ index: i + 1, balanceSat: 50_000, txCount: 1, descriptor }));
  },
  async listUtxos(account) {
    const a = accounts.find((x) => x.id === account);
    if (a?.rail !== "onchain") return [];
    if (!mockUtxos.has(account)) {
      const t = (n: number) => n.toString(16).padStart(2, "0").repeat(32);
      mockUtxos.set(account, [
        { outpoint: `${t(1)}:0`, txid: t(1), vout: 0, amountSat: 2_500_000, address: "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu", label: "Kraken withdrawal", confirmations: 812, frozen: false, change: false },
        { outpoint: `${t(2)}:1`, txid: t(2), vout: 1, amountSat: 1_250_000, address: "bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g", label: "Sat Market sales", confirmations: 140, frozen: false, change: false },
        { outpoint: `${t(3)}:0`, txid: t(3), vout: 0, amountSat: 500_000, address: "bc1qp59yckz4ae5c4efgw2s5wfyvrz0ala7rgvuz8z", label: null, confirmations: 31, frozen: true, change: true },
        { outpoint: `${t(4)}:0`, txid: t(4), vout: 0, amountSat: 120_000, address: "bc1qgl5vlg0zdl7yvprgxj9fevsc6q6x5dmcyk3cn3", label: null, confirmations: 0, frozen: false, change: false },
      ]);
    }
    return mockUtxos.get(account)!;
  },
  async setLabel(account, kind, reference, label) {
    mockBackupSoon();
    if (kind !== "output") return;
    const u = mockUtxos.get(account)?.find((x) => x.outpoint === reference);
    if (u) u.label = label;
  },
  async setFrozen(account, outpoint, frozen) {
    mockBackupSoon();
    const u = mockUtxos.get(account)?.find((x) => x.outpoint === outpoint);
    if (u) u.frozen = frozen;
  },
  async txDetails(account, txid) {
    const a = accounts.find((x) => x.id === account);
    const p = activity.find((x) => x.id === txid);
    if (a?.rail !== "onchain" || !p) return null;
    const pending = p.status === "pending";
    return {
      txid: "4f3c2a91e7b05d68c1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8",
      confirmations: pending ? 0 : 42,
      blockHeight: pending ? null : 921_384,
      blockTime: pending ? null : p.timestamp,
      feeSat: p.feeSat || 412,
      feeRate: 2.9,
      vsize: 141,
      canSpeedUp: pending,
      canCancel: pending && p.direction !== "incoming",
      speedUpKind: pending ? (p.direction === "incoming" ? "cpfp" : "rbf") : null,
    };
  },
  async prepareBump(account, _txid, action) {
    await sleep(300);
    const a = find(account);
    return { account, method: "bitcoinAddress", amountSat: 0, feeSat: action === "cancel" ? 1_240 : 860, handle: "mock-bump", needsSignature: a.custody.type === "hot" ? null : { psbtBase64: "cHNidP8BAHECAAAAAQ==", signaturesNeeded: 1 }, notes: action === "cancel" ? [] : ["This replaces your earlier transaction with a faster one."] };
  },
  async confirmQuote(quote) {
    await sleep(300);
    return { id: "mock-bump-tx", account: quote.account, direction: "internal", amountSat: 0, feeSat: quote.feeSat, status: "pending", method: "bitcoinAddress", timestamp: now() };
  },
  async accountDetails(id) {
    const a = accounts.find((x) => x.id === id);
    const onchain = a?.rail === "onchain";
    return [
      ...(a?.custody.type === "hardware" ? [{ label: "Device", value: a.custody.device, mono: false }] : []),
      ...(onchain
        ? [
            { label: "Address type", value: "Native SegWit", mono: false },
            { label: "Fingerprint", value: a?.custody.type === "hardware" ? a.custody.fingerprint : "73c5da0a", mono: true },
            { label: "Derivation path", value: "m/84h/0h/0h", mono: true },
            { label: "Extended public key", value: "xpub6CatWdiZiodmUeTDp8LT5or8nmbKNcuyvz7WyksVFkKB4RHwCD3XyuvPEbvqAQY3rAPshWcMLoP2fMFMKHPJ4ZeZXYVUhLv1VMrjPC7PW6V", mono: true },
            { label: "Descriptor", value: "wpkh([73c5da0a/84h/0h/0h]xpub6CatWdiZiodmUeTDp8LT5or8nmbKNcuyvz7WyksVFkKB4RHwCD3XyuvPEbvqAQY3rAPshWcMLoP2fMFMKHPJ4ZeZXYVUhLv1VMrjPC7PW6V/<0;1>/*)", mono: true },
          ]
        : [{ label: "Network", value: "Lightning and Spark", mono: false }]),
      { label: "Receives with", value: (a?.receives ?? []).join(", "), mono: false },
      { label: "Account id", value: id, mono: true },
    ];
  },
  async receiveQuote(into, method, amountSat) {
    const a = accounts.find((x) => x.id === into);
    if (a?.rail !== "onchain" || method === "bitcoinAddress") return null;
    const pct = method === "bolt11" ? 0.5 : 0.25;
    const network = method === "bolt11" ? 530 : 1_090;
    const fee = amountSat ? Math.ceil((amountSat * pct) / 100) + network : null;
    return { provider: "Boltz", minSat: 25_000, maxSat: 25_000_000, feePercent: pct, networkFeeSat: network, totalFeeSat: fee, receiveSat: amountSat && fee ? amountSat - fee : null };
  },
  async inspectDescriptor(descriptor) {
    const d = descriptor.trim();
    if (d.length < 20) throw new Error("That doesn't look like a wallet export");
    const fp = /\[([0-9a-f]{8})/i.exec(d)?.[1] ?? "73c5da0a";
    return {
      descriptor: d.startsWith("[") ? `wpkh(${d}/0/*)` : d,
      fingerprint: fp.toLowerCase(),
      scriptType: d.startsWith("tr(") ? "Taproot" : "Native SegWit",
      firstAddress: "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu",
    };
  },
  async trezorSignRequest() {
    return { coin: "btc", inputs: [], outputs: [] };
  },
  async trezorMerge(psbt) {
    return psbt;
  },
  async bitboxDescriptors(script, accounts) {
    mockBitbox({ kind: "status", state: "connecting" });
    await sleep(400);
    mockBitbox({ kind: "status", state: "unlock" });
    await sleep(800);
    if (!bitboxPaired) {
      bitboxPaired = true;
      mockBitbox({ kind: "pairing", code: "XKQ7M 2RT9P\nHB4WD 8NJ3C" });
      await sleep(2500);
    }
    mockBitbox({ kind: "paired" });
    // BIP84 test vector key (abandon … about); fake for other accounts and Taproot.
    const purpose = script === "tr" ? 86 : 84;
    return accounts.map(
      (n) => `${script}([73c5da0a/${purpose}h/0h/${n}h]xpub6CatWdiZiodmUeTDp8LT5or8nmbKNcuyvz7WyksVFkKB4RHwCD3XyuvPEbvqAQY3rAPshWcMLoP2fMFMKHPJ4ZeZXYVUhLv1VMrjPC7PW6V/<0;1>/*)`,
    );
  },
  async bitboxSign(psbt) {
    mockBitbox({ kind: "status", state: "confirm" });
    await sleep(1500);
    return psbt;
  },
  async bitboxShowAddress() {
    mockBitbox({ kind: "status", state: "confirm" });
    await sleep(1200);
    return "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu";
  },
  async onBitbox(cb) {
    bitboxListeners.add(cb);
    return () => bitboxListeners.delete(cb);
  },
  async jadeDescriptors(script, accounts) {
    await mockJadeConnect();
    // BIP84 test vector key (abandon … about); fake for other accounts and Taproot.
    const purpose = script === "tr" ? 86 : 84;
    return accounts.map(
      (n) => `${script}([73c5da0a/${purpose}h/0h/${n}h]xpub6CatWdiZiodmUeTDp8LT5or8nmbKNcuyvz7WyksVFkKB4RHwCD3XyuvPEbvqAQY3rAPshWcMLoP2fMFMKHPJ4ZeZXYVUhLv1VMrjPC7PW6V/<0;1>/*)`,
    );
  },
  async jadeSign(psbt) {
    await mockJadeConnect();
    mockJade({ kind: "status", state: "confirm" });
    await sleep(1500);
    return psbt;
  },
  async jadeShowAddress() {
    await mockJadeConnect();
    mockJade({ kind: "status", state: "confirm" });
    await sleep(1200);
    return "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu";
  },
  async onJade(cb) {
    jadeListeners.add(cb);
    return () => jadeListeners.delete(cb);
  },
  async payjoinReceive(account) {
    return mockPayjoin.has(account);
  },
  async setPayjoinReceive(account, enabled) {
    if (enabled) mockPayjoin.add(account);
    else mockPayjoin.delete(account);
  },
  async removeAccount(id) {
    mockBackupSoon();
    accounts = accounts.filter((a) => a.id !== id);
    layout = { ...layout, modules: layout.modules.filter((m) => !m.id.endsWith(`:${id}`)) };
  },
  async cosignerKey() {
    const account = 0;
    return `[73c5da0a/48'/0'/${account}'/2']xpub6DkFAXWQ2dHxq2vatrt9qyA3bXYU4ToWQwCHbf5XB2mSTexcHZCeKS1VZYcPoBd5X8yVcbXFHJR9R8UCVpt82VX1VhR28mCyxUFL4r6KFrf`;
  },
  async sync() {
    await sleep(900);
  },
  async activity(limit) {
    return activity.slice(0, limit);
  },

  async parse(input) {
    const request = parseMock(input);
    return { request, amountSat: requestAmount(request) };
  },
  async planPayment(input, amountSat, from) {
    const req = parseMock(input);
    const amount = amountSat ?? requestAmount(req);
    if (!amount) throw "Amount required";
    const candidates = from ? [find(from)] : [...accounts].filter((a) => a.balance.confirmed >= amount);
    for (const a of candidates) {
      const plan = planPayment(a, req, amount);
      if (plan) return plan;
    }
    throw "None of your accounts can pay that";
  },
  async planMove(from, to, amountSat) {
    const a = find(from);
    const b = find(to);
    const shared = RANK.find((m) => a.pays.includes(m) && b.receives.includes(m));
    if (shared) return { legs: [{ type: "toAccount", from, to, method: shared }], amountSat };
    const hub = accounts.find((h) => h.id !== from && h.id !== to && h.rail === "spark");
    if (hub) {
      const m1 = RANK.find((m) => a.pays.includes(m) && hub.receives.includes(m));
      const m2 = RANK.find((m) => hub.pays.includes(m) && b.receives.includes(m));
      if (m1 && m2)
        return { legs: [{ type: "toAccount", from, to: hub.id, method: m1 }, { type: "toAccount", from: hub.id, to, method: m2 }], amountSat };
    }
    throw "No route between these accounts";
  },
  async prepare(plan, fee, style, spend) {
    await sleep(500);
    const first = find(legFrom(plan.legs[0]));
    const leg = plan.legs[0];
    const onchain = (leg.type === "toAccount" && leg.method === "bitcoinAddress") || (leg.type === "toExternal" && leg.request.type === "bitcoinAddress") || (leg.type === "viaSwap" && leg.payMethod === "bitcoinAddress");
    const rate = typeof fee === "number" ? fee : fee === "fast" ? 6 : fee === "slow" ? 2 : 4;
    const swapFee = leg.type === "viaSwap" ? Math.ceil(plan.amountSat * 0.001) + 120 : 0;
    const isPrivate = onchain && style === "private" && first.rail === "onchain";
    if (isPrivate && first.custody.type === "hardware") throw "Private send needs coins at two or more different addresses. Use Efficient for this one";
    const feeSat = onchain ? rate * (isPrivate ? 340 : 141) : 3;
    const have = first.balance.confirmed + first.balance.pendingIn;
    const drain = !!spend?.sendMax && onchain && first.rail === "onchain" && plan.legs.length === 1 && leg.type !== "viaSwap";
    if (drain && have <= feeSat) throw `This account's ${have.toLocaleString()} sats don't cover the network fee`;
    const amountSat = drain ? have - feeSat : plan.amountSat + swapFee;
    if (have < amountSat + feeSat) throw `Not enough funds: need ${(amountSat + feeSat).toLocaleString()} sats, have ${have.toLocaleString()} sats`;
    const hw = first.custody.type === "hardware" || first.custody.type === "multisig";
    return {
      quote: {
        account: first.id,
        method: onchain ? "bitcoinAddress" : "bolt11",
        amountSat,
        feeSat,
        handle: "mock",
        needsSignature: hw ? { psbtBase64: MOCK_PSBT, signaturesNeeded: 1 } : null,
        private: isPrivate,
        payjoin: false,
        notes: isPrivate ? [] : [],
      },
      leg,
      remaining: plan.legs.slice(1),
      hopsAfter: plan.legs.length - 1,
      amountSat: drain ? amountSat : plan.amountSat,
      swapFeeSat: swapFee,
    };
  },
  async confirm(prepared) {
    await sleep(900);
    const q = prepared.quote;
    if (q.needsSignature) return { payments: [], awaitingSignature: q, remaining: prepared.remaining, amountSat: prepared.amountSat };
    return settle(prepared.leg, prepared.remaining, q.amountSat, q.feeSat, prepared.amountSat);
  },
  async resume(paused) {
    await sleep(900);
    const q = paused.awaitingSignature!;
    const leg: Leg = paused.remaining[0] ?? { type: "toExternal", from: q.account, request: { type: "bitcoinAddress", address: "" } };
    return settle({ ...leg, from: q.account } as Leg, paused.remaining, q.amountSat, q.feeSat, paused.amountSat);
  },

  async receive(into, method, amountSat) {
    await sleep(250);
    const a = find(into);
    const examples: Partial<Record<Method, string>> = {
      bitcoinAddress: "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu",
      bolt11:
        "lnbc210u1pjexamplepp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypqdq5xysxxatsyp3k7enxv4jsxqzpusp5zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zygs9qrsgq",
      sparkAddress: "sp1pgss8xn2ujhyth4gljnkcq0g7kd3sd5gw4enmvla9dd9x5v4h5jfvlxzyqyk4",
      liquidAddress: "lq1qqvp9g33gw9y05xava3dvcpq8pnkv82yj3tdnzp547eyp9yrztz2lkyxrhscd55ev4p7lj2n72jtkn5u4xnj4v577c42jhf3ww",
    };
    const req = examples[method];
    lastReceive = { account: into, method, amountSat };
    const swapped = a.rail === "onchain" && (method === "bolt11" || method === "liquidAddress");
    if (!req || (!a.receives.includes(method) && method !== "liquidAddress" && !swapped)) throw `This account can't receive via ${method}`;
    return {
      method,
      request: amountSat && method === "bitcoinAddress" ? `bitcoin:${req.toUpperCase()}?amount=${amountSat / 1e8}` : req,
      amountSat,
      feeSat: swapped && amountSat ? Math.ceil(amountSat * 0.005) + 530 : method === "liquidAddress" ? 120 : null,
      expiresAt: method === "bolt11" || method === "liquidAddress" ? now() + 3600 : null,
    };
  },
  async redeem(_token, into) {
    await sleep(600);
    const a = accounts.find((x) => x.rail === "ecash" && (!into || x.id === into));
    if (!a) throw "Add an ecash module for this mint first";
    const pay = p(`r${Date.now()}`, a.id, "incoming", 2_100, 0, "cashuToken", now(), "Ecash received");
    a.balance.confirmed += 2_100;
    activity.unshift(pay);
    return pay;
  },
  async createToken(from, amountSat, memo) {
    await sleep(500);
    const a = find(from);
    a.balance.confirmed -= amountSat;
    const pay = { ...p(`t${Date.now()}`, from, "outgoing", amountSat, 0, "cashuToken", now(), memo), token: "cashuBo2FteCJodHRwczovL21pbnQubWluaWJpdHMuY2FzaC9CaXRjb2luYXVjc2F0YXSBomFpSABQBVDwSUFGYXCD" };
    activity.unshift(pay);
    return pay;
  },

  async layout() {
    return structuredClone(layout);
  },
  async saveLayout(l) {
    mockBackupSoon();
    layout = structuredClone(l);
  },
  async settings() {
    return { ...mockSettings };
  },
  async saveSettings(s) {
    mockBackupSoon();
    let electrumUrl = mockSettings.electrumUrl ?? null;
    if (s.electrumUrl !== undefined) {
      const e = s.electrumUrl?.trim() ?? "";
      if (e && !/^((ssl|tcp|tls):\/\/)?[^\s/@]+(:\d+)?$/i.test(e)) throw new Error("That Electrum server address isn't valid");
      electrumUrl = e ? (e.includes("://") ? e.replace(/^tls:/i, "ssl:") : `ssl://${e}`) : null;
    }
    mockSettings = {
      ...mockSettings,
      esploraUrl: s.esploraUrl,
      electrumUrl,
      chainServer: electrumUrl ?? s.esploraUrl ?? mockElectrumDefaults[0],
      hasBreezKey: mockSettings.hasBreezKey || !!s.breezApiKey,
    };
  },
  async lightningAddress(account) {
    return account === "spending" ? mockLnAddress : null;
  },
  async lightningAddressAvailable(_account, username) {
    await sleep(300);
    return !["ben", "satoshi", "admin"].includes(username.toLowerCase());
  },
  async registerLightningAddress(_account, username) {
    await sleep(500);
    mockLnAddress = { address: `${username.toLowerCase()}@breez.tips`, username: username.toLowerCase(), lnurl: "lnurl1dp68gurn8ghj7ctsdyhxgetkd9h8vmmfvdjjuer9wchxvef0wejk" };
    return mockLnAddress;
  },
  async spendingDeposits(account) {
    await sleep(200);
    return account === "spending" ? mockDeposits.map((d) => ({ ...d })) : [];
  },
  async claimDeposit(account, txid, vout, maxFeeSat, instant): Promise<ClaimResult> {
    await sleep(600);
    const d = account === "spending" ? mockDeposits.find((x) => x.txid === txid && x.vout === vout) : undefined;
    if (!d) throw new Error("That deposit can't be found on the bitcoin network");
    const mature = (d.confirmations ?? 0) >= d.confirmationsNeeded;
    if (!mature && !instant) throw new Error(`This deposit has ${d.confirmations ?? 0} of the ${d.confirmationsNeeded} confirmations it needs. Wait for them, or claim it instantly`);
    const fee = mature ? (d.claimFeeSat ?? 0) : (d.instantFeeSat ?? 0);
    if (fee > maxFeeSat) {
      return { status: "deferred", payment: null, message: `Claiming it costs ${fee} sats right now, more than the ${maxFeeSat} sats allowed.`, requiredFeeSat: fee };
    }
    mockDeposits = mockDeposits.filter((x) => x !== d);
    const payment: Payment = {
      id: `claim-${txid.slice(0, 8)}`,
      account: "spending",
      direction: "incoming",
      amountSat: d.amountSat - fee,
      feeSat: fee,
      status: mature ? "complete" : "pending",
      method: "bitcoinAddress",
      timestamp: Math.floor(Date.now() / 1000),
      description: null,
    };
    return mature
      ? { status: "claimed", payment, message: null, requiredFeeSat: null }
      : { status: "submitted", payment: null, message: "Claimed early. It reaches your balance in a moment.", requiredFeeSat: null };
  },
  async refundDeposit(account, txid, vout, address, feeRateSatVb) {
    await sleep(600);
    const d = account === "spending" ? mockDeposits.find((x) => x.txid === txid && x.vout === vout) : undefined;
    if (!d) throw new Error("That deposit can't be found on the bitcoin network");
    if (!address.trim()) throw new Error("Enter a bitcoin address");
    if (feeRateSatVb < 1) throw new Error("The fee rate must be at least 1 sat/vB");
    const refundTxid = "9c".repeat(32);
    d.status = "refunding";
    d.refundTxid = refundTxid;
    d.claimFeeSat = null;
    d.error = null;
    return refundTxid;
  },
  async vaultPhoneKey() {
    return mkKey(fakeKey("73c5da0a", vaults.size), "This phone", { type: "phone", account: vaults.size });
  },
  async vaultReadKey(text) {
    await sleep(200);
    const m = text.trim().match(/^\[([0-9a-fA-F]{8})\/48h?'?\/[01]h?'?\/\d+h?'?\/2h?'?\](\w+)/);
    if (!m) throw "That isn't a multisig key. On the device, export the multisig (BIP48) key.";
    return mkKey(text.trim(), "", { type: "external" });
  },
  async bitboxMultisigKey(account) {
    await sleep(900);
    return fakeKey("b17b0a02", account);
  },
  async jadeMultisigKey(account) {
    await sleep(900);
    return fakeKey("9ade1234", account);
  },
  async bitboxRegisterMultisig() {
    await sleep(800);
    return true;
  },
  async jadeRegisterMultisig() {
    await sleep(800);
    return true;
  },
  async bitboxShowMultisigAddress() {
    await sleep(600);
    return "bc1qmockvaultaddress0000000000000000000000000000000000000000";
  },
  async jadeShowMultisigAddress() {
    await sleep(600);
    return "bc1qmockvaultaddress0000000000000000000000000000000000000000";
  },
  async vaultCreate(v) {
    await sleep(500);
    if (v.keys.length < 2) throw "A multisig wallet needs at least 2 keys";
    const fps = new Set(v.keys.map((k) => k.key.slice(1, 9)));
    if (fps.size !== v.keys.length) throw "Two keys come from the same seed. Each key needs its own device or seed, or one loss could cost you two keys";
    const id = `vault-${accounts.length + 1}`;
    const ours = v.keys.filter((k) => k.source.type === "phone").length;
    const a: AccountInfo = {
      balance: { confirmed: 1_250_000, pendingIn: 0, pendingOut: 0 }, id, name: v.name.trim() || "Vault", rail: "onchain",
      custody: { type: "multisig", threshold: v.threshold, total: v.keys.length, localSigners: ours },
      receives: ["bitcoinAddress"], pays: ["bitcoinAddress"],
    };
    accounts.push(a);
    vaults.set(id, { threshold: v.threshold, keys: v.keys.map((k, i) => mkKey(k.key, k.label || `Key ${i + 1}`, k.source)) });
    return a;
  },
  async vaultImportPreview(text) {
    await sleep(300);
    if (!/multi|Policy:|BSMS|extendedPublicKeys/i.test(text)) throw "That file isn't a multisig wallet";
    const keys = [mkKey(fakeKey(randFp()), "Key 1", { type: "external" }), mkKey(fakeKey(randFp()), "Key 2", { type: "external" }), mkKey(fakeKey(randFp()), "Key 3", { type: "external" })];
    return { name: "Imported vault", threshold: 2, total: 3, script: "Native SegWit (P2WSH)", keys, notes: ["None of these keys is this phone's: you'll sign with your hardware wallets or other cosigners."], descriptor: "wsh(sortedmulti(2,…))" };
  },
  async vaultImport(v) {
    return this.vaultCreate({ name: v.name, threshold: 2, keys: v.keys.map((k, i) => ({ key: fakeKey(randFp() + String(i)), label: k.label, source: k.source })) });
  },
  async vaultDetails(account) {
    const v = vaults.get(account) ?? { threshold: 2, keys: [mkKey(fakeKey("73c5da0a"), "This phone", { type: "phone", account: 0 }), mkKey(fakeKey("b17b0a02"), "BitBox02", { type: "device", device: "BitBox02" }), mkKey(fakeKey("5eed5157"), "SeedSigner", { type: "device", device: "SeedSigner" })] };
    const a = find(account);
    return { name: a.name, threshold: v.threshold, total: v.keys.length, script: "Native SegWit (P2WSH)", keys: v.keys, coordination: { type: "solo" as const }, sorted: true, descriptor: `wsh(sortedmulti(${v.threshold},${v.keys.map((k) => k.key + "/<0;1>/*").join(",")}))#mockcsum`, firstAddress: "bc1qmockvaultaddress0000000000000000000000000000000000000000" };
  },
  async vaultSetKeyLabel(account, xpub, label) {
    const v = vaults.get(account);
    const k = v?.keys.find((x) => x.xpub === xpub);
    if (k) k.label = label.trim();
  },
  async vaultExport(account, format) {
    const d = await this.vaultDetails(account);
    const name = d.name.replace(/[^A-Za-z0-9]+/g, "-");
    switch (format) {
      case "coldcard":
        return { filename: `${name}.txt`, mime: "text/plain", content: `# Sessions Wallet multisig setup file\nName: ${d.name}\nPolicy: ${d.threshold} of ${d.total}\nDerivation: m/48'/0'/0'/2'\nFormat: P2WSH\n\n${d.keys.map((k) => `${k.fingerprint.toUpperCase()}: ${k.xpub}`).join("\n")}\n` };
      case "bsms":
        return { filename: `${name}.bsms`, mime: "text/plain", content: `BSMS 1.0\n${d.descriptor}\n/0/*,/1/*\n${d.firstAddress}\n` };
      case "caravan":
        return { filename: `${name}.json`, mime: "application/json", content: JSON.stringify({ name: d.name, addressType: "P2WSH", network: "mainnet", quorum: { requiredSigners: d.threshold, totalSigners: d.total } }, null, 2) };
      case "specter":
        return { filename: `${name}-specter.json`, mime: "application/json", content: JSON.stringify({ label: d.name, blockheight: 0, descriptor: d.descriptor }) };
      default:
        return { filename: `${name}-descriptor.txt`, mime: "text/plain", content: d.descriptor + "\n" };
    }
  },
  async vaultSignatures(account, psbt) {
    const v = vaults.get(account);
    const n = (psbt.match(/\+sig/g) ?? []).length;
    const keys = (v?.keys ?? []).map((k, i) => ({ fingerprint: k.fingerprint, label: k.label, source: k.source, signed: k.ours || i < n }));
    const signed = keys.filter((k) => k.signed).length;
    const needed = v?.threshold ?? 2;
    return { keys, signed, needed, ready: signed >= needed, destinations: [{ address: "bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh", amountSat: 50_000 }] };
  },
  async vaultCombine(_account, psbt) {
    await sleep(200);
    return psbt + "+sig";
  },
  async proposals(account) {
    return proposalsMock.filter((p) => !account || p.account === account);
  },
  async proposalSave(p) {
    const saved = { ...p, id: p.id || `p${Date.now()}`, createdAt: p.createdAt || now(), updatedAt: now() };
    proposalsMock = [saved, ...proposalsMock.filter((x) => x.id !== saved.id)];
    return saved;
  },
  async proposalDelete(id) {
    proposalsMock = proposalsMock.filter((p) => p.id !== id);
  },
  async fileExport(file) {
    await sleep(200);
    try {
      const url = URL.createObjectURL(new Blob([file.content], { type: file.mime }));
      const a = document.createElement("a");
      a.href = url;
      a.download = file.filename;
      a.click();
      URL.revokeObjectURL(url);
    } catch {
      /* preview only */
    }
    return true;
  },
  async nwcList() {
    return nwc.map((n) => ({ ...n.c }));
  },
  async nwcCreate(req) {
    await sleep(300);
    if (!req.name.trim()) throw "Name the app (up to 40 characters)";
    const id = `n${Date.now().toString(36)}`;
    const c: NwcConnection = { id, name: req.name.trim(), createdAt: now(), lastUsedAt: null, enabled: true, permissions: req.permissions, budgetSat: req.budgetSat, budgetPeriod: req.budgetPeriod, maxPerPaymentSat: req.maxPerPaymentSat, spentSat: 0, relay: "wss://relay.primal.net" };
    const uri = `nostr+walletconnect://${"7e".repeat(32)}?relay=wss%3A%2F%2Frelay.primal.net&secret=${"5c".repeat(32)}`;
    nwc.push({ c, uri });
    return { connection: { ...c }, uri };
  },
  async nwcUpdate(id, patch) {
    const n = nwc.find((x) => x.c.id === id);
    if (!n) throw "That connection is gone";
    Object.assign(n.c, Object.fromEntries(Object.entries(patch).filter(([, v]) => v !== undefined)));
    return { ...n.c };
  },
  async nwcDelete(id) {
    nwc = nwc.filter((x) => x.c.id !== id);
  },
  async nwcUri(id) {
    const n = nwc.find((x) => x.c.id === id);
    if (!n) throw "That connection is gone";
    return n.uri;
  },
  async alertsStatus() {
    return { ...alerts };
  },
  async alertsRequestNotifications() {
    alerts.notifications = "granted";
    return { ...alerts };
  },
  async alertsRequestUnrestricted() {
    alerts.unrestricted = true;
    return { ...alerts };
  },
  async onPayment(cb) {
    paymentListeners.add(cb);
    return () => paymentListeners.delete(cb);
  },
  async onChange() {
    return () => {};
  },
  async onWalletChanged() {
    return () => {};
  },
  async unavailableAccounts() {
    return [];
  },
  async feeRates() {
    await sleep(300);
    return { fast: 6, normal: 4, slow: 2 };
  },
  async btcPrice(currency) {
    return currency === "CAD" ? 152_340 : 111_800;
  },
};
