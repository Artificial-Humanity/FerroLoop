import { useEffect, useState } from "react";

export const TIMEZONE_STORAGE_KEY = "ferrodeck.timezone";

export interface TimeZoneOption {
  value: string;
  label: string;
}

export const TIMEZONE_OPTIONS: readonly TimeZoneOption[] = [
  { value: "UTC", label: "UTC" },
  { value: "local", label: "Browser Local Time" },
  { value: "America/New_York", label: "America/New_York (US Eastern)" },
  { value: "America/Chicago", label: "America/Chicago (US Central)" },
  { value: "America/Denver", label: "America/Denver (US Mountain)" },
  { value: "America/Phoenix", label: "America/Phoenix (US Arizona)" },
  { value: "America/Los_Angeles", label: "America/Los_Angeles (US Pacific)" },
  { value: "America/Anchorage", label: "America/Anchorage (US Alaska)" },
  { value: "Pacific/Honolulu", label: "Pacific/Honolulu (US Hawaii)" },
  { value: "Europe/London", label: "Europe/London (UK)" },
  { value: "Europe/Paris", label: "Europe/Paris (Central Europe)" },
  { value: "Europe/Berlin", label: "Europe/Berlin (Central Europe)" },
  { value: "Asia/Tokyo", label: "Asia/Tokyo (Japan)" },
  { value: "Asia/Shanghai", label: "Asia/Shanghai (China)" },
  { value: "Asia/Singapore", label: "Asia/Singapore (Singapore)" },
  { value: "Asia/Kolkata", label: "Asia/Kolkata (India)" },
  { value: "Australia/Sydney", label: "Australia/Sydney (Australia Eastern)" },
] as const;

export function normalizeTimeZone(raw: unknown, fallback = "UTC"): string {
  if (typeof raw !== "string") {
    return fallback;
  }
  const trimmed = raw.trim();
  if (!trimmed || trimmed.length > 64) {
    return fallback;
  }
  if (trimmed.toUpperCase() === "UTC") {
    return "UTC";
  }
  if (trimmed.toLowerCase() === "local") {
    return "local";
  }
  if (
    trimmed.startsWith("/") ||
    trimmed.endsWith("/") ||
    trimmed.includes("..") ||
    trimmed.includes("//") ||
    !/^[A-Za-z0-9_+\-/]+$/.test(trimmed)
  ) {
    return fallback;
  }
  return trimmed;
}

let memoryTimeZone = "UTC";
const listeners = new Set<(tz: string) => void>();

export function getActiveTimeZone(): string {
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      const stored = window.localStorage.getItem(TIMEZONE_STORAGE_KEY);
      if (stored !== null) {
        return normalizeTimeZone(stored, "UTC");
      }
      memoryTimeZone = "UTC";
      return "UTC";
    }
  } catch {
    // Ignore storage errors in restricted environments
  }
  return memoryTimeZone;
}

export function setActiveTimeZone(tz: string): string {
  const normalized = normalizeTimeZone(tz, "UTC");
  memoryTimeZone = normalized;
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      window.localStorage.setItem(TIMEZONE_STORAGE_KEY, normalized);
    }
  } catch {
    // Ignore storage errors
  }
  for (const listener of listeners) {
    listener(normalized);
  }
  return normalized;
}

export function subscribeActiveTimeZone(listener: (tz: string) => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function useActiveTimeZone(): string {
  const [tz, setTz] = useState<string>(() => getActiveTimeZone());
  useEffect(() => {
    setTz(getActiveTimeZone());
    return subscribeActiveTimeZone((next) => setTz(next));
  }, []);
  return tz;
}

function formatDateInZone(date: Date, tz: string): string {
  try {
    const formatter = new Intl.DateTimeFormat("en-CA", {
      ...(tz === "local" ? {} : { timeZone: tz }),
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      hour12: false,
      timeZoneName: "short",
    });
    const parts = formatter.formatToParts(date);
    const part = (type: Intl.DateTimeFormatPartTypes) =>
      parts.find((p) => p.type === type)?.value ?? "";
    const year = part("year");
    const month = part("month");
    const day = part("day");
    const rawHour = part("hour");
    const hour = rawHour === "24" ? "00" : rawHour;
    const minute = part("minute");
    const second = part("second");
    const tzName = part("timeZoneName");
    if (year && month && day && hour && minute && second) {
      return `${year}-${month}-${day} ${hour}:${minute}:${second}${tzName ? ` ${tzName}` : ""}`;
    }
  } catch {
    // Fall back to UTC string if runtime lacks the requested IANA zone
  }
  return date.toISOString().replace("T", " ").replace(/\.\d+Z$/, " UTC");
}

export function formatTimestamp(
  value: string | number | null | undefined,
  timeZone?: string,
): string {
  if (value === null || value === undefined || value === "") {
    return "";
  }
  const tz = normalizeTimeZone(timeZone ?? getActiveTimeZone(), "UTC");

  if (typeof value === "number") {
    if (!Number.isFinite(value) || value <= 0) {
      return "unknown time";
    }
    const date = new Date(value);
    if (tz === "UTC") {
      return date.toISOString().replace("T", " ").replace(/\.\d+Z$/, " UTC");
    }
    return formatDateInZone(date, tz);
  }

  const trimmed = value.trim();
  if (!trimmed || !/^\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}/.test(trimmed)) {
    return value;
  }
  const ms = Date.parse(trimmed);
  if (Number.isNaN(ms)) {
    return value;
  }
  if (tz === "UTC") {
    if (/[Zz]$/.test(trimmed)) {
      return trimmed;
    }
    return new Date(ms).toISOString().replace(/\.\d{3}Z$/, "Z");
  }
  return formatDateInZone(new Date(ms), tz);
}
