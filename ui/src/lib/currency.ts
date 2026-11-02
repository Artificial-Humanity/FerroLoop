import { useEffect, useState } from "react";

export const CURRENCY_STORAGE_KEY = "ferrodeck.currency";

export type SupportedCurrencyCode =
  | "USD"
  | "EUR"
  | "GBP"
  | "CAD"
  | "AUD"
  | "JPY"
  | "CHF"
  | "CNY"
  | "INR"
  | "SGD"
  | "KRW"
  | "BRL"
  | "MXN"
  | "NZD"
  | "HKD"
  | "SEK";

export type CurrencyPreference = "auto" | SupportedCurrencyCode;

export interface CurrencyOption {
  value: CurrencyPreference;
  label: string;
}

export const USD_CONVERSION_RATES: Readonly<Record<SupportedCurrencyCode, number>> = {
  USD: 1,
  EUR: 0.92,
  GBP: 0.79,
  CAD: 1.38,
  AUD: 1.52,
  JPY: 152,
  CHF: 0.88,
  CNY: 7.24,
  INR: 84.0,
  SGD: 1.34,
  KRW: 1380,
  BRL: 5.75,
  MXN: 20.0,
  NZD: 1.66,
  HKD: 7.78,
  SEK: 10.6,
};

export const CURRENCY_OPTIONS: readonly CurrencyOption[] = [
  { value: "auto", label: "Auto-Detect" },
  { value: "USD", label: "USD ($ — US Dollar)" },
  { value: "EUR", label: "EUR (€ — Euro)" },
  { value: "GBP", label: "GBP (£ — British Pound)" },
  { value: "CAD", label: "CAD (CA$ — Canadian Dollar)" },
  { value: "AUD", label: "AUD (A$ — Australian Dollar)" },
  { value: "JPY", label: "JPY (¥ — Japanese Yen)" },
  { value: "CHF", label: "CHF (CHF — Swiss Franc)" },
  { value: "CNY", label: "CNY (CN¥ — Chinese Yuan)" },
  { value: "INR", label: "INR (₹ — Indian Rupee)" },
  { value: "SGD", label: "SGD (SGD — Singapore Dollar)" },
  { value: "KRW", label: "KRW (₩ — South Korean Won)" },
  { value: "BRL", label: "BRL (R$ — Brazilian Real)" },
  { value: "MXN", label: "MXN (MX$ — Mexican Peso)" },
  { value: "NZD", label: "NZD (NZ$ — New Zealand Dollar)" },
  { value: "HKD", label: "HKD (HK$ — Hong Kong Dollar)" },
  { value: "SEK", label: "SEK (SEK — Swedish Krona)" },
] as const;

const REGION_TO_CURRENCY: Readonly<Record<string, SupportedCurrencyCode>> = {
  US: "USD",
  PR: "USD",
  GU: "USD",
  VI: "USD",
  GB: "GBP",
  UK: "GBP",
  CA: "CAD",
  AU: "AUD",
  JP: "JPY",
  CH: "CHF",
  LI: "CHF",
  CN: "CNY",
  IN: "INR",
  SG: "SGD",
  KR: "KRW",
  BR: "BRL",
  MX: "MXN",
  NZ: "NZD",
  HK: "HKD",
  SE: "SEK",
  // Eurozone countries
  DE: "EUR",
  FR: "EUR",
  IT: "EUR",
  ES: "EUR",
  NL: "EUR",
  BE: "EUR",
  AT: "EUR",
  IE: "EUR",
  FI: "EUR",
  PT: "EUR",
  GR: "EUR",
  LU: "EUR",
  SK: "EUR",
  SI: "EUR",
  EE: "EUR",
  LV: "EUR",
  LT: "EUR",
  CY: "EUR",
  MT: "EUR",
  HR: "EUR",
};

