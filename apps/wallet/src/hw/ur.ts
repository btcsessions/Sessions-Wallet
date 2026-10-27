// Uniform Resources (BCR-2020-005): the animated-QR format SeedSigner, Jade,
// and other wallets use for PSBTs and account exports.
import "./ur-polyfill";
import { URDecoder, UREncoder, UR } from "@ngraveio/bc-ur";
import { CryptoAccount, CryptoCoinInfo, CryptoHDKey, CryptoKeypath, CryptoOutput, CryptoPSBT, MultiKey, PathComponent, ScriptExpressions } from "@keystonehq/bc-ur-registry";
import bs58check from "./bs58check-shim";
import { Buffer } from "buffer";

export interface UrProgress {
  /** 0..1 */
  progress: number;
  done: boolean;
}

/** Collects the frames of a (possibly animated) UR. */
export class UrCollector {
  private d = new URDecoder();

  /** Feed one scanned frame. Returns null for frames that aren't UR. */
  receive(part: string): UrProgress | null {
    if (!/^ur:/i.test(part.trim())) return null;
    this.d.receivePart(part.trim().toLowerCase());
    // A frame from a different code (or a decode error): start over.
    if (this.d.isError()) {
      this.d = new URDecoder();
      this.d.receivePart(part.trim().toLowerCase());
    }
    const done = this.d.isComplete() === true;
    return { progress: done ? 1 : Math.max(this.d.estimatedPercentComplete(), this.d.getProgress()), done };
  }

  result(): { type: string; cbor: Buffer } {
    if (!this.d.isSuccess()) throw new Error(this.d.resultError() || "That QR code didn't decode");
    const ur = this.d.resultUR();
    return { type: ur.type, cbor: ur.cbor };
  }
}

const b64 = (b: Buffer | Uint8Array) => Buffer.from(b).toString("base64");

/** Base64 PSBT from a `crypto-psbt` (or newer `psbt`) UR. */
export function psbtFromUr(type: string, cbor: Buffer): string {
  if (type === "crypto-psbt" || type === "psbt") return b64(CryptoPSBT.fromCBOR(cbor).getPSBT());
  if (type === "bytes") {
    // Some coordinators wrap the raw PSBT, others its base64 text.
    const raw = Buffer.from(new UR(cbor, "bytes").decodeCBOR());
    return raw.subarray(0, 5).toString("hex") === "70736274ff" ? b64(raw) : raw.toString("utf8").trim();
  }
  throw new Error(`Expected a signed transaction, got "${type}"`);
}

