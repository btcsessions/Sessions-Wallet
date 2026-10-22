import { applyTheme, THEMES, type Appearance, type ThemeId } from "../theme";
import { create } from "zustand";
import { api } from "../api";
import type { AccountInfo, InstalledModule, Layout, Payment, SpendStyle, WalletStatus } from "../api/types";
import { errorText, type Unit } from "../format";

export type SheetName =
  | { name: "pay"; prefill?: string; from?: string }
  | { name: "receive"; account?: string }
  | { name: "move"; from?: string; to?: string }
  | { name: "gallery" }
  | { name: "add-account"; kind: "multisig" | "hardware" | "watchOnly" | "ecash" | "savings" }
  | { name: "account"; id: string }
  | { name: "settings" }
  | { name: "ecash-token"; account: string }
  | { name: "tx"; id: string; back?: string }
  | { name: "coins"; account: string }
  | { name: "discover"; found: import("../api/types").FoundAccount[] };

interface Toast {
  kind: "ok" | "error";
  text: string;
}

interface State {
  status: WalletStatus | "loading";
  /** Set when startup fails, so the user sees why instead of a blank screen. */
  bootError: string | null;
  accounts: AccountInfo[];
  activity: Payment[];
  layout: Layout | null;
  unit: Unit;
  /** The fiat currency shown right now (and under amounts). One of `currencies`. */
  currency: string;
  /** Fiat currencies the user picked; tapping a balance cycles sats, BTC, then each of these. */
  currencies: string[];
  setCurrencies(list: string[]): void;
  prices: Record<string, number | null>;
  theme: ThemeId;
  appearance: Appearance;
  setTheme(t: ThemeId): void;
  setAppearance(a: Appearance): void;
  price: number | null;
  editing: boolean;
  sheet: SheetName | null;
  toast: Toast | null;
  syncing: boolean;
  isMock: boolean;
  unavailable: { id: string; reason: string }[];

  boot(): Promise<void>;
  refresh(): Promise<void>;
  /** `manual`: the user asked, so say if it fails. Background syncs stay quiet. */
  sync(manual?: boolean): Promise<void>;
  setStatus(s: WalletStatus): void;
  /** Set right after restoring from a phrase: look for accounts with history. */
  restored: boolean;
  cycleUnit(): void;
  setEditing(v: boolean): void;
  open(s: SheetName): void;
  close(): void;
  notify(kind: Toast["kind"], text: string): void;
  fail(e: unknown): void;

  addModule(m: InstalledModule): Promise<void>;
  removeModule(id: string): Promise<void>;
  resizeModule(id: string, sizes: InstalledModule["size"][]): Promise<void>;
  reorder(ids: string[]): Promise<void>;
  setAdvanced(v: boolean, keep?: (moduleId: string) => boolean): Promise<void>;
  setSpendStyle(v: SpendStyle): Promise<void>;
}

const UNITS: Unit[] = ["sats", "btc", "fiat"];
const SIZES: InstalledModule["size"][] = ["small", "medium", "large"];

function loadPref<T>(key: string, fallback: T): T {
  try {
    const v = localStorage.getItem(key);
    return v ? (JSON.parse(v) as T) : fallback;
  } catch {
    return fallback;
  }
}

function savePref(key: string, v: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(v));
  } catch {
    /* preferences are best-effort */
  }
}

let toastTimer: ReturnType<typeof setTimeout> | undefined;
let unlisten: (() => void) | null = null;

