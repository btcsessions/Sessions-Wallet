import { applyTheme, THEMES, type Appearance, type ThemeId } from "../theme";
import { create } from "zustand";
import { api } from "../api";
import type { AccountInfo, InstalledModule, Layout, Payment, SpendStyle, WalletStatus } from "../api/types";
import { errorText, type Unit } from "../format";
import { buzz } from "../components/SuccessCheck";
import { normalize, SYNCED_KEYS, type Synced, type SyncedKey } from "./synced";

export type SheetName =
  | { name: "pay"; prefill?: string; from?: string }
  | { name: "receive"; account?: string }
  | { name: "move"; from?: string; to?: string; amount?: number }
  | { name: "gallery" }
  | { name: "add-account"; kind: "multisig" | "hardware" | "watchOnly" | "ecash" | "savings" }
  | { name: "account"; id: string }
  | { name: "settings" }
  | { name: "ecash-token"; account: string }
  | { name: "tx"; id: string; back?: string }
  | { name: "coins"; account: string }
  | { name: "discover"; found: import("../api/types").FoundAccount[] }
  | { name: "contacts"; add?: string }
  | { name: "checkin" }
  | { name: "nwc" }
  | { name: "nostr-id" }
  | { name: "devices" }
  | { name: "group-join"; code?: string }
  | { name: "group"; groupId?: string }
  | { name: "vault" }
  | { name: "vault-export"; account: string }
  | { name: "proposal"; id: string };

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
  /** Vault accounts run with other people (group vaults). */
  groupAccounts: string[];
  /** Desktop window: a sidebar and pages instead of a phone screen. */
  desktop: boolean;
  /** Desktop: what the main area shows. */
  page: { kind: "home" } | { kind: "account"; id: string };
  toast: Toast | null;
  syncing: boolean;
  isMock: boolean;
  unavailable: { id: string; reason: string }[];
  /** Alerts, contacts, auto-sweep, check-ins: saved in the wallet (and its backup). */
  synced: Synced;
  setSynced<K extends SyncedKey>(key: K, value: Synced[K]): void;

  boot(): Promise<void>;
  refresh(): Promise<void>;
  /** `manual`: the user asked, so say if it fails. Background syncs stay quiet. */
  sync(manual?: boolean): Promise<void>;
  setStatus(s: WalletStatus): void;
  /** Apply the preferences saved in the wallet (restored from backup or set on another device). */
  loadPrefs(): Promise<void>;
  /** Set right after restoring from a phrase: look for accounts with history. */
  restored: boolean;
  cycleUnit(): void;
  setEditing(v: boolean): void;
  open(s: SheetName): void;
  close(): void;
  setDesktop(on: boolean): void;
  showPage(page: State["page"]): void;
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

/** Preferences that travel with the wallet backup (not just this device). */
const SYNCED_PREFS = ["unit", "currency", "currencies", "theme", "appearance", "qrDensity", ...SYNCED_KEYS] as const;
let prefsTimer: ReturnType<typeof setTimeout> | undefined;
let prefsReady = false;

function readPref(key: string): unknown {
  try {
    const v = localStorage.getItem(key);
    return v === null ? undefined : JSON.parse(v);
  } catch {
    return undefined;
  }
}

/** Save this device's synced preferences into the wallet (and so its backup), debounced. */
export function pushPrefs() {
  if (!prefsReady) return;
  clearTimeout(prefsTimer);
  prefsTimer = setTimeout(() => {
    const prefs: Record<string, unknown> = {};
    for (const k of SYNCED_PREFS) {
      const v = readPref(k);
      if (v !== undefined) prefs[k] = v;
    }
    void api()
      .then((a) => a.savePrefs(prefs))
      .catch(() => {});
  }, 1500);
}

