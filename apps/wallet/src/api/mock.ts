// Browser-only stand-in for the Rust core so the UI can be designed and
// reviewed without a native build. Never ships in the Tauri app.

import type { WalletApi } from "./index";
import type { AccountInfo, BitboxEvent, JadeEvent, Layout, Leg, LightningAddress, Method, Payment, PaymentRequest, Plan, Settings } from "./types";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const bitboxListeners = new Set<(e: BitboxEvent) => void>();
let bitboxPaired = false;
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
const mockPayjoin = new Set<string>();

let mockSettings: Settings = { esploraUrl: null, hasBreezKey: true };

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
    return DEMO_PHRASE;
  },
  async restoreWallet(phrase) {
    await sleep(600);
    if (phrase.trim().split(/\s+/).length < 12) throw "That recovery phrase is too short. It should be 12 or 24 words.";
    seedDemo();
    status = "unlocked";
  },
  async unlock(passcode) {
    await sleep(300);
    if (passcode.length < 6) throw "Wrong passcode";
    status = "unlocked";
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
  async renameAccount(id, name) {
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
    if (kind !== "output") return;
    const u = mockUtxos.get(account)?.find((x) => x.outpoint === reference);
    if (u) u.label = label;
  },
  async setFrozen(account, outpoint, frozen) {
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
  async prepare(plan, fee, style) {
    await sleep(500);
    const first = find(legFrom(plan.legs[0]));
    const leg = plan.legs[0];
    const onchain = (leg.type === "toAccount" && leg.method === "bitcoinAddress") || (leg.type === "toExternal" && leg.request.type === "bitcoinAddress") || (leg.type === "viaSwap" && leg.payMethod === "bitcoinAddress");
    const rate = typeof fee === "number" ? fee : fee === "fast" ? 6 : fee === "slow" ? 2 : 4;
    const swapFee = leg.type === "viaSwap" ? Math.ceil(plan.amountSat * 0.001) + 120 : 0;
    const isPrivate = onchain && style === "private" && first.rail === "onchain";
    if (isPrivate && first.custody.type === "hardware") throw "Private send needs coins at two or more different addresses. Use Efficient for this one";
    const feeSat = onchain ? rate * (isPrivate ? 340 : 141) : 3;
    const amountSat = plan.amountSat + swapFee;
    const have = first.balance.confirmed + first.balance.pendingIn;
    if (have < amountSat + feeSat) throw `Not enough funds: need ${(amountSat + feeSat).toLocaleString()} sats, have ${have.toLocaleString()} sats`;
    const hw = first.custody.type === "hardware" || first.custody.type === "multisig";
    return {
      quote: {
        account: first.id,
        method: onchain ? "bitcoinAddress" : "bolt11",
        amountSat,
        feeSat,
        handle: "mock",
        needsSignature: hw ? { psbtBase64: "cHNidP8BAHECAAAAAWZtb2NrcHNidG1vY2twc2J0bW9ja3BzYnRtb2NrcHNidG1vY2s=", signaturesNeeded: 1 } : null,
        private: isPrivate,
        payjoin: false,
        notes: isPrivate ? [] : [],
      },
      leg,
      remaining: plan.legs.slice(1),
      hopsAfter: plan.legs.length - 1,
      amountSat: plan.amountSat,
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
    layout = structuredClone(l);
  },
  async settings() {
    return { ...mockSettings };
  },
  async saveSettings(s) {
    mockSettings = { esploraUrl: s.esploraUrl, hasBreezKey: mockSettings.hasBreezKey || !!s.breezApiKey };
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
  async onChange() {
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
