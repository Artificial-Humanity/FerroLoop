import type { QuotaWindowUsage, SessionQuotaUsage } from "../types/api";

/** Telemetry older than 1 hour is treated as stale (`status: "unknown"` on the dot). */
export const QUOTA_STALE_MS = 60 * 60 * 1000;

export type QuotaVisualStatus = "normal" | "yellow" | "orange" | "red" | "unknown";

export type QuotaReasonKind =
  | "normal"
  | "approaching_yellow"
  | "approaching_orange"
  | "below_1_percent"
  | "exhausted"
  | "stale"
  | "expired"
  | "missing";

export interface EvaluatedQuotaWindow {
  key: "primary" | "secondary";
  usedPercent: number;
  remainingPercent: number;
  windowMinutes: number | null;
  windowLabel: string;
  resetsAtMs: number | null;
  resetInLabel: string | null;
}

export interface StaleQuotaReading {
  usedPercent: number | null;
  remainingPercent: number | null;
  windowLabel: string | null;
  resetsAtMs: number | null;
  ageMs: number | null;
  ageLabel: string;
  summaryLabel: string;
}

export interface QuotaStatusEvaluation {
  status: QuotaVisualStatus;
  reasonKind: QuotaReasonKind;
  limitingWindow: EvaluatedQuotaWindow | null;
  exhaustedReason: string | null;
  staleReading: StaleQuotaReading | null;
  poolLabel: string | null;
  shortReason: string;
  detailText: string;
}

export interface EvaluateQuotaStatusInput {
  quotaUsage?: SessionQuotaUsage | null;
  vendor?: string | null;
  model?: string | null;
  nowMs?: number;
}

function isAntigravityVendor(vendor?: string | null): boolean {
  const normalized = (vendor ?? "").trim().toLowerCase();
  return (
    normalized === "antigravity" ||
    normalized === "agy" ||
    normalized === "argon" ||
    normalized === "gemini"
  );
}

export function formatQuotaPercent(value: number): string {
  const clamped = Math.max(0, Math.min(100, value));
  const rounded = Math.round(clamped * 10) / 10;
  return Number.isInteger(rounded) ? String(rounded) : rounded.toFixed(1);
}

export function formatQuotaWindowLabel(
  windowMinutes?: number | null,
  fallbackKey: "primary" | "secondary" = "primary",
): string {
  if (
    typeof windowMinutes === "number" &&
    Number.isFinite(windowMinutes) &&
    windowMinutes > 0
  ) {
    if (windowMinutes === 300) return "5h";
    if (windowMinutes === 10080) return "7d";
    if (windowMinutes % (24 * 60) === 0) {
      return `${Math.round(windowMinutes / (24 * 60))}d`;
    }
    if (windowMinutes % 60 === 0) {
      return `${Math.round(windowMinutes / 60)}h`;
    }
    return `${Math.round(windowMinutes)}m`;
  }
  return fallbackKey;
}

export function formatQuotaResetIn(
  resetsAtMs?: number | null,
  nowMs: number = Date.now(),
): string | null {
  if (typeof resetsAtMs !== "number" || !Number.isFinite(resetsAtMs)) {
    return null;
  }
  const diffMs = resetsAtMs - nowMs;
  if (diffMs <= 0) {
    return null;
  }
  const totalMinutes = Math.max(1, Math.round(diffMs / 60_000));
  if (totalMinutes >= 24 * 60) {
    const days = Math.floor(totalMinutes / (24 * 60));
    const hours = Math.floor((totalMinutes % (24 * 60)) / 60);
    return hours > 0 ? `resets in ${days}d ${hours}h` : `resets in ${days}d`;
  }
  if (totalMinutes >= 60) {
    const hours = Math.floor(totalMinutes / 60);
    const mins = totalMinutes % 60;
    return mins > 0 ? `resets in ${hours}h ${mins}m` : `resets in ${hours}h`;
  }
  return `resets in ${totalMinutes}m`;
}

export function formatQuotaAge(ageMs?: number | null): string {
  if (typeof ageMs !== "number" || !Number.isFinite(ageMs) || ageMs < 0) {
    return "age unknown";
  }
  const totalMinutes = Math.max(1, Math.round(ageMs / 60_000));
  if (totalMinutes >= 24 * 60) {
    const days = Math.round(totalMinutes / (24 * 60));
    return `${days} d ago`;
  }
  if (totalMinutes >= 60) {
    const hours = Math.round(totalMinutes / 60);
    return `${hours} h ago`;
  }
  return `${totalMinutes} m ago`;
}