export const useWallet = create<State>((set, get) => {
  async function persistLayout(layout: Layout) {
    set({ layout });
    await (await api()).saveLayout(layout);
  }

  return {
    status: "loading",
    bootError: null,
    accounts: [],
    activity: [],
    layout: null,
    unit: loadPref<Unit>("unit", "sats"),
    currency: loadPref("currency", "CAD"),
    currencies: (() => {
      const list = loadPref<string[]>("currencies", []);
      const main = loadPref("currency", "CAD");
      return list.length ? list : [main];
    })(),
    prices: {},
    setCurrencies(list) {
      if (!list.length) return;
      const currency = list.includes(get().currency) ? get().currency : list[0];
      savePref("currencies", list);
      savePref("currency", currency);
      set({ currencies: list, currency, price: get().prices[currency] ?? null });
      void get().refresh();
    },
    restored: false,
    theme: loadPref<ThemeId>("theme", "sessions"),
    appearance: loadPref<Appearance>("appearance", "dark"),
    setTheme(theme) {
      const def = THEMES.find((t) => t.id === theme) ?? THEMES[0];
      const appearance = def.preferred;
      savePref("theme", theme);
      savePref("appearance", appearance);
      applyTheme(theme, appearance);
      set({ theme, appearance });
    },
    setAppearance(appearance) {
      savePref("appearance", appearance);
      applyTheme(get().theme, appearance);
      set({ appearance });
    },
    price: null,
    editing: false,
    sheet: null,
    toast: null,
    syncing: false,
    isMock: false,
    unavailable: [],

    async boot() {
      set({ bootError: null });
      try {
        const a = await api();
        const status = await Promise.race([
          a.status(),
          new Promise<never>((_, reject) => setTimeout(() => reject(new Error("The wallet engine didn't respond within 15 seconds.")), 15_000)),
        ]);
        set({ isMock: a.isMock, status });
        if (status === "unlocked") await get().refresh();
        // Refresh as soon as an account reports a change (debounced).
        if (!unlisten) {
          let t: ReturnType<typeof setTimeout> | undefined;
          unlisten = await a.onChange(() => {
            clearTimeout(t);
            t = setTimeout(() => {
              if (get().status === "unlocked") void get().refresh();
            }, 300);
          });
        }
      } catch (e) {
        set({ bootError: errorText(e) });
      }
    },

    async refresh() {
      const a = await api();
      try {
        const fiats = get().currencies;
        const [accounts, activity, layout, priceList, unavailable] = await Promise.all([
          a.accounts(),
          a.activity(50),
          a.layout(),
          Promise.all(fiats.map((c) => a.btcPrice(c).catch(() => null))),
          a.unavailableAccounts().catch(() => []),
        ]);
        const prices = Object.fromEntries(fiats.map((c, i) => [c, priceList[i]]));
        set({ accounts, activity, layout, prices, price: prices[get().currency] ?? null, unavailable });
      } catch (e) {
        get().fail(e);
      }
    },

    async sync(manual = false) {
      if (get().syncing) return;
      set({ syncing: true });
      // Show what's already known straight away; the network catches up after.
      void get().refresh();
      try {
        await (await api()).sync();
        await get().refresh();
      } catch (e) {
        if (manual) get().fail(e);
        else console.warn("background sync:", e);
      } finally {
        set({ syncing: false });
      }
    },

    setStatus(status) {
      set({ status });
      if (status === "unlocked") void get().refresh();
    },

    cycleUnit() {
      // sats → BTC → each chosen fiat currency → sats
      const { unit, currency, currencies, prices } = get();
      const i = currencies.indexOf(currency);
      if (unit === "fiat" && i >= 0 && i < currencies.length - 1) {
        const next = currencies[i + 1];
        savePref("currency", next);
        set({ currency: next, price: prices[next] ?? null });
        return;
      }
      const nextUnit = UNITS[(UNITS.indexOf(unit) + 1) % UNITS.length];
      savePref("unit", nextUnit);
      if (nextUnit === "fiat" && currencies.length) {
        const first = currencies[0];
        savePref("currency", first);
        set({ unit: nextUnit, currency: first, price: prices[first] ?? null });
      } else set({ unit: nextUnit });
    },

    setEditing(editing) {
      set({ editing });
    },
    open(sheet) {
      // The module gallery belongs to edit mode; everything else leaves it.
      set({ sheet, editing: sheet.name === "gallery" || sheet.name === "add-account" ? get().editing : false });
    },
    close() {
      set({ sheet: null });
    },

    notify(kind, text) {
      clearTimeout(toastTimer);
      set({ toast: { kind, text } });
      toastTimer = setTimeout(() => set({ toast: null }), 3200);
    },
    fail(e) {
      get().notify("error", errorText(e));
    },

    async addModule(m) {
      const layout = get().layout;
      if (!layout || layout.modules.some((x) => x.id === m.id)) return;
      await persistLayout({ ...layout, modules: [...layout.modules, m] });
    },
    async removeModule(id) {
      const layout = get().layout;
      if (!layout) return;
      await persistLayout({ ...layout, modules: layout.modules.filter((m) => m.id !== id) });
    },
    async resizeModule(id, sizes) {
      const layout = get().layout;
      if (!layout) return;
      const allowed = sizes.length ? sizes : SIZES;
      await persistLayout({
        ...layout,
        modules: layout.modules.map((m) =>
          m.id === id ? { ...m, size: allowed[(allowed.indexOf(m.size) + 1) % allowed.length] } : m,
        ),
      });
    },
    async reorder(ids) {
      const layout = get().layout;
      if (!layout) return;
      const byId = new Map(layout.modules.map((m) => [m.id, m]));
      await persistLayout({ ...layout, modules: ids.map((id) => byId.get(id)!).filter(Boolean) });
    },
    async setSpendStyle(spendStyle) {
      const layout = get().layout;
      if (!layout) return;
      await persistLayout({ ...layout, spendStyle });
    },
    async setAdvanced(advanced, keep) {
      const layout = get().layout;
      if (!layout) return;
      // Leaving Advanced: only the modules the caller keeps, and sends go back to Efficient.
      const modules = keep ? layout.modules.filter((m) => keep(m.id)) : layout.modules;
      await persistLayout({ ...layout, advanced, modules, spendStyle: advanced ? layout.spendStyle : "efficient" });
    },
  };
});