const TIMEZONE_TO_CURRENCY: Readonly<Record<string, SupportedCurrencyCode>> = {
  "America/New_York": "USD",
  "America/Chicago": "USD",
  "America/Denver": "USD",
  "America/Phoenix": "USD",
  "America/Los_Angeles": "USD",
  "America/Anchorage": "USD",
  "Pacific/Honolulu": "USD",
  "Europe/London": "GBP",
  "Europe/Belfast": "GBP",
  "Europe/Paris": "EUR",
  "Europe/Berlin": "EUR",
  "Europe/Madrid": "EUR",
  "Europe/Rome": "EUR",
  "Europe/Amsterdam": "EUR",
  "Europe/Brussels": "EUR",
  "Europe/Vienna": "EUR",
  "Europe/Dublin": "EUR",
  "Europe/Lisbon": "EUR",
  "Europe/Athens": "EUR",
  "Europe/Helsinki": "EUR",
  "Europe/Luxembourg": "EUR",
  "America/Toronto": "CAD",
  "America/Vancouver": "CAD",
  "America/Montreal": "CAD",
  "America/Edmonton": "CAD",
  "America/Winnipeg": "CAD",
  "America/Halifax": "CAD",
  "Australia/Sydney": "AUD",
  "Australia/Melbourne": "AUD",
  "Australia/Brisbane": "AUD",
  "Australia/Perth": "AUD",
  "Australia/Adelaide": "AUD",
  "Asia/Tokyo": "JPY",
  "Europe/Zurich": "CHF",
  "Asia/Shanghai": "CNY",
  "Asia/Chongqing": "CNY",
  "Asia/Urumqi": "CNY",
  "Asia/Kolkata": "INR",
  "Asia/Calcutta": "INR",
  "Asia/Singapore": "SGD",
  "Asia/Seoul": "KRW",
  "America/Sao_Paulo": "BRL",
  "America/Mexico_City": "MXN",
  "Pacific/Auckland": "NZD",
  "Asia/Hong_Kong": "HKD",
  "Europe/Stockholm": "SEK",
};

function extractRegionSubtag(localeTag: string): string | null {
  const trimmed = localeTag.trim();
  if (!trimmed) return null;
  try {
    if (typeof Intl !== "undefined" && typeof Intl.Locale === "function") {
      const parsed = new Intl.Locale(trimmed);
      if (parsed.region) {
        return parsed.region.toUpperCase();
      }
    }
  } catch {
    // Fallback to regex parsing below
  }
  const match = /^[A-Za-z]{2,3}[-_](?:[A-Za-z]{4}[-_])?([A-Za-z]{2}|\d{3})\b/.exec(
    trimmed,
  );
  return match ? match[1].toUpperCase() : null;
}

function currencyFromTimeZone(tz: string | undefined | null): SupportedCurrencyCode | null {
  if (!tz || typeof tz !== "string") return null;
  const trimmed = tz.trim();
  if (!trimmed) return null;
  if (trimmed in TIMEZONE_TO_CURRENCY) {
    return TIMEZONE_TO_CURRENCY[trimmed];
  }
  if (trimmed.startsWith("Australia/")) {
    return "AUD";
  }
  return null;
}

export interface BrowserCurrencyDetectInputs {
  languages?: readonly string[];
  language?: string;
  timeZone?: string;
}

export function detectBrowserCurrency(
  inputs?: BrowserCurrencyDetectInputs,
): SupportedCurrencyCode {
  const candidateLocales: string[] = [];

  if (inputs?.languages && inputs.languages.length > 0) {
    candidateLocales.push(...inputs.languages);
  } else if (inputs?.language) {
    candidateLocales.push(inputs.language);
  } else if (typeof navigator !== "undefined") {
    if (Array.isArray(navigator.languages) && navigator.languages.length > 0) {
      candidateLocales.push(...navigator.languages);
    }
    if (typeof navigator.language === "string" && navigator.language) {
      candidateLocales.push(navigator.language);
    }
  }

  let resolvedTimeZone = inputs?.timeZone;
  if (resolvedTimeZone === undefined) {
    try {
      const opts = Intl.DateTimeFormat().resolvedOptions();
      resolvedTimeZone = opts.timeZone;
      if (candidateLocales.length === 0 && opts.locale) {
        candidateLocales.push(opts.locale);
      }
    } catch {
      // Ignore Intl errors
    }
  }

  const tzCurrency = currencyFromTimeZone(resolvedTimeZone);

  let matchedRegionCurrency: SupportedCurrencyCode | null = null;
  for (const tag of candidateLocales) {
    if (typeof tag !== "string") continue;
    const region = extractRegionSubtag(tag);
    if (region && region in REGION_TO_CURRENCY) {
      matchedRegionCurrency = REGION_TO_CURRENCY[region];
      break;
    }
  }

  // If the locale explicitly specifies a non-US region (e.g. en-GB, de-DE, ja-JP), honor it directly.
  if (matchedRegionCurrency && matchedRegionCurrency !== "USD") {
    return matchedRegionCurrency;
  }

  // When browser language is default en-US (or has no region), check IANA timezone before falling back to USD.
  if (tzCurrency) {
    return tzCurrency;
  }

  if (matchedRegionCurrency) {
    return matchedRegionCurrency;
  }

  return "USD";
}

