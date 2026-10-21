// App themes. A theme ("skin") swaps the design tokens in app.css; the
// appearance decides light or dark. Both are per-device preferences.

export type ThemeId = "classic" | "sessions";
export type Appearance = "auto" | "light" | "dark";

export interface ThemeDef {
  id: ThemeId;
  name: string;
  blurb: string;
  /** Appearance to switch to when the theme is picked (its home ground). */
  preferred: Appearance;
  /** Swatches for the picker: ground, card, accent, second accent. */
  swatch: { light: string[]; dark: string[] };
  /** Browser/status bar colour per mode. */
  bar: { light: string; dark: string };
}

export const THEMES: ThemeDef[] = [
  {
    id: "classic",
    name: "Classic",
    blurb: "Clean and familiar, in the style of iOS.",
    preferred: "auto",
    swatch: { light: ["#f2f2f7", "#ffffff", "#ff9500", "#5856d6"], dark: ["#000000", "#1c1c1e", "#ff9f0a", "#5e5ce6"] },
    bar: { light: "#f2f2f7", dark: "#000000" },
  },
  {
    id: "sessions",
    name: "Sessions",
    blurb: "BTC Sessions colours and type. Midnight first.",
    preferred: "dark",
    swatch: { light: ["#ffffff", "#f8f9fa", "#f7931a", "#1c6f95"], dark: ["#0d101c", "#161a2a", "#f7931a", "#7ed3f2"] },
    bar: { light: "#ffffff", dark: "#0d101c" },
  },
];

const dark = () => typeof matchMedia === "function" && matchMedia("(prefers-color-scheme: dark)").matches;

let current: { theme: ThemeId; appearance: Appearance } = { theme: "classic", appearance: "auto" };

/** Put the theme on <html>. Safe to call any time. */
export function applyTheme(theme: ThemeId, appearance: Appearance) {
  current = { theme, appearance };
  const mode = appearance === "auto" ? (dark() ? "dark" : "light") : appearance;
  const root = document.documentElement;
  root.dataset.skin = theme;
  root.dataset.mode = mode;
  root.style.colorScheme = mode;
  const def = THEMES.find((t) => t.id === theme) ?? THEMES[0];
  let meta = document.querySelector<HTMLMetaElement>('meta[name="theme-color"]:not([media])');
  if (!meta) {
    document.querySelectorAll('meta[name="theme-color"]').forEach((m) => m.remove());
    meta = document.createElement("meta");
    meta.name = "theme-color";
    document.head.appendChild(meta);
  }
  meta.content = def.bar[mode];
}

// Follow the system when the appearance is automatic.
if (typeof matchMedia === "function") {
  matchMedia("(prefers-color-scheme: dark)").addEventListener?.("change", () => {
    if (current.appearance === "auto") applyTheme(current.theme, current.appearance);
  });
}
