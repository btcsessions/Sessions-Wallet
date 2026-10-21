// Drop-in for the `bs58check` package (used by bc-ur-registry to print
// xpubs). The original pulls in Node's crypto streams, which don't exist in a
// webview; this needs only a SHA-256.
import { sha256 } from "@noble/hashes/sha256";

const ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

function checksum(data: Uint8Array): Uint8Array {
  return sha256(sha256(data)).subarray(0, 4);
}

function toBase58(bytes: Uint8Array): string {
  let zeros = 0;
  while (zeros < bytes.length && bytes[zeros] === 0) zeros++;
  let n = 0n;
  for (const b of bytes) n = (n << 8n) | BigInt(b);
  let out = "";
  while (n > 0n) {
    out = ALPHABET[Number(n % 58n)] + out;
    n /= 58n;
  }
  return "1".repeat(zeros) + out;
}

function fromBase58(s: string): Uint8Array {
  let n = 0n;
  for (const c of s) {
    const v = ALPHABET.indexOf(c);
    if (v < 0) throw new Error("Non-base58 character");
    n = n * 58n + BigInt(v);
  }
  const bytes: number[] = [];
  while (n > 0n) {
    bytes.unshift(Number(n & 0xffn));
    n >>= 8n;
  }
  let zeros = 0;
  while (s[zeros] === "1") zeros++;
  return new Uint8Array([...new Array(zeros).fill(0), ...bytes]);
}

export function encode(payload: Uint8Array): string {
  const sum = checksum(payload);
  const all = new Uint8Array(payload.length + 4);
  all.set(payload);
  all.set(sum, payload.length);
  return toBase58(all);
}

export function decode(s: string): Uint8Array {
  const all = fromBase58(s);
  const payload = all.subarray(0, -4);
  const sum = checksum(payload);
  if (sum.some((b, i) => b !== all[all.length - 4 + i])) throw new Error("Invalid checksum");
  return payload;
}

export default { encode, decode };