export function resolveAntigravityModelPool(
  model?: string | null,
): "3p pool" | "gemini pool" | null {
  const lower = (model ?? "").trim().toLowerCase();
  if (!lower) return null;
  if (lower.startsWith("claude-") || lower.startsWith("gpt-")) {
    return "3p pool";
  }
  if (lower.startsWith("gemini-")) {
    return "gemini pool";
  }
  return null;
}

export function formatQuotaPoolLabel(
  vendor?: string | null,
  tier?: string | null,
  model?: string | null,
): string | null {
  const cleanTier = (tier ?? "").trim();
  if (isAntigravityVendor(vendor)) {
    const pool = resolveAntigravityModelPool(model);
    const tierSuffix =
      cleanTier && cleanTier.toLowerCase() !== "antigravity"
        ? ` (${cleanTier})`
        : "";
    if (pool) {
      return `antigravity · ${pool}${tierSuffix}`;
    }
    return `antigravity${tierSuffix}`;
  }
  const cleanVendor = (vendor ?? "").trim().toLowerCase();
  if (cleanVendor && cleanVendor !== "unknown") {
    if (cleanTier && cleanTier.toLowerCase() !== cleanVendor) {
      return `${cleanVendor} (${cleanTier})`;
    }
    return cleanVendor;
  }
  if (cleanTier) {
    return cleanTier;
  }
  return null;
}

interface ParsedWindowState {
  present: boolean;
  expired: boolean;
  hasResetTimestamp: boolean;
  active: EvaluatedQuotaWindow | null;
}

function parseQuotaWindow(
  window: QuotaWindowUsage | null | undefined,
  key: "primary" | "secondary",
  nowMs: number,
): ParsedWindowState {
  if (
    !window ||
    typeof window.used_percent !== "number" ||
    !Number.isFinite(window.used_percent)
  ) {
    return {
      present: false,
      expired: false,
      hasResetTimestamp: false,
      active: null,
    };
  }
  const hasResetTimestamp =
    typeof window.resets_at_ms === "number" &&
    Number.isFinite(window.resets_at_ms);
  const expired = hasResetTimestamp && window.resets_at_ms! <= nowMs;
  if (expired) {
    return {
      present: true,
      expired: true,
      hasResetTimestamp: true,
      active: null,
    };
  }
  const usedPercent = Math.max(0, Math.min(100, window.used_percent));
  const remainingPercent = Math.max(0, Math.min(100, 100 - usedPercent));
  const windowMinutes =
    typeof window.window_minutes === "number" &&
    Number.isFinite(window.window_minutes) &&
    window.window_minutes > 0
      ? window.window_minutes
      : null;
  const resetsAtMs = hasResetTimestamp ? window.resets_at_ms! : null;
  return {
    present: true,
    expired: false,
    hasResetTimestamp,
    active: {
      key,
      usedPercent,
      remainingPercent,
      windowMinutes,
      windowLabel: formatQuotaWindowLabel(windowMinutes, key),
      resetsAtMs,
      resetInLabel: formatQuotaResetIn(resetsAtMs, nowMs),
    },
  };
}

function selectLimitingWindow(
  primary: EvaluatedQuotaWindow | null,
  secondary: EvaluatedQuotaWindow | null,
): EvaluatedQuotaWindow | null {
  if (primary && secondary) {
    if (primary.remainingPercent !== secondary.remainingPercent) {
      return primary.remainingPercent < secondary.remainingPercent
        ? primary
        : secondary;
    }
    const primaryMins = primary.windowMinutes ?? Infinity;
    const secondaryMins = secondary.windowMinutes ?? Infinity;
    return primaryMins <= secondaryMins ? primary : secondary;
  }
  return primary ?? secondary ?? null;
}

function isExhaustedReasonExpired(
  rawReason: string,
  primary: ParsedWindowState,
  secondary: ParsedWindowState,
): boolean {
  const anyActiveAt100 =
    (primary.active !== null && primary.active.usedPercent >= 100) ||
    (secondary.active !== null && secondary.active.usedPercent >= 100);
  if (anyActiveAt100) {
    return false;
  }
  const windowsWithReset = [primary, secondary].filter((w) => w.hasResetTimestamp);
  if (
    windowsWithReset.length > 0 &&
    windowsWithReset.every((w) => w.expired)
  ) {
    return true;
  }
  const namesPrimary = /\b(primary|5h|five_hour|5-hour|5 hour)\b/i.test(rawReason);
  const namesSecondary = /\b(secondary|7d|weekly|7-day|7 day)\b/i.test(rawReason);
  if (namesPrimary && primary.expired) {
    return true;
  }
  if (namesSecondary && secondary.expired) {
    return true;
  }
  return false;
}

