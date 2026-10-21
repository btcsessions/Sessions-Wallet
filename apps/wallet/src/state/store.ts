import { create } from "zustand";
import { api } from "../api";
import type { AccountInfo, InstalledModule, Layout, Payment, WalletStatus } from "../api/types";
import { errorText, type Unit } from "../format";

export type SheetName =
  | { name: "pay"; prefill?: string }
  | { name: "receive"; account?: string }
  | { name: "move"; from?: string; to?: string }
  | { name: "gallery" }
  | { name: "add-account"; kind: "multisig" | "hardware" | "watchOnly" | "ecash" }
  | { name: "account"; id: string }
  | { name: "settings" }
  | { name: "ecash-token"; account: string };

interface Toast {
  kind: "ok" | "error";
  text: string;
}

interface State {
  status: WalletStatus | "loading";
  accounts: AccountInfo[];
  activity: Payment[];
  layout: Layout | null;
  unit: Unit;
  currency: string;
  price: number | null;
  editing: boolean;
  sheet: SheetName | null;
  toast: Toast | null;
  syncing: boolean;
  isMock: boolean;

  boot(): Promise<void>;
  refresh(): Promise<void>;
  sync(): Promise<void>;
  setStatus(s: WalletStatus): void;
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
  setAdvanced(v: boolean): Promise<void>;
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

export const useWallet = create<State>((set, get) => {
  async function persistLayout(layout: Layout) {
    set({ layout });
    await (await api()).saveLayout(layout);
  }

  return {
    status: "loading",
    accounts: [],
    activity: [],
    layout: null,
    unit: loadPref<Unit>("unit", "sats"),
    currency: loadPref("currency", "CAD"),
    price: null,
    editing: false,
    sheet: null,
    toast: null,
    syncing: false,
    isMock: false,

    async boot() {
      const a = await api();
      set({ isMock: a.isMock, status: await a.status() });
      if (get().status === "unlocked") await get().refresh();
    },

    async refresh() {
      const a = await api();
      try {
        const [accounts, activity, layout, price] = await Promise.all([
          a.accounts(),
          a.activity(50),
          a.layout(),
          a.btcPrice(get().currency).catch(() => null),
        ]);
        set({ accounts, activity, layout, price });
      } catch (e) {
        get().fail(e);
      }
    },

    async sync() {
      if (get().syncing) return;
      set({ syncing: true });
      try {
        await (await api()).sync();
        await get().refresh();
      } catch (e) {
        get().fail(e);
      } finally {
        set({ syncing: false });
      }
    },

    setStatus(status) {
      set({ status });
      if (status === "unlocked") void get().refresh();
    },

    cycleUnit() {
      const unit = UNITS[(UNITS.indexOf(get().unit) + 1) % UNITS.length];
      savePref("unit", unit);
      set({ unit });
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
    async setAdvanced(advanced) {
      const layout = get().layout;
      if (!layout) return;
      await persistLayout({ ...layout, advanced });
    },
  };
});