function hdKeyExpr(key: CryptoHDKey, fallbackFp?: Buffer): string {
  const origin = key.getOrigin();
  const fp = origin?.getSourceFingerprint() ?? fallbackFp;
  const path = origin?.getPath()?.replace(/'/g, "h");
  const xpub = key.getBip32Key();
  return fp && path ? `[${fp.toString("hex")}/${path}]${xpub}` : xpub;
}

function outputDescriptor(out: CryptoOutput, masterFp?: Buffer): string | null {
  const exprs = out.getScriptExpressions().map((e) => e.getExpression());
  const key = out.getCryptoKey();
  if (!(key instanceof CryptoHDKey)) return null;
  const inner = `${hdKeyExpr(key, masterFp)}/<0;1>/*`;
  return exprs.reduceRight((acc, e) => `${e}(${acc})`, inner);
}

/** Preference when an export offers several script types. */
const RANK = ["wpkh(", "tr(", "sh(wpkh(", "pkh("];
const rank = (d: string) => {
  const i = RANK.findIndex((p) => d.startsWith(p));
  return i < 0 ? 99 : i;
};

/** A single-sig descriptor from a `crypto-account`, `crypto-output` or `crypto-hdkey` UR. */
export function descriptorFromUr(type: string, cbor: Buffer): string {
  if (type === "crypto-account") {
    const acct = CryptoAccount.fromCBOR(cbor);
    const fp = acct.getMasterFingerprint();
    const ds = acct
      .getOutputDescriptors()
      .map((o) => outputDescriptor(o, fp))
      .filter((d): d is string => !!d && !d.includes("multi"));
    if (!ds.length) throw new Error("This export has no single-signature account in it");
    return ds.sort((a, b) => rank(a) - rank(b))[0];
  }
  if (type === "crypto-output") {
    const d = outputDescriptor(CryptoOutput.fromCBOR(cbor));
    if (!d) throw new Error("Unsupported output in this QR code");
    return d;
  }
  if (type === "crypto-hdkey") {
    // A bare key: the import step works out the script type from its path.
    return hdKeyExpr(CryptoHDKey.fromCBOR(cbor));
  }
  throw new Error(`Expected an account export (xpub), got "${type}"`);
}

/** Frames of an animated `crypto-psbt` for a hardware wallet to scan. */
export function psbtFrames(psbtBase64: string, maxFragment = 120): { next: () => string; count: number } {
  const ur = new CryptoPSBT(Buffer.from(psbtBase64, "base64")).toUR();
  const enc = new UREncoder(ur, maxFragment);
  return { next: () => enc.nextPart().toUpperCase(), count: enc.fragmentsLength };
}

/** Frames of any UR, given its type and raw CBOR payload. */
export function urFrames(type: string, cbor: Uint8Array, maxFragment = 400): { next: () => string; count: number } {
  const enc = new UREncoder(new UR(Buffer.from(cbor), type), maxFragment);
  return { next: () => enc.nextPart().toUpperCase(), count: enc.fragmentsLength };
}

/** Text carried in a `bytes` UR (animated ecash tokens, long invoices). */
export function textFromUr(type: string, cbor: Uint8Array): string {
  if (type !== "bytes") throw new Error(`Unexpected code type "${type}"`);
  return Buffer.from(new UR(Buffer.from(cbor), "bytes").decodeCBOR())
    .toString("utf8")
    .trim();
}

/** Every `[fingerprint/path]xpub` key in an account / output / hdkey UR. */
export function keysFromUr(type: string, cbor: Uint8Array): string[] {
  const buf = Buffer.from(cbor);
  if (type === "crypto-account") {
    const acct = CryptoAccount.fromCBOR(buf);
    const fp = acct.getMasterFingerprint();
    return acct
      .getOutputDescriptors()
      .map((o) => o.getCryptoKey())
      .filter((k): k is CryptoHDKey => k instanceof CryptoHDKey)
      .map((k) => hdKeyExpr(k, fp));
  }
  if (type === "crypto-output") {
    const k = CryptoOutput.fromCBOR(buf).getCryptoKey();
    return k instanceof CryptoHDKey ? [hdKeyExpr(k)] : [];
  }
  if (type === "crypto-hdkey") return [hdKeyExpr(CryptoHDKey.fromCBOR(buf))];
  throw new Error(`Expected a key export, got "${type}"`);
}

// ---- Multisig wallets -----------------------------------------------------

/** A multisig descriptor (`wsh(sortedmulti(2,[fp/48h/0h/0h/2h]xpub/<0;1>/*,…))`) from a `crypto-output` UR. */
export function multisigDescriptorFromUr(type: string, cbor: Uint8Array): string {
  if (type === "bytes") return textFromUr(type, cbor);
  if (type !== "crypto-output" && type !== "output-descriptor") throw new Error(`Expected a multisig wallet, got "${type}"`);
  const out = CryptoOutput.fromCBOR(Buffer.from(cbor));
  const key = out.getCryptoKey();
  if (!(key instanceof MultiKey)) throw new Error("That QR code is a single-signature wallet, not a multisig one");
  const keys = key
    .getKeys()
    .filter((k): k is CryptoHDKey => k instanceof CryptoHDKey)
    .map((k) => `${hdKeyExpr(k)}/<0;1>/*`);
  const exprs = out.getScriptExpressions().map((e) => e.getExpression());
  return exprs.reduceRight((acc, e) => `${e}(${acc})`, `${key.getThreshold()},${keys.join(",")}`);
}

function pathComponents(path: string): PathComponent[] {
  return path
    .replace(/^m\/?/, "")
    .split("/")
    .filter(Boolean)
    .map((p) => new PathComponent({ index: parseInt(p, 10), hardened: /['hH]$/.test(p) }));
}

/** One `[fp/path]xpub` as a `crypto-hdkey`. */
function hdKeyFrom(fingerprint: string, path: string, xpub: string, testnet: boolean): CryptoHDKey {
  const raw = Buffer.from(bs58check.decode(xpub));
  // version(4) depth(1) parent fp(4) child(4) chain code(32) key(33)
  const depth = raw[4];
  return new CryptoHDKey({
    isMaster: false,
    key: raw.subarray(45, 78),
    chainCode: raw.subarray(13, 45),
    useInfo: new CryptoCoinInfo(undefined, testnet ? 1 : 0),
    origin: new CryptoKeypath(pathComponents(path), Buffer.from(fingerprint, "hex"), depth),
    parentFingerprint: raw.subarray(5, 9),
  });
}

/** Frames of a `crypto-output` UR for a P2WSH sortedmulti vault (Sparrow, SeedSigner, Keystone read it). */
export function multisigOutputFrames(
  threshold: number,
  keys: { fingerprint: string; path: string; xpub: string }[],
  script: string,
  testnet = false,
  maxFragment = 250,
): { next: () => string; count: number } {
  const multi = new MultiKey(
    threshold,
    keys.map((k) => hdKeyFrom(k.fingerprint, k.path, k.xpub, testnet)),
  );
  const exprs = /Nested/i.test(script)
    ? [ScriptExpressions.SCRIPT_HASH, ScriptExpressions.WITNESS_SCRIPT_HASH, ScriptExpressions.SORTED_MULTISIG]
    : /Legacy/i.test(script)
      ? [ScriptExpressions.SCRIPT_HASH, ScriptExpressions.SORTED_MULTISIG]
      : [ScriptExpressions.WITNESS_SCRIPT_HASH, ScriptExpressions.SORTED_MULTISIG];
  const ur = new CryptoOutput(exprs, multi).toUR();
  const enc = new UREncoder(ur, maxFragment);
  return { next: () => enc.nextPart().toUpperCase(), count: enc.fragmentsLength };
}

/** Frames of a `bytes` UR carrying a text file (the Coldcard-style wallet file Jade, Keystone, Passport scan). */
export function textFrames(text: string, maxFragment = 250): { next: () => string; count: number } {
  const ur = UR.fromBuffer(Buffer.from(text, "utf8"));
  const enc = new UREncoder(ur, maxFragment);
  return { next: () => enc.nextPart().toUpperCase(), count: enc.fragmentsLength };
}