function savePref(key: string, v: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(v));
  } catch {
    /* preferences are best-effort */
  }
  if ((SYNCED_PREFS as readonly string[]).includes(key)) pushPrefs();
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
    desktop: false,
    page: { kind: "home" },
    groupAccounts: [],
    toast: null,
    syncing: false,
    isMock: false,
    unavailable: [],
    synced: Object.fromEntries(SYNCED_KEYS.map((k) => [k, normalize(k, loadPref(k, undefined))])) as unknown as Synced,
    setSynced(key, value) {
      savePref(key, value);
      set({ synced: { ...get().synced, [key]: value } });
    },

    async boot() {
      set({ bootError: null });
      try {
        const a = await api();
        let status = await Promise.race([
          a.status(),
          new Promise<never>((_, reject) => setTimeout(() => reject(new Error("The wallet engine didn't respond within 15 seconds.")), 15_000)),
        ]);
        // The wallet kept running while the app's screen was closed (it was
        // watching for payments): opening the app again asks for the passcode.
        if (status === "unlocked" && !a.isMock) {
          await a.lock();
          status = await a.status();
        }
        set({ isMock: a.isMock, status });
        if (status === "unlocked") {
          await get().refresh();
          void get().loadPrefs();
        }
        // Refresh as soon as an account reports a change (debounced).
        if (!unlisten) {
          let t: ReturnType<typeof setTimeout> | undefined;
          // A payment landed while the Receive screen isn't the one showing it.
          void a.onPayment((e) => {
            const sheet = get().sheet;
            if (get().status !== "unlocked" || sheet?.name === "receive") return;
            const amount = `${e.amountSat.toLocaleString("en-US")} sats`;
            const text =
              e.kind === "received"
                ? `Received ${amount} in ${e.accountName}`
                : e.kind === "confirmed"
                  ? `${amount} confirmed in ${e.accountName}`
                  : `Incoming ${amount} to ${e.accountName}`;
            get().notify("ok", text);
            buzz();
            void get().refresh();
          });
          void a.onStateRestored(() => {
            void get().refresh();
            void get().loadPrefs();
          });
          // Coming back to the app: pick up changes from linked devices.
          let lastSync = 0;
          document.addEventListener("visibilitychange", () => {
            if (document.visibilityState !== "visible" || get().status !== "unlocked") return;
            if (Date.now() - lastSync < 60_000) return;
            lastSync = Date.now();
            void a.syncNow("both").catch(() => {});
          });
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
        const [accounts, activity, layout, priceList, unavailable, groupAccounts] = await Promise.all([
          a.accounts(),
          a.activity(50),
          a.layout(),
          Promise.all(fiats.map((c) => a.btcPrice(c).catch(() => null))),
          a.unavailableAccounts().catch(() => []),
          a.groupAccounts().catch(() => [] as string[]),
        ]);
        const prices = Object.fromEntries(fiats.map((c, i) => [c, priceList[i]]));
        set({ accounts, activity, layout, prices, price: prices[get().currency] ?? null, unavailable, groupAccounts });
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
      if (status === "unlocked") {
        void get().refresh();
        void get().loadPrefs();
      } else prefsReady = false;
    },

    async loadPrefs() {
      try {
        const saved = await (await api()).prefs();
        if (Object.keys(saved).length) {
          for (const k of SYNCED_PREFS) if (k in saved) localStorage.setItem(k, JSON.stringify(saved[k]));
          const theme = (saved.theme as ThemeId) ?? get().theme;
          const appearance = (saved.appearance as Appearance) ?? get().appearance;
          applyTheme(theme, appearance);
          set({
            theme,
            appearance,
            unit: (saved.unit as Unit) ?? get().unit,
            currency: (saved.currency as string) ?? get().currency,
            currencies: (saved.currencies as string[]) ?? get().currencies,
            synced: Object.fromEntries(SYNCED_KEYS.map((k) => [k, normalize(k, k in saved ? saved[k] : readPref(k))])) as unknown as Synced,
          });
          prefsReady = true;
          void get().refresh();
        } else {
          // First time: this device's choices become the wallet's.
          prefsReady = true;
          pushPrefs();
        }
      } catch {
        prefsReady = true;
      }
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
      // On the desktop an account is a page beside the sidebar, not a sheet.
      if (get().desktop && sheet.name === "account") {
        set({ page: { kind: "account", id: sheet.id }, sheet: null, editing: false });
        return;
      }
      // The module gallery belongs to edit mode; everything else leaves it.
      set({ sheet, editing: sheet.name === "gallery" || sheet.name === "add-account" ? get().editing : false });
    },
    setDesktop(desktop) {
      if (desktop === get().desktop) return;
      const { sheet, page } = get();
      // Carry an open account across: phone sheet ⇄ desktop page.
      if (desktop && sheet?.name === "account") set({ desktop, page: { kind: "account", id: sheet.id }, sheet: null });
      else if (!desktop && page.kind === "account") set({ desktop, page: { kind: "home" }, sheet: sheet ?? { name: "account", id: page.id } });
      else set({ desktop });
    },
    showPage(page) {
      set({ page, editing: false });
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
