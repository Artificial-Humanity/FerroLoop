export type ThemeMode = "dark" | "light" | "system";

export const THEME_STORAGE_KEY = "fd_theme_mode";
export const LEGACY_THEME_STORAGE_KEY = "ferrodeck.theme";

export const FONT_SCALE_STORAGE_KEY = "fd_font_scale_pct";
export const FONT_SCALE_MIN_PCT = 90;
export const FONT_SCALE_MAX_PCT = 125;
export const FONT_SCALE_STEP_PCT = 5;
export const FONT_SCALE_PRESETS = [90, 100, 110, 120, 125] as const;

export function resolveSystemTheme(): "dark" | "light" {
  try {
    if (
      typeof window !== "undefined" &&
      typeof window.matchMedia === "function" &&
      window.matchMedia("(prefers-color-scheme: light)").matches
    ) {
      return "light";
    }
  } catch {
    // Ignore matchMedia errors.
  }
  return "dark";
}

export function resolveEffectiveTheme(mode: ThemeMode): "dark" | "light" {
  if (mode === "light" || mode === "dark") {
    return mode;
  }
  return resolveSystemTheme();
}

export function getInitialThemeMode(): ThemeMode {
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      const stored =
        window.localStorage.getItem(THEME_STORAGE_KEY) ??
        window.localStorage.getItem(LEGACY_THEME_STORAGE_KEY);
      if (stored === "light" || stored === "dark" || stored === "system") {
        return stored;
      }
    }
  } catch {
    // Ignore storage access errors in private/sandboxed contexts.
  }
  return resolveSystemTheme();
}

export function applyThemeMode(mode: ThemeMode): void {
  if (typeof document !== "undefined") {
    document.documentElement.setAttribute(
      "data-theme",
      resolveEffectiveTheme(mode),
    );
  }
}

export function persistThemeMode(mode: ThemeMode): void {
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      window.localStorage.setItem(THEME_STORAGE_KEY, mode);
    }
  } catch {
    // Ignore storage errors.
  }
}

export function clampFontScalePct(raw: number): number {
  if (!Number.isFinite(raw)) {
    return 100;
  }
  return Math.max(
    FONT_SCALE_MIN_PCT,
    Math.min(FONT_SCALE_MAX_PCT, Math.round(raw)),
  );
}

export function getInitialFontScalePct(): number {
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      const stored = window.localStorage.getItem(FONT_SCALE_STORAGE_KEY);
      if (stored !== null && stored.trim() !== "") {
        return clampFontScalePct(Number(stored));
      }
    }
  } catch {
    // Ignore storage errors.
  }
  return 100;
}

export function persistFontScalePct(pct: number): void {
  const clamped = clampFontScalePct(pct);
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      window.localStorage.setItem(FONT_SCALE_STORAGE_KEY, String(clamped));
    }
  } catch {
    // Ignore storage errors.
  }
}

export function applyFontScaleToElementTree(
  root: ParentNode,
  pct: number,
): void {
  const clamped = clampFontScalePct(pct);
  const scale = clamped / 100;
  const elements = root.querySelectorAll<HTMLElement>(
    "[style], [data-fd-base-font-px]",
  );
  elements.forEach((el) => {
    const existingBase = el.getAttribute("data-fd-base-font-px");
    const currentInline = el.style.fontSize;
    let basePx: number | null = null;
    if (existingBase !== null) {
      const parsed = Number(existingBase);
      if (Number.isFinite(parsed) && parsed > 0) {
        basePx = parsed;
      }
    }
    if (basePx === null && currentInline) {
      const match = /^(\d+(?:\.\d+)?)px$/i.exec(currentInline.trim());
      if (match) {
        basePx = Number(match[1]);
        if (clamped !== 100) {
          el.setAttribute("data-fd-base-font-px", match[1]);
        }
      }
    }
    if (basePx !== null) {
      if (clamped === 100) {
        const rawAttr = existingBase ?? String(basePx);
        el.style.fontSize = `${rawAttr}px`;
        el.removeAttribute("data-fd-base-font-px");
      } else {
        if (existingBase === null) {
          el.setAttribute("data-fd-base-font-px", String(basePx));
        }
        const scaled = Math.round(basePx * scale * 100) / 100;
        el.style.fontSize = `${scaled}px`;
      }
    }
  });
}

export function applyFontScalePct(pct: number): void {
  const clamped = clampFontScalePct(pct);
  if (typeof document !== "undefined") {
    document.documentElement.style.setProperty(
      "--fd-font-scale",
      String(clamped / 100),
    );
    const cockpit = document.querySelector(".fd-cockpit");
    if (cockpit) {
      applyFontScaleToElementTree(cockpit, clamped);
    } else if (document.body) {
      applyFontScaleToElementTree(document.body, clamped);
    }
  }
}
