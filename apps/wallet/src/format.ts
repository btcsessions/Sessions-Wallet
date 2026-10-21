import type { AccountInfo, Method } from "./api/types";

export type Unit = "sats" | "btc" | "fiat";

const satsFmt = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });

export function formatSats(sats: number): string {
  return satsFmt.format(sats);
}

/** 0.04 250 000 — BTC with the last six digits grouped so sats stay readable. */
export function formatBtc(sats: number): string {
  const s = (sats / 1e8).toFixed(8);
  const [whole, frac] = s.split(".");
  return `${whole}.${frac.slice(0, 2)} ${frac.slice(2, 5)} ${frac.slice(5)}`;
}

export function formatFiat(sats: number, price: number | null, currency: string): string {
  if (price == null) return "—";
  const v = (sats / 1e8) * price;
  return new Intl.NumberFormat(undefined, {
    style: "currency",
    currency,
    maximumFractionDigits: v < 100 ? 2 : 0,
  }).format(v);
}

export function amountParts(sats: number, unit: Unit, price: number | null, currency: string): { value: string; suffix: string } {
  switch (unit) {
    case "sats":
      return { value: formatSats(sats), suffix: "sats" };
    case "btc":
      return { value: formatBtc(sats), suffix: "BTC" };
    case "fiat":
      return { value: formatFiat(sats, price, currency), suffix: "" };
  }
}

export function signed(sats: number, dir: "incoming" | "outgoing" | "internal"): string {
  const s = formatSats(sats);
  return dir === "incoming" ? `+${s}` : dir === "outgoing" ? `−${s}` : s;
}

export const METHOD_LABEL: Record<Method, string> = {
  bitcoinAddress: "Bitcoin",
  bolt11: "Lightning",
  bolt12: "Lightning offer",
  lightningAddress: "Lightning address",
  lnurl: "LNURL",
  sparkAddress: "Spark",
  liquidAddress: "Liquid",
  cashuToken: "Ecash",
};

export function accountColor(a: AccountInfo): string {
  if (a.rail === "spark") return "var(--rail-spark)";
  if (a.rail === "ecash") return "var(--rail-ecash)";
  if (a.custody.type === "multisig") return "var(--rail-vault)";
  if (a.custody.type === "hardware") return "var(--rail-hardware)";
  if (a.custody.type === "watchOnly") return "var(--rail-watch)";
  return "var(--rail-onchain)";
}

export function accountKind(a: AccountInfo): string {
  switch (a.custody.type) {
    case "hot":
      return a.rail === "spark" ? "Lightning & Spark" : "On-chain";
    case "hardware":
      return a.custody.device;
    case "multisig":
      return `${a.custody.threshold}-of-${a.custody.total} vault`;
    case "watchOnly":
      return "Watch-only";
    case "mint":
      try {
        return `Ecash from ${new URL(a.custody.url).host}`;
      } catch {
        return "Ecash";
      }
  }
}

export function relativeTime(unix: number): string {
  const diff = Date.now() / 1000 - unix;
  if (diff < 60) return "Just now";
  if (diff < 3600) return `${Math.floor(diff / 60)} min ago`;
  if (diff < 86_400) return `${Math.floor(diff / 3600)} h ago`;
  if (diff < 7 * 86_400) return new Date(unix * 1000).toLocaleDateString(undefined, { weekday: "long" });
  return new Date(unix * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return "Something went wrong";
}
