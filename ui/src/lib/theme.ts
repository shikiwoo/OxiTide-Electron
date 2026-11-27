/// Catppuccin flavours (catppuccin.css): palettes laid over the dark or light
/// recipe through `data-palette`. Mocha OLED is Mocha on true black.
export const CATPPUCCIN_FLAVOURS = ["latte", "frappe", "macchiato", "mocha", "mocha-oled"] as const;
export type CatppuccinFlavour = (typeof CATPPUCCIN_FLAVOURS)[number];
export type CatppuccinTheme = `catppuccin-${CatppuccinFlavour}`;

export type ThemeSetting = "dark" | "light" | "system" | CatppuccinTheme;
export type ResolvedTheme = "dark" | "light";

export const THEME_SETTINGS: readonly ThemeSetting[] = [
  "dark",
  "light",
  "system",
  ...CATPPUCCIN_FLAVOURS.map((f): CatppuccinTheme => `catppuccin-${f}`),
];

/// The Catppuccin flavour a theme paints (`data-palette`), or null for
/// OxiTide's own Warm Graphite recipe.
export function themeFlavour(setting: ThemeSetting): CatppuccinFlavour | null {
  if (!setting.startsWith("catppuccin-")) return null;
  const flavour = setting.slice("catppuccin-".length);
  return (CATPPUCCIN_FLAVOURS as readonly string[]).includes(flavour) ? (flavour as CatppuccinFlavour) : null;
}

export const THEME_STORAGE_KEY = "oxitide.theme";
/// Mode and Studio tone are mirrored the same way, so the pre-paint script
/// (index.html) draws the first frame in the right look.
export const MODE_STORAGE_KEY = "oxitide.mode";
export const TONE_STORAGE_KEY = "oxitide.tone";

export interface ThemeStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

/// `System` follows the OS preference; explicit settings win (spec §6.1).
export function resolveTheme(setting: ThemeSetting, prefersDark: boolean): ResolvedTheme {
  if (setting === "system") return prefersDark ? "dark" : "light";
  const flavour = themeFlavour(setting);
  if (flavour) return flavour === "latte" ? "light" : "dark";
  return setting === "light" ? "light" : "dark";
}

/// Reads a mirrored setting; an absent, unknown or unreadable value means the
/// config default, so first paint is never wrong for existing installs.
export function readMirror<T extends string>(
  storage: ThemeStorage | undefined,
  key: string,
  allowed: readonly T[],
  fallback: T,
): T {
  try {
    const value = storage?.getItem(key);
    if (value && (allowed as readonly string[]).includes(value)) return value as T;
  } catch {
    // Storage may be unavailable; the config default is the fallback.
  }
  return fallback;
}

export function writeMirror(storage: ThemeStorage | undefined, key: string, value: string): void {
  try {
    storage?.setItem(key, value);
  } catch {
    // Non-fatal: the next boot falls back to the default and init reconciles.
  }
}

/// The pre-paint script reads the same mirror (Review Focus 1/3).
export function readThemeMirror(storage?: ThemeStorage): ThemeSetting {
  return readMirror(storage, THEME_STORAGE_KEY, THEME_SETTINGS, "dark");
}

export function writeThemeMirror(storage: ThemeStorage | undefined, setting: ThemeSetting): void {
  writeMirror(storage, THEME_STORAGE_KEY, setting);
}