export function evaluateQuotaStatus(
  input: EvaluateQuotaStatusInput,
): QuotaStatusEvaluation {
  const nowMs =
    typeof input.nowMs === "number" && Number.isFinite(input.nowMs)
      ? input.nowMs
      : Date.now();
  const quota = input.quotaUsage ?? null;
  const poolLabel = formatQuotaPoolLabel(
    input.vendor,
    quota?.tier,
    input.model,
  );

  if (!quota) {
    return {
      status: "unknown",
      reasonKind: "missing",
      limitingWindow: null,
      exhaustedReason: null,
      staleReading: null,
      poolLabel,
      shortReason: "Quota: unknown",
      detailText: "Unknown (no telemetry reported)",
    };
  }

  const primary = parseQuotaWindow(quota.primary_window, "primary", nowMs);
  const secondary = parseQuotaWindow(quota.secondary_window, "secondary", nowMs);
  const rawExhaustedReason = (quota.exhausted_reason ?? "").trim() || null;

  if (!primary.present && !secondary.present && !rawExhaustedReason) {
    return {
      status: "unknown",
      reasonKind: "missing",
      limitingWindow: null,
      exhaustedReason: null,
      staleReading: null,
      poolLabel,
      shortReason: "Quota: unknown",
      detailText: "Unknown (no telemetry reported)",
    };
  }

  const activeExhaustedReason =
    rawExhaustedReason && !isExhaustedReasonExpired(rawExhaustedReason, primary, secondary)
      ? rawExhaustedReason
      : null;

  const limitingWindow = selectLimitingWindow(primary.active, secondary.active);

  if (!limitingWindow && !activeExhaustedReason) {
    return {
      status: "unknown",
      reasonKind: "expired",
      limitingWindow: null,
      exhaustedReason: null,
      staleReading: null,
      poolLabel,
      shortReason: "Quota: unknown (window reset passed)",
      detailText: "Unknown (previous quota window expired)",
    };
  }

  const observedAtMs = quota.observed_at_ms;
  const hasValidObservedAt =
    typeof observedAtMs === "number" && Number.isFinite(observedAtMs);
  const ageMs = hasValidObservedAt ? Math.max(0, nowMs - observedAtMs!) : null;
  const isFresh =
    hasValidObservedAt && nowMs - observedAtMs! <= QUOTA_STALE_MS;

  if (!isFresh) {
    const usedPercent = limitingWindow
      ? limitingWindow.usedPercent
      : activeExhaustedReason
        ? 100
        : null;
    const remainingPercent = limitingWindow
      ? limitingWindow.remainingPercent
      : activeExhaustedReason
        ? 0
        : null;
    const windowLabel = limitingWindow?.windowLabel ?? null;
    const resetsAtMs = limitingWindow?.resetsAtMs ?? null;
    const ageLabel = formatQuotaAge(ageMs);
    const valuePart =
      usedPercent !== null
        ? `${formatQuotaPercent(usedPercent)}% used`
        : "exhausted";
    const summaryLabel = `${valuePart}, ${ageLabel}`;
    const staleReading: StaleQuotaReading = {
      usedPercent,
      remainingPercent,
      windowLabel,
      resetsAtMs,
      ageMs,
      ageLabel,
      summaryLabel,
    };
    const windowPart = windowLabel ? `, ${windowLabel} window` : "";
    return {
      status: "unknown",
      reasonKind: "stale",
      limitingWindow,
      exhaustedReason: activeExhaustedReason,
      staleReading,
      poolLabel,
      shortReason: `Quota: unknown (${summaryLabel}${windowPart})`,
      detailText: `Unknown (stale: ${summaryLabel}${windowPart})`,
    };
  }

  const confirmedExhausted =
    Boolean(activeExhaustedReason) ||
    (limitingWindow !== null && limitingWindow.usedPercent >= 100);

  if (confirmedExhausted) {
    const metaParts = [
      limitingWindow ? `${limitingWindow.windowLabel} window` : null,
      "0% remaining",
      limitingWindow ? `${formatQuotaPercent(limitingWindow.usedPercent)}% used` : null,
      limitingWindow?.resetInLabel ?? null,
      activeExhaustedReason ? `reason: ${activeExhaustedReason}` : null,
    ].filter(Boolean);
    const metaStr = metaParts.join(", ");
    return {
      status: "red",
      reasonKind: "exhausted",
      limitingWindow,
      exhaustedReason: activeExhaustedReason,
      staleReading: null,
      poolLabel,
      shortReason: `Quota exhausted (${metaStr})`,
      detailText: `Quota exhausted · ${metaStr}`,
    };
  }

  // Limiting window is guaranteed non-null here since !confirmedExhausted and (limitingWindow || activeExhaustedReason).
  const win = limitingWindow!;
  const remStr = formatQuotaPercent(win.remainingPercent);
  const usedStr = formatQuotaPercent(win.usedPercent);
  const metaParts = [
    `${win.windowLabel} window`,
    `${usedStr}% used`,
    win.resetInLabel,
  ].filter(Boolean);
  const metaStr = metaParts.join(", ");

  if (win.remainingPercent < 1) {
    return {
      status: "red",
      reasonKind: "below_1_percent",
      limitingWindow: win,
      exhaustedReason: null,
      staleReading: null,
      poolLabel,
      shortReason: `Quota critical: <1% remaining (${remStr}% remaining, ${metaStr})`,
      detailText: `Critical (<1% remaining) · ${remStr}% remaining (${metaStr})`,
    };
  }

  if (win.remainingPercent <= 10) {
    return {
      status: "orange",
      reasonKind: "approaching_orange",
      limitingWindow: win,
      exhaustedReason: null,
      staleReading: null,
      poolLabel,
      shortReason: `Quota warning: ${remStr}% remaining (${metaStr})`,
      detailText: `Approaching limit (1–10% remaining) · ${remStr}% remaining (${metaStr})`,
    };
  }

  if (win.remainingPercent <= 20) {
    return {
      status: "yellow",
      reasonKind: "approaching_yellow",
      limitingWindow: win,
      exhaustedReason: null,
      staleReading: null,
      poolLabel,
      shortReason: `Quota warning: ${remStr}% remaining (${metaStr})`,
      detailText: `Approaching limit (10–20% remaining) · ${remStr}% remaining (${metaStr})`,
    };
  }

  return {
    status: "normal",
    reasonKind: "normal",
    limitingWindow: win,
    exhaustedReason: null,
    staleReading: null,
    poolLabel,
    shortReason: `Quota: ${remStr}% remaining (${metaStr})`,
    detailText: `Normal · ${remStr}% remaining (${metaStr})`,
  };
}