export function isSupportedCurrencyCode(raw: string): raw is SupportedCurrencyCode {
  return Object.prototype.hasOwnProperty.call(USD_CONVERSION_RATES, raw);
}

export function normalizeCurrencyPreference(
  raw: unknown,
  fallback: CurrencyPreference = "auto",
): CurrencyPreference {
  if (typeof raw !== "string") {
    return fallback;
  }
  const trimmed = raw.trim();
  if (!trimmed) {
    return fallback;
  }
  if (trimmed.toLowerCase() === "auto") {
    return "auto";
  }
  const upper = trimmed.toUpperCase();
  if (isSupportedCurrencyCode(upper)) {
    return upper;
  }
  return fallback;
}

export function resolveCurrencyCode(
  pref?: string | null,
  detectInputs?: BrowserCurrencyDetectInputs,
): SupportedCurrencyCode {
  const normalized = normalizeCurrencyPreference(
    pref ?? getActiveCurrency(),
    "auto",
  );
  if (normalized === "auto") {
    return detectBrowserCurrency(detectInputs);
  }
  return normalized;
}

let memoryCurrency: CurrencyPreference = "auto";
const listeners = new Set<(currency: CurrencyPreference) => void>();

export function getActiveCurrency(): CurrencyPreference {
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      const stored = window.localStorage.getItem(CURRENCY_STORAGE_KEY);
      if (stored !== null) {
        return normalizeCurrencyPreference(stored, "auto");
      }
      memoryCurrency = "auto";
      return "auto";
    }
  } catch {
    // Ignore storage errors in restricted environments
  }
  return memoryCurrency;
}

export function setActiveCurrency(pref: string): CurrencyPreference {
  const normalized = normalizeCurrencyPreference(pref, "auto");
  memoryCurrency = normalized;
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      window.localStorage.setItem(CURRENCY_STORAGE_KEY, normalized);
    }
  } catch {
    // Ignore storage errors
  }
  for (const listener of listeners) {
    listener(normalized);
  }
  return normalized;
}

export function subscribeActiveCurrency(
  listener: (currency: CurrencyPreference) => void,
): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function useActiveCurrency(): CurrencyPreference {
  const [currency, setCurrency] = useState<CurrencyPreference>(() =>
    getActiveCurrency(),
  );
  useEffect(() => {
    setCurrency(getActiveCurrency());
    return subscribeActiveCurrency((next) => setCurrency(next));
  }, []);
  return currency;
}

export interface FormatCurrencyOptions {
  minimumFractionDigits?: number;
  maximumFractionDigits?: number;
}

export function formatCurrencyFromUsd(
  amountUsd: number | null | undefined,
  currencyPref?: string,
  options?: FormatCurrencyOptions,
): string {
  const safeUsd =
    typeof amountUsd === "number" && Number.isFinite(amountUsd) && amountUsd >= 0
      ? amountUsd
      : 0;
  const code = resolveCurrencyCode(currencyPref);
  const rate = USD_CONVERSION_RATES[code] ?? 1;
  const converted = safeUsd * rate;

  const isZeroDecimal = code === "JPY" || code === "KRW";
  const defaultDigits =
    isZeroDecimal && (converted === 0 || Math.abs(converted) >= 1) ? 0 : 2;
  const minDigits = options?.minimumFractionDigits ?? defaultDigits;
  const maxDigits =
    options?.maximumFractionDigits ?? Math.max(minDigits, defaultDigits);

  try {
    return new Intl.NumberFormat("en-US", {
      style: "currency",
      currency: code,
      minimumFractionDigits: minDigits,
      maximumFractionDigits: maxDigits,
    }).format(converted);
  } catch {
    return `$${safeUsd.toFixed(minDigits)}`;
  }
}

export function formatCurrencyFromMicros(
  micros: number | null | undefined,
  currencyPref?: string,
  options?: FormatCurrencyOptions,
): string {
  const safeMicros =
    typeof micros === "number" && Number.isFinite(micros) && micros >= 0
      ? micros
      : 0;
  return formatCurrencyFromUsd(safeMicros / 1_000_000, currencyPref, options);
}