export interface RosterStatusDotPresentation {
  className: string;
  accessibleLabel: string;
}

export function resolveRosterStatusDot(
  livenessState: "alive" | "stale" | "dead",
  quotaEval: QuotaStatusEvaluation,
): RosterStatusDotPresentation {
  let className = "fd-dot fd-dot-alive";
  if (livenessState === "dead") {
    if (
      quotaEval.status === "red" ||
      quotaEval.status === "orange" ||
      quotaEval.status === "yellow"
    ) {
      className =
        quotaEval.status === "red"
          ? "fd-dot fd-dot-dead fd-dot-quota-red"
          : `fd-dot fd-dot-dead fd-dot-quota-${quotaEval.status} fd-dot-quota-red`;
    } else {
      className = "fd-dot fd-dot-dead";
    }
  } else if (quotaEval.status === "red") {
    className = "fd-dot fd-dot-dead fd-dot-quota-red";
  } else if (quotaEval.status === "orange") {
    className = "fd-dot fd-dot-quota-orange";
  } else if (quotaEval.status === "yellow") {
    className = "fd-dot fd-dot-stale fd-dot-quota-yellow";
  } else if (livenessState === "stale") {
    className = "fd-dot fd-dot-stale";
  } else {
    className = "fd-dot fd-dot-alive";
  }

  const livenessPart =
    livenessState === "dead"
      ? "Liveness: dead (process offline)"
      : `Liveness: ${livenessState}`;
  const poolPart = quotaEval.poolLabel ? ` · Pool: ${quotaEval.poolLabel}` : "";
  const accessibleLabel = `${livenessPart} · ${quotaEval.shortReason}${poolPart}`;

  return {
    className,
    accessibleLabel,
  };
}
