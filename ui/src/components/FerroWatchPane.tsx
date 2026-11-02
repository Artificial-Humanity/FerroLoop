import React, { useCallback, useEffect, useMemo, useState } from "react";
import type { HubApiClient } from "../api/client";
import { formatTimestamp, useActiveTimeZone } from "../lib/datetime";
import {
  DEFAULT_WATCH_BALANCE_POLICY,
  DEFAULT_WATCH_THRESHOLDS,
  type AgentRecord,
  type DiscoveredSessionSummary,
  type HubAuthority,
  type RemodelMode,
  type RemodelSessionResponse,
  type WatchAgentHealth,
  type WatchAlert,
  type WatchBalanceApplyResponse,
  type WatchBalancePlan,
  type WatchBalancePolicy,
  type WatchQuotaWindow,
  type WatchSeverity,
  type WatchSummaryResponse,
  type WatchTelemetryReport,
  type WatchThresholdOverrides,
} from "../types/api";
import { getAdapterBadgeClass } from "./RosterSidebar";

export interface FerroWatchPaneProps {
  authority?: HubAuthority | null;
  agents?: AgentRecord[];
  sessions?: DiscoveredSessionSummary[];
  apiClient?: HubApiClient;
  onAfterRemodel?: (res: RemodelSessionResponse) => void;
}

export const CANONICAL_WATCH_VENDORS = [
  "claude",
  "codex",
  "grok",
  "antigravity",
  "muse",
] as const;

export type WatchSectionFilter =
  | "all"
  | "alerts"
  | "quotas"
  | "liveness"
  | "rollups";

export function formatUsdFromMicros(micros: number | undefined | null): string {
  const safe =
    typeof micros === "number" && Number.isFinite(micros) && micros >= 0
      ? micros
      : 0;
  const dollars = safe / 1_000_000;
  return `$${dollars.toFixed(2)}`;
}

export function formatCompactNumber(value: number | undefined | null): string {
  const safe =
    typeof value === "number" && Number.isFinite(value) ? value : 0;
  return safe.toLocaleString("en-US");
}

export function formatDurationMs(ms: number | undefined | null): string {
  if (typeof ms !== "number" || !Number.isFinite(ms) || ms < 0) {
    return "—";
  }
  if (ms < 1_000) {
    return `${Math.round(ms)}ms`;
  }
  const totalSecs = Math.floor(ms / 1_000);
  if (totalSecs < 60) {
    return `${totalSecs}s`;
  }
  const mins = Math.floor(totalSecs / 60);
  const secs = totalSecs % 60;
  if (mins < 60) {
    return secs > 0 ? `${mins}m ${secs}s` : `${mins}m`;
  }
  const hours = Math.floor(mins / 60);
  const remMins = mins % 60;
  if (hours < 24) {
    return remMins > 0 ? `${hours}h ${remMins}m` : `${hours}h`;
  }
  const days = Math.floor(hours / 24);
  const remHours = hours % 24;
  return remHours > 0 ? `${days}d ${remHours}h` : `${days}d`;
}

export function formatResetCountdown(
  resetsAtMs: number | undefined | null,
  nowMs: number = Date.now(),
): string {
  if (typeof resetsAtMs !== "number" || !Number.isFinite(resetsAtMs) || resetsAtMs <= 0) {
    return "No reset window";
  }
  const delta = resetsAtMs - nowMs;
  if (delta <= 0) {
    return "Reset due";
  }
  return `Resets in ${formatDurationMs(delta)}`;
}

function getSeverityBadgeStyle(severity: WatchSeverity): React.CSSProperties {
  if (severity === "critical") {
    return {
      backgroundColor: "rgba(239, 68, 68, 0.16)",
      color: "#F87171",
      borderColor: "rgba(239, 68, 68, 0.45)",
      fontWeight: 700,
      textTransform: "uppercase",
    };
  }
  if (severity === "warning") {
    return {
      backgroundColor: "rgba(245, 158, 11, 0.16)",
      color: "#FBBF24",
      borderColor: "rgba(245, 158, 11, 0.4)",
      fontWeight: 700,
      textTransform: "uppercase",
    };
  }
  return {
    backgroundColor: "rgba(56, 189, 248, 0.14)",
    color: "#38BDF8",
    borderColor: "rgba(56, 189, 248, 0.35)",
    fontWeight: 700,
    textTransform: "uppercase",
  };
}

function getLivenessBadgeStyle(
  liveness: WatchAgentHealth["liveness"],
): React.CSSProperties {
  switch (liveness) {
    case "live":
      return {
        backgroundColor: "rgba(16, 185, 129, 0.15)",
        color: "#34D399",
        borderColor: "rgba(16, 185, 129, 0.35)",
      };
    case "stale":
      return {
        backgroundColor: "rgba(245, 158, 11, 0.16)",
        color: "#FBBF24",
        borderColor: "rgba(245, 158, 11, 0.4)",
      };
    case "dead":
      return {
        backgroundColor: "rgba(239, 68, 68, 0.16)",
        color: "#F87171",
        borderColor: "rgba(239, 68, 68, 0.45)",
      };
    case "exited":
    case "missing":
    default:
      return {
        backgroundColor: "var(--bg-elevated)",
        color: "var(--text-muted)",
        borderColor: "var(--border-subtle)",
      };
  }
}

function getQuotaBarColor(
  pct: number,
  warnPct: number,
  critPct: number,
  exhausted = false,
): string {
  if (exhausted || pct >= critPct) {
    return "var(--status-danger, #EF4444)";
  }
  if (pct >= warnPct) {
    return "var(--status-warning, #F59E0B)";
  }
  return "var(--status-ok, #10B981)";
}

interface AggregatedVendorQuota {
  vendor: string;
  tier: string;
  primaryUsedPct: number | null;
  primaryResetsAtMs: number | null;
  secondaryUsedPct: number | null;
  secondaryResetsAtMs: number | null;
  exhaustedReason: string | null;
  activePersonas: string[];
  models: string[];
}

export const FerroWatchPane: React.FC<FerroWatchPaneProps> = ({
  authority = null,
  agents = [],
  sessions = [],
  apiClient,
  onAfterRemodel,
}) => {
  const isOwner = authority?.tier === "owner";
  const activeTimeZone = useActiveTimeZone();

  const [sectionFilter, setSectionFilter] = useState<WatchSectionFilter>("all");
  const [summary, setSummary] = useState<WatchSummaryResponse | null>(null);
  const [scopedRollups, setScopedRollups] = useState<WatchTelemetryReport | null>(
    null,
  );
  const [selectedProjectSlug, setSelectedProjectSlug] = useState<string>("");
  const [loading, setLoading] = useState<boolean>(false);
  const [error, setError] = useState<string | null>(null);

  // Interactive Threshold Inputs
  const [warnQuotaInput, setWarnQuotaInput] = useState<string>(
    String(DEFAULT_WATCH_THRESHOLDS.warn_quota_pct),
  );
  const [critQuotaInput, setCritQuotaInput] = useState<string>(
    String(DEFAULT_WATCH_THRESHOLDS.critical_quota_pct),
  );
  const [warnCostUsdInput, setWarnCostUsdInput] = useState<string>(
    String(DEFAULT_WATCH_THRESHOLDS.warn_cost_usd_micros / 1_000_000),
  );
  const [critCostUsdInput, setCritCostUsdInput] = useState<string>(
    String(DEFAULT_WATCH_THRESHOLDS.critical_cost_usd_micros / 1_000_000),
  );
  const [staleMsInput, setStaleMsInput] = useState<string>(
    String(DEFAULT_WATCH_THRESHOLDS.stale_after_ms),
  );
  const [appliedOverrides, setAppliedOverrides] = useState<
    WatchThresholdOverrides | undefined
  >(undefined);
  const [thresholdError, setThresholdError] = useState<string | null>(null);

  // Direct Re-Model Controls
  const [remodelTarget, setRemodelTarget] = useState<string>("");
  const [remodelVendor, setRemodelVendor] = useState<string>("codex");
  const [remodelModel, setRemodelModel] = useState<string>("");
  const [remodelMode, setRemodelMode] = useState<RemodelMode>("execute");
  const [remodeling, setRemodeling] = useState<boolean>(false);
  const [remodelStatus, setRemodelStatus] = useState<string | null>(null);
  const [remodelError, setRemodelError] = useState<string | null>(null);

  // Subscription Auto-Balance Controls
  const [balancePolicy, setBalancePolicy] = useState<WatchBalancePolicy>(
    DEFAULT_WATCH_BALANCE_POLICY,
  );
  const [balancePlan, setBalancePlan] = useState<WatchBalancePlan | null>(null);
  const [balanceApplyRes, setBalanceApplyRes] =
    useState<WatchBalanceApplyResponse | null>(null);
  const [balanceBusy, setBalanceBusy] = useState<boolean>(false);
  const [balanceMessage, setBalanceMessage] = useState<string | null>(null);
  const [balanceError, setBalanceError] = useState<string | null>(null);

  const loadSummary = useCallback(
    async (overrides?: WatchThresholdOverrides) => {
      if (!apiClient) return;
      setLoading(true);
      setError(null);
      try {
        const [summaryRes, balanceRes] = await Promise.all([
          apiClient.getWatchSummary(overrides),
          apiClient.getWatchBalance().catch(() => null),
        ]);
        setSummary(summaryRes);
        if (balanceRes?.policy) {
          setBalancePolicy(balanceRes.policy);
        }
      } catch (err) {
        setError(
          err instanceof Error ? err.message : "Failed to load FerroWatch summary.",
        );
      } finally {
        setLoading(false);
      }
    },
    [apiClient],
  );

  useEffect(() => {
    void loadSummary(appliedOverrides);
  }, [loadSummary, appliedOverrides]);

  useEffect(() => {
    if (!apiClient) return;
    if (!selectedProjectSlug.trim()) {
      setScopedRollups(null);
      return;
    }
    let cancelled = false;
    void apiClient
      .getWatchRollups(selectedProjectSlug.trim())
      .then((res) => {
        if (!cancelled) {
          setScopedRollups(res.telemetry);
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setError(
            err instanceof Error
              ? err.message
              : "Failed to load project rollups.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [apiClient, selectedProjectSlug]);

  const nowMs = summary?.health?.now_ms ?? Date.now();
  const activeThresholds = summary?.thresholds ?? DEFAULT_WATCH_THRESHOLDS;

  // Build Vendor Quota & Usage Matrix across canonical + discovered vendors
  const vendorMatrix = useMemo<AggregatedVendorQuota[]>(() => {
    const map = new Map<string, AggregatedVendorQuota>();
    const ensureVendor = (rawVendor: string): AggregatedVendorQuota => {
      const key = rawVendor.trim().toLowerCase() || "unknown";
      let entry = map.get(key);
      if (!entry) {
        const defaultTier =
          key === "claude"
            ? "Max 20x"
            : key === "codex"
              ? "Pro"
              : key === "grok"
                ? "SuperGrok"
                : key === "antigravity"
                  ? "Workspace"
                  : key === "muse"
                    ? "Local Engine"
                    : "Subscription";
        entry = {
          vendor: key,
          tier: defaultTier,
          primaryUsedPct: null,
          primaryResetsAtMs: null,
          secondaryUsedPct: null,
          secondaryResetsAtMs: null,
          exhaustedReason: null,
          activePersonas: [],
          models: [],
        };
        map.set(key, entry);
      }
      return entry;
    };

    for (const v of CANONICAL_WATCH_VENDORS) {
      ensureVendor(v);
    }

    const quotaWindows: WatchQuotaWindow[] = summary?.quotas ?? [];
    for (const q of quotaWindows) {
      const entry = ensureVendor(q.vendor);
      if (q.window === "primary") {
        if (
          entry.primaryUsedPct === null ||
          q.used_percent > entry.primaryUsedPct
        ) {
          entry.primaryUsedPct = q.used_percent;
          entry.primaryResetsAtMs = q.resets_at_ms ?? entry.primaryResetsAtMs;
        }
      } else if (q.window === "secondary") {
        if (
          entry.secondaryUsedPct === null ||
          q.used_percent > entry.secondaryUsedPct
        ) {
          entry.secondaryUsedPct = q.used_percent;
          entry.secondaryResetsAtMs =
            q.resets_at_ms ?? entry.secondaryResetsAtMs;
        }
      }
      if (q.exhausted_reason && !entry.exhaustedReason) {
        entry.exhaustedReason = q.exhausted_reason;
      }
    }

    for (const sess of sessions) {
      const entry = ensureVendor(sess.adapter || "claude");
      if (sess.quota_usage?.tier) {
        entry.tier = sess.quota_usage.tier;
      }
      const pWin = sess.quota_usage?.primary_window;
      if (pWin && typeof pWin.used_percent === "number") {
        if (
          entry.primaryUsedPct === null ||
          pWin.used_percent > entry.primaryUsedPct
        ) {
          entry.primaryUsedPct = pWin.used_percent;
          entry.primaryResetsAtMs = pWin.resets_at_ms ?? entry.primaryResetsAtMs;
        }
      }
      const sWin = sess.quota_usage?.secondary_window;
      if (sWin && typeof sWin.used_percent === "number") {
        if (
          entry.secondaryUsedPct === null ||
          sWin.used_percent > entry.secondaryUsedPct
        ) {
          entry.secondaryUsedPct = sWin.used_percent;
          entry.secondaryResetsAtMs =
            sWin.resets_at_ms ?? entry.secondaryResetsAtMs;
        }
      }
      if (sess.quota_usage?.exhausted_reason && !entry.exhaustedReason) {
        entry.exhaustedReason = sess.quota_usage.exhausted_reason;
      }
      const persona = (sess.persona ?? "").trim().toLowerCase();
      if (persona && !entry.activePersonas.includes(persona)) {
        entry.activePersonas.push(persona);
      }
      const model = (sess.model ?? "").trim();
      if (model && !entry.models.includes(model)) {
        entry.models.push(model);
      }
    }

    for (const ag of summary?.health?.agents ?? []) {
      const vKey = ag.hosted_session?.vendor || ag.adapter;
      if (vKey) {
        const entry = ensureVendor(vKey);
        const persona = ag.agent.trim().toLowerCase();
        if (ag.active && persona && !entry.activePersonas.includes(persona)) {
          entry.activePersonas.push(persona);
        }
        const model = (ag.hosted_session?.model ?? "").trim();
        if (model && !entry.models.includes(model)) {
          entry.models.push(model);
        }
      }
    }

    return Array.from(map.values());
  }, [summary, sessions]);

  // Candidate targets for Re-Model dropdown
  const remodelCandidates = useMemo(() => {
    const seen = new Set<string>();
    const out: Array<{ key: string; label: string; vendor: string | null }> = [];
    for (const ag of summary?.health?.agents ?? []) {
      const key = ag.agent.trim().toLowerCase();
      if (key && !seen.has(key)) {
        seen.add(key);
        const vendor = ag.hosted_session?.vendor ?? ag.adapter ?? null;
        out.push({
          key,
          label: `${ag.agent}${vendor ? ` (${vendor})` : ""}`,
          vendor,
        });
      }
    }
    for (const sess of sessions) {
      const key = (sess.persona || sess.id).trim().toLowerCase();
      if (key && !seen.has(key)) {
        seen.add(key);
        out.push({
          key,
          label: `${sess.persona || sess.id} (${sess.adapter})`,
          vendor: sess.adapter,
        });
      }
    }
    for (const ag of agents) {
      const key = ag.name_key.trim().toLowerCase();
      if (key && !seen.has(key)) {
        seen.add(key);
        out.push({
          key,
          label: `${ag.display_name} (${ag.project})`,
          vendor: null,
        });
      }
    }
    return out;
  }, [summary, sessions, agents]);

  useEffect(() => {
    if (!remodelTarget && remodelCandidates.length > 0) {
      setRemodelTarget(remodelCandidates[0].key);
    }
  }, [remodelTarget, remodelCandidates]);

  // Best least-loaded vendor suggestion
  const pickLeastLoadedVendor = useCallback(
    (excludeVendor?: string): string => {
      const candidates = vendorMatrix.filter(
        (v) =>
          v.vendor !== (excludeVendor ?? "").toLowerCase() &&
          !v.exhaustedReason,
      );
      if (candidates.length === 0) {
        return excludeVendor === "codex" ? "claude" : "codex";
      }
      const sorted = [...candidates].sort((a, b) => {
        const aScore = Math.max(a.primaryUsedPct ?? 0, a.secondaryUsedPct ?? 0);
        const bScore = Math.max(b.primaryUsedPct ?? 0, b.secondaryUsedPct ?? 0);
        return aScore - bScore;
      });
      return sorted[0].vendor;
    },
    [vendorMatrix],
  );

  const handleApplyThresholds = (e: React.FormEvent) => {
    e.preventDefault();
    setThresholdError(null);
    const warnQuota = Number(warnQuotaInput);
    const critQuota = Number(critQuotaInput);
    const warnUsd = Number(warnCostUsdInput);
    const critUsd = Number(critCostUsdInput);
    const staleMs = Number(staleMsInput);

    if (
      !Number.isFinite(warnQuota) ||
      warnQuota < 0 ||
      warnQuota > 100 ||
      !Number.isFinite(critQuota) ||
      critQuota < 0 ||
      critQuota > 100
    ) {
      setThresholdError("Quota thresholds must be numbers between 0 and 100.");
      return;
    }
    if (warnQuota > critQuota) {
      setThresholdError(
        `Warning quota (${warnQuota}%) cannot exceed critical quota (${critQuota}%).`,
      );
      return;
    }
    if (!Number.isFinite(warnUsd) || warnUsd < 0 || !Number.isFinite(critUsd) || critUsd < 0) {
      setThresholdError("Spend thresholds must be non-negative USD amounts.");
      return;
    }
    if (warnUsd > critUsd) {
      setThresholdError(
        `Warning spend ($${warnUsd}) cannot exceed critical spend ($${critUsd}).`,
      );
      return;
    }
    if (!Number.isFinite(staleMs) || staleMs < 1 || !Number.isInteger(staleMs)) {
      setThresholdError("Stale heartbeat threshold must be a whole number >= 1 ms.");
      return;
    }

    const nextOverrides: WatchThresholdOverrides = {
      warn_quota_pct: warnQuota,
      critical_quota_pct: critQuota,
      warn_cost_usd_micros: Math.round(warnUsd * 1_000_000),
      critical_cost_usd_micros: Math.round(critUsd * 1_000_000),
      stale_after_ms: staleMs,
    };
    setAppliedOverrides(nextOverrides);
  };

  const handleResetThresholds = () => {
    setThresholdError(null);
    setWarnQuotaInput(String(DEFAULT_WATCH_THRESHOLDS.warn_quota_pct));
    setCritQuotaInput(String(DEFAULT_WATCH_THRESHOLDS.critical_quota_pct));
    setWarnCostUsdInput(
      String(DEFAULT_WATCH_THRESHOLDS.warn_cost_usd_micros / 1_000_000),
    );
    setCritCostUsdInput(
      String(DEFAULT_WATCH_THRESHOLDS.critical_cost_usd_micros / 1_000_000),
    );
    setStaleMsInput(String(DEFAULT_WATCH_THRESHOLDS.stale_after_ms));
    setAppliedOverrides(undefined);
  };

  const handleRemodelSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!apiClient || !remodelTarget.trim()) return;
    setRemodeling(true);
    setRemodelStatus(null);
    setRemodelError(null);
    try {
      const res = await apiClient.remodelSession(remodelTarget.trim(), {
        vendor: remodelVendor,
        model: remodelModel.trim() || undefined,
        mode: remodelMode,
      });
      setRemodelStatus(
        res.mode === "prepare"
          ? `Prepared handoff for ${res.persona} → ${res.vendor} (${res.session_id}).`
          : `Remodeled ${res.persona}: ${res.from_vendor} → ${res.vendor}${res.model ? ` (${res.model})` : ""} [${res.session_id}].`,
      );
      onAfterRemodel?.(res);
      await loadSummary(appliedOverrides);
    } catch (err) {
      setRemodelError(
        err instanceof Error ? err.message : "Failed to remodel agent session.",
      );
    } finally {
      setRemodeling(false);
    }
  };

  const handleSaveBalancePolicy = async () => {
    if (!apiClient) return;
    setBalanceBusy(true);
    setBalanceMessage(null);
    setBalanceError(null);
    try {
      const saved = await apiClient.updateWatchBalancePolicy({
        enabled: balancePolicy.enabled,
        primary_trigger_pct: balancePolicy.primary_trigger_pct,
        secondary_trigger_pct: balancePolicy.secondary_trigger_pct,
        target_max_primary_pct: balancePolicy.target_max_primary_pct,
        target_max_secondary_pct: balancePolicy.target_max_secondary_pct,
      });
      setBalancePolicy(saved);
      setBalanceMessage(
        `Saved quota balance policy (Auto-balance: ${saved.enabled ? "Enabled" : "Disabled"}).`,
      );
    } catch (err) {
      setBalanceError(
        err instanceof Error ? err.message : "Failed to save balance policy.",
      );
    } finally {
      setBalanceBusy(false);
    }
  };

  const handlePreviewBalancePlan = async () => {
    if (!apiClient) return;
    setBalanceBusy(true);
    setBalanceMessage(null);
    setBalanceError(null);
    try {
      const plan = await apiClient.planWatchBalance({
        primary_trigger_pct: balancePolicy.primary_trigger_pct,
        secondary_trigger_pct: balancePolicy.secondary_trigger_pct,
        target_max_primary_pct: balancePolicy.target_max_primary_pct,
        target_max_secondary_pct: balancePolicy.target_max_secondary_pct,
      });
      setBalancePlan(plan);
      setBalanceMessage(
        plan.switches.length > 0
          ? `Dry-run plan ready: ${plan.switches.length} vendor switch(es) proposed.`
          : plan.blocked
            ? `Balance plan blocked: ${plan.blocked}`
            : "Dry-run plan evaluated: all sessions within target quota thresholds.",
      );
    } catch (err) {
      setBalanceError(
        err instanceof Error ? err.message : "Failed to evaluate balance plan.",
      );
    } finally {
      setBalanceBusy(false);
    }
  };

  const handleApplyBalance = async () => {
    if (!apiClient) return;
    setBalanceBusy(true);
    setBalanceMessage(null);
    setBalanceError(null);
    try {
      const res = await apiClient.applyWatchBalance();
      setBalanceApplyRes(res);
      setBalancePlan(res.plan);
      const okCount = res.results.filter((r) => r.ok).length;
      setBalanceMessage(
        `Applied quota balance: ${okCount}/${res.results.length} switch(es) completed.`,
      );
      await loadSummary(appliedOverrides);
    } catch (err) {
      setBalanceError(
        err instanceof Error ? err.message : "Failed to apply quota balance.",
      );
    } finally {
      setBalanceBusy(false);
    }
  };

  const activeTelemetry: WatchTelemetryReport | null =
    scopedRollups ?? summary?.telemetry ?? null;
  const alerts: WatchAlert[] = summary?.alerts ?? [];
  const healthAgents: WatchAgentHealth[] = summary?.health?.agents ?? [];

  // Map adapters to discovered models for Vendor + Model breakdown
  const adapterModelsMap = useMemo(() => {
    const map = new Map<string, string[]>();
    for (const v of vendorMatrix) {
      if (v.models.length > 0) {
        map.set(v.vendor.toLowerCase(), v.models);
      }
    }
    return map;
  }, [vendorMatrix]);

  const showAlerts = sectionFilter === "all" || sectionFilter === "alerts";
  const showQuotas = sectionFilter === "all" || sectionFilter === "quotas";
  const showLiveness = sectionFilter === "all" || sectionFilter === "liveness";
  const showRollups = sectionFilter === "all" || sectionFilter === "rollups";

  return (
    <section
      className="fd-pane"
      data-testid="ferrowatch-pane"
      aria-label="FerroWatch Observability & Quota Balancing Console"
      style={{
        display: "flex",
        flexDirection: "column",
        height: "100%",
        minHeight: 0,
        overflow: "hidden",
      }}
    >
      {/* Header & Top KPI Strip */}
      <div
        style={{
          padding: "10px 12px",
          borderBottom: "1px solid var(--border-subtle)",
          backgroundColor: "var(--bg-surface)",
          display: "flex",
          flexDirection: "column",
          gap: "8px",
          flexShrink: 0,
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: "8px",
          }}
        >
          <div>
            <div
              style={{
                fontSize: "13px",
                fontWeight: 700,
                color: "var(--text-primary)",
                display: "flex",
                alignItems: "center",
                gap: "6px",
              }}
            >
              <span>FerroWatch</span>
              <span
                className="fd-badge"
                style={{ fontSize: "10px", fontWeight: 600 }}
              >
                {summary?.health?.local_host || "ai-lab-0"}
              </span>
            </div>
            <div style={{ fontSize: "11px", color: "var(--text-muted)" }}>
              Live Alerts, Subscription Quotas, Liveness &amp; Spend Rollups
            </div>
          </div>

          <button
            type="button"
            className="fd-btn"
            data-testid="watch-refresh-btn"
            disabled={loading}
            onClick={() => void loadSummary(appliedOverrides)}
            style={{ fontSize: "11px", padding: "4px 10px" }}
          >
            {loading ? "Refreshing..." : "Refresh"}
          </button>
        </div>

        {/* Summary KPI Strip */}
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "repeat(4, minmax(0, 1fr))",
            gap: "6px",
          }}
        >
          <div
            data-testid="watch-kpi-alerts"
            style={{
              padding: "6px 8px",
              borderRadius: "6px",
              backgroundColor: "var(--bg-elevated)",
              border: "1px solid var(--border-subtle)",
            }}
          >
            <div style={{ fontSize: "10px", color: "var(--text-muted)" }}>
              Alerts
            </div>
            <div
              style={{
                fontSize: "13px",
                fontWeight: 700,
                color:
                  alerts.some((a) => a.severity === "critical")
                    ? "#F87171"
                    : alerts.length > 0
                      ? "#FBBF24"
                      : "#34D399",
              }}
            >
              {alerts.length}
            </div>
          </div>

          <div
            data-testid="watch-kpi-liveness"
            style={{
              padding: "6px 8px",
              borderRadius: "6px",
              backgroundColor: "var(--bg-elevated)",
              border: "1px solid var(--border-subtle)",
            }}
          >
            <div style={{ fontSize: "10px", color: "var(--text-muted)" }}>
              Live / Stale
            </div>
            <div style={{ fontSize: "13px", fontWeight: 700 }}>
              <span style={{ color: "#34D399" }}>
                {summary?.health?.totals?.live ?? 0}
              </span>
              {" / "}
              <span
                style={{
                  color:
                    (summary?.health?.totals?.stale ?? 0) +
                      (summary?.health?.totals?.dead ?? 0) >
                    0
                      ? "#FBBF24"
                      : "var(--text-secondary)",
                }}
              >
                {(summary?.health?.totals?.stale ?? 0) +
                  (summary?.health?.totals?.dead ?? 0)}
              </span>
            </div>
          </div>

          <div
            data-testid="watch-kpi-spend"
            style={{
              padding: "6px 8px",
              borderRadius: "6px",
              backgroundColor: "var(--bg-elevated)",
              border: "1px solid var(--border-subtle)",
            }}
          >
            <div style={{ fontSize: "10px", color: "var(--text-muted)" }}>
              Spend
            </div>
            <div
              className="fd-mono"
              style={{ fontSize: "13px", fontWeight: 700 }}
            >
              {formatUsdFromMicros(summary?.telemetry?.totals?.cost_usd_micros)}
            </div>
          </div>

          <div
            data-testid="watch-kpi-attempts"
            style={{
              padding: "6px 8px",
              borderRadius: "6px",
              backgroundColor: "var(--bg-elevated)",
              border: "1px solid var(--border-subtle)",
            }}
          >
            <div style={{ fontSize: "10px", color: "var(--text-muted)" }}>
              Attempts
            </div>
            <div
              className="fd-mono"
              style={{ fontSize: "13px", fontWeight: 700 }}
            >
              {formatCompactNumber(summary?.telemetry?.totals?.attempts)}
            </div>
          </div>
        </div>

        {/* Section Filter Bar */}
        <div
          role="tablist"
          aria-label="FerroWatch Sections"
          style={{ display: "flex", gap: "4px", flexWrap: "wrap" }}
        >
          {(
            [
              { id: "all", label: "All Views" },
              { id: "alerts", label: `Alerts (${alerts.length})` },
              { id: "quotas", label: "Quotas & Balance" },
              { id: "liveness", label: `Liveness (${healthAgents.length})` },
              { id: "rollups", label: "Spend & Rollups" },
            ] as Array<{ id: WatchSectionFilter; label: string }>
          ).map((item) => (
            <button
              key={item.id}
              type="button"
              role="tab"
              aria-selected={sectionFilter === item.id}
              data-testid={`watch-section-filter-${item.id}`}
              className={`fd-btn${sectionFilter === item.id ? " fd-btn-active" : ""}`}
              style={{ fontSize: "11px", padding: "3px 8px" }}
              onClick={() => setSectionFilter(item.id)}
            >
              {item.label}
            </button>
          ))}
        </div>
      </div>

      {/* Main Scrollable Console Body */}
      <div
        className="fd-scroll"
        style={{
          flex: 1,
          minHeight: 0,
          padding: "10px 12px",
          display: "flex",
          flexDirection: "column",
          gap: "14px",
        }}
      >
        {error && (
          <div
            role="alert"
            data-testid="watch-error-banner"
            style={{
              padding: "8px 10px",
              borderRadius: "6px",
              backgroundColor: "rgba(239, 68, 68, 0.14)",
              border: "1px solid rgba(239, 68, 68, 0.4)",
              color: "#F87171",
              fontSize: "12px",
            }}
          >
            {error}
          </div>
        )}

        {/* 1. Live Alerts Banner / List */}
        {showAlerts && (
          <div
            data-testid="watch-alerts-section"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "8px",
            }}
          >
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                fontSize: "11px",
                fontWeight: 700,
                textTransform: "uppercase",
                letterSpacing: "0.05em",
                color: "var(--text-muted)",
              }}
            >
              <span>Live Alerts ({alerts.length})</span>
              <span>
                Warn {activeThresholds.warn_quota_pct}% / Crit{" "}
                {activeThresholds.critical_quota_pct}%
              </span>
            </div>

            {alerts.length === 0 ? (
              <div
                data-testid="watch-alerts-empty"
                style={{
                  padding: "10px 12px",
                  borderRadius: "6px",
                  backgroundColor: "rgba(16, 185, 129, 0.1)",
                  border: "1px solid rgba(16, 185, 129, 0.3)",
                  color: "#34D399",
                  fontSize: "12px",
                }}
              >
                All clear — no active spend, quota, liveness, or crash-loop
                alerts.
              </div>
            ) : (
              alerts.map((alert) => (
                <div
                  key={alert.id}
                  data-testid={`watch-alert-item-${alert.id}`}
                  data-severity={alert.severity}
                  style={{
                    padding: "9px 10px",
                    borderRadius: "6px",
                    backgroundColor: "var(--bg-elevated)",
                    border: "1px solid var(--border-subtle)",
                    display: "flex",
                    flexDirection: "column",
                    gap: "4px",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: "6px",
                      flexWrap: "wrap",
                    }}
                  >
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: "6px",
                        flexWrap: "wrap",
                      }}
                    >
                      <span
                        className="fd-badge"
                        data-testid={`watch-alert-severity-${alert.id}`}
                        style={getSeverityBadgeStyle(alert.severity)}
                      >
                        {alert.severity}
                      </span>
                      <span className="fd-badge fd-mono">{alert.rule}</span>
                      <span
                        className="fd-mono"
                        style={{
                          fontSize: "11px",
                          fontWeight: 600,
                          color: "var(--text-primary)",
                        }}
                      >
                        {alert.entity.label || alert.entity.id}
                      </span>
                    </div>
                    {alert.resets_at_ms && (
                      <span
                        className="fd-mono"
                        style={{ fontSize: "10px", color: "var(--text-muted)" }}
                      >
                        {formatResetCountdown(alert.resets_at_ms, nowMs)}
                      </span>
                    )}
                  </div>

                  <div
                    style={{
                      fontSize: "12px",
                      color: "var(--text-primary)",
                      lineHeight: 1.4,
                    }}
                  >
                    {alert.message}
                  </div>

                  <div
                    style={{
                      fontSize: "11px",
                      color: "var(--text-muted)",
                      lineHeight: 1.35,
                    }}
                  >
                    Remediation: {alert.remediation}
                  </div>
                </div>
              ))
            )}
          </div>
        )}

        {/* 2. Subscription Quota & Usage Matrix + Balancing Controls */}
        {showQuotas && (
          <div
            data-testid="watch-quota-matrix-section"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "10px",
            }}
          >
            <div
              style={{
                fontSize: "11px",
                fontWeight: 700,
                textTransform: "uppercase",
                letterSpacing: "0.05em",
                color: "var(--text-muted)",
              }}
            >
              Subscription Quota &amp; Usage Matrix
            </div>

            <div
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "8px",
              }}
            >
              {vendorMatrix.map((item) => {
                const pPct =
                  item.primaryUsedPct !== null
                    ? Math.max(0, Math.min(100, item.primaryUsedPct))
                    : 0;
                const sPct =
                  item.secondaryUsedPct !== null
                    ? Math.max(0, Math.min(100, item.secondaryUsedPct))
                    : 0;
                const isExhausted = Boolean(item.exhaustedReason);

                return (
                  <div
                    key={item.vendor}
                    data-testid={`watch-vendor-card-${item.vendor}`}
                    style={{
                      padding: "10px",
                      borderRadius: "6px",
                      backgroundColor: "var(--bg-elevated)",
                      border: isExhausted
                        ? "1px solid rgba(239, 68, 68, 0.45)"
                        : "1px solid var(--border-subtle)",
                      display: "flex",
                      flexDirection: "column",
                      gap: "6px",
                    }}
                  >
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                        gap: "6px",
                        flexWrap: "wrap",
                      }}
                    >
                      <div
                        style={{
                          display: "flex",
                          alignItems: "center",
                          gap: "6px",
                          flexWrap: "wrap",
                        }}
                      >
                        <span className={getAdapterBadgeClass(item.vendor)}>
                          {item.vendor}
                        </span>
                        <span
                          className="fd-badge"
                          data-testid={`watch-vendor-tier-${item.vendor}`}
                          style={{ fontSize: "10px" }}
                        >
                          {item.tier}
                        </span>
                        {isExhausted && (
                          <span
                            className="fd-badge"
                            data-testid={`watch-vendor-exhausted-${item.vendor}`}
                            style={{
                              backgroundColor: "rgba(239, 68, 68, 0.16)",
                              color: "#F87171",
                              borderColor: "rgba(239, 68, 68, 0.45)",
                              fontSize: "10px",
                            }}
                            title={item.exhaustedReason ?? undefined}
                          >
                            Exhausted
                          </span>
                        )}
                      </div>

                      <button
                        type="button"
                        className="fd-btn"
                        data-testid={`watch-vendor-remodel-btn-${item.vendor}`}
                        style={{ fontSize: "10px", padding: "2px 7px" }}
                        onClick={() => {
                          if (item.activePersonas.length > 0) {
                            setRemodelTarget(item.activePersonas[0]);
                          }
                          setRemodelVendor(pickLeastLoadedVendor(item.vendor));
                        }}
                      >
                        Re-Model / Balance
                      </button>
                    </div>

                    {/* 5h Session (Primary) Window */}
                    <div style={{ display: "flex", flexDirection: "column", gap: "3px" }}>
                      <div
                        style={{
                          display: "flex",
                          justifyContent: "space-between",
                          fontSize: "11px",
                          color: "var(--text-secondary)",
                        }}
                      >
                        <span>5h Session Window</span>
                        <span className="fd-mono">
                          <span data-testid={`watch-quota-primary-pct-${item.vendor}`}>
                            {item.primaryUsedPct !== null
                              ? `${item.primaryUsedPct.toFixed(0)}%`
                              : "—"}
                          </span>{" "}
                          ·{" "}
                          <span data-testid={`watch-quota-primary-reset-${item.vendor}`}>
                            {formatResetCountdown(item.primaryResetsAtMs, nowMs)}
                          </span>
                        </span>
                      </div>
                      <div
                        style={{
                          height: "6px",
                          borderRadius: "3px",
                          backgroundColor: "var(--bg-surface)",
                          overflow: "hidden",
                        }}
                      >
                        <div
                          data-testid={`watch-quota-primary-bar-${item.vendor}`}
                          style={{
                            width: `${pPct}%`,
                            height: "100%",
                            backgroundColor: getQuotaBarColor(
                              pPct,
                              activeThresholds.warn_quota_pct,
                              activeThresholds.critical_quota_pct,
                              isExhausted,
                            ),
                          }}
                        />
                      </div>
                    </div>

                    {/* 7d Weekly (Secondary) Window */}
                    <div style={{ display: "flex", flexDirection: "column", gap: "3px" }}>
                      <div
                        style={{
                          display: "flex",
                          justifyContent: "space-between",
                          fontSize: "11px",
                          color: "var(--text-secondary)",
                        }}
                      >
                        <span>7d Weekly Window</span>
                        <span className="fd-mono">
                          <span data-testid={`watch-quota-secondary-pct-${item.vendor}`}>
                            {item.secondaryUsedPct !== null
                              ? `${item.secondaryUsedPct.toFixed(0)}%`
                              : "—"}
                          </span>{" "}
                          ·{" "}
                          <span data-testid={`watch-quota-secondary-reset-${item.vendor}`}>
                            {formatResetCountdown(item.secondaryResetsAtMs, nowMs)}
                          </span>
                        </span>
                      </div>
                      <div
                        style={{
                          height: "6px",
                          borderRadius: "3px",
                          backgroundColor: "var(--bg-surface)",
                          overflow: "hidden",
                        }}
                      >
                        <div
                          data-testid={`watch-quota-secondary-bar-${item.vendor}`}
                          style={{
                            width: `${sPct}%`,
                            height: "100%",
                            backgroundColor: getQuotaBarColor(
                              sPct,
                              activeThresholds.warn_quota_pct,
                              activeThresholds.critical_quota_pct,
                              isExhausted,
                            ),
                          }}
                        />
                      </div>
                    </div>

                    {item.activePersonas.length > 0 && (
                      <div
                        style={{
                          fontSize: "10px",
                          color: "var(--text-muted)",
                          display: "flex",
                          gap: "4px",
                          flexWrap: "wrap",
                        }}
                      >
                        <span>Active sessions:</span>
                        {item.activePersonas.map((p) => (
                          <span key={p} className="fd-mono" style={{ color: "var(--text-secondary)" }}>
                            {p}
                          </span>
                        ))}
                      </div>
                    )}
                  </div>
                );
              })}
            </div>

            {/* Quick Re-Model & Auto-Balance Action Box */}
            <div
              data-testid="watch-quota-balance-controls"
              style={{
                padding: "10px",
                borderRadius: "6px",
                backgroundColor: "var(--bg-elevated)",
                border: "1px solid var(--border-subtle)",
                display: "flex",
                flexDirection: "column",
                gap: "10px",
              }}
            >
              <div
                style={{
                  fontSize: "11px",
                  fontWeight: 700,
                  color: "var(--text-primary)",
                }}
              >
                Quick Session Re-Model &amp; Subscription Quota Balancing
              </div>

              <form
                onSubmit={(e) => void handleRemodelSubmit(e)}
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "8px",
                }}
              >
                <div
                  style={{
                    display: "grid",
                    gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
                    gap: "6px",
                  }}
                >
                  <label
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      gap: "3px",
                      fontSize: "11px",
                      color: "var(--text-secondary)",
                    }}
                  >
                    <span>Agent / Session</span>
                    <select
                      className="fd-input"
                      data-testid="watch-remodel-agent-select"
                      value={remodelTarget}
                      onChange={(e) => setRemodelTarget(e.target.value)}
                      style={{ fontSize: "12px", padding: "4px 6px" }}
                    >
                      {remodelCandidates.map((c) => (
                        <option key={c.key} value={c.key}>
                          {c.label}
                        </option>
                      ))}
                    </select>
                  </label>

                  <label
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      gap: "3px",
                      fontSize: "11px",
                      color: "var(--text-secondary)",
                    }}
                  >
                    <span>Target Vendor</span>
                    <select
                      className="fd-input"
                      data-testid="watch-remodel-vendor-select"
                      value={remodelVendor}
                      onChange={(e) => setRemodelVendor(e.target.value)}
                      style={{ fontSize: "12px", padding: "4px 6px" }}
                    >
                      {CANONICAL_WATCH_VENDORS.map((v) => (
                        <option key={v} value={v}>
                          {v}
                        </option>
                      ))}
                    </select>
                  </label>

                  <label
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      gap: "3px",
                      fontSize: "11px",
                      color: "var(--text-secondary)",
                    }}
                  >
                    <span>Model Override (Optional)</span>
                    <input
                      type="text"
                      className="fd-input"
                      data-testid="watch-remodel-model-input"
                      placeholder="Default vendor model"
                      value={remodelModel}
                      onChange={(e) => setRemodelModel(e.target.value)}
                      style={{ fontSize: "12px", padding: "4px 6px" }}
                    />
                  </label>

                  <label
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      gap: "3px",
                      fontSize: "11px",
                      color: "var(--text-secondary)",
                    }}
                  >
                    <span>Mode</span>
                    <select
                      className="fd-input"
                      data-testid="watch-remodel-mode-select"
                      value={remodelMode}
                      onChange={(e) =>
                        setRemodelMode(e.target.value as RemodelMode)
                      }
                      style={{ fontSize: "12px", padding: "4px 6px" }}
                    >
                      <option value="execute">Execute (Immediate Move)</option>
                      <option value="prepare">Prepare (Handoff First)</option>
                    </select>
                  </label>
                </div>

                <div style={{ display: "flex", gap: "6px", flexWrap: "wrap" }}>
                  <button
                    type="submit"
                    className="fd-btn fd-btn-primary"
                    data-testid="watch-remodel-submit-btn"
                    disabled={remodeling || !remodelTarget.trim()}
                    style={{ fontSize: "11px", padding: "5px 10px" }}
                  >
                    {remodeling ? "Re-Modeling..." : "Re-Model Agent"}
                  </button>
                </div>
              </form>

              {remodelStatus && (
                <div
                  data-testid="watch-remodel-status"
                  style={{
                    fontSize: "11px",
                    color: "#34D399",
                    padding: "6px 8px",
                    borderRadius: "4px",
                    backgroundColor: "rgba(16, 185, 129, 0.12)",
                  }}
                >
                  {remodelStatus}
                </div>
              )}

              {remodelError && (
                <div
                  role="alert"
                  data-testid="watch-remodel-error"
                  style={{
                    fontSize: "11px",
                    color: "#F87171",
                    padding: "6px 8px",
                    borderRadius: "4px",
                    backgroundColor: "rgba(239, 68, 68, 0.14)",
                  }}
                >
                  {remodelError}
                </div>
              )}

              {/* Automated Quota Balancing Policy & Dry-Run / Apply */}
              <div
                style={{
                  borderTop: "1px solid var(--border-subtle)",
                  paddingTop: "8px",
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
                }}
              >
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "6px",
                    flexWrap: "wrap",
                  }}
                >
                  <label
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: "6px",
                      fontSize: "11px",
                      color: "var(--text-secondary)",
                      cursor: "pointer",
                    }}
                  >
                    <input
                      type="checkbox"
                      data-testid="watch-balance-enabled-checkbox"
                      checked={balancePolicy.enabled}
                      disabled={!isOwner}
                      onChange={(e) =>
                        setBalancePolicy((prev) => ({
                          ...prev,
                          enabled: e.target.checked,
                        }))
                      }
                    />
                    <span>Auto-Balance Subscriptions</span>
                  </label>

                  <div style={{ display: "flex", gap: "6px", flexWrap: "wrap" }}>
                    {isOwner && (
                      <button
                        type="button"
                        className="fd-btn"
                        data-testid="watch-balance-save-policy-btn"
                        disabled={balanceBusy}
                        onClick={() => void handleSaveBalancePolicy()}
                        style={{ fontSize: "11px", padding: "3px 8px" }}
                      >
                        Save Policy
                      </button>
                    )}
                    <button
                      type="button"
                      className="fd-btn"
                      data-testid="watch-balance-plan-btn"
                      disabled={balanceBusy}
                      onClick={() => void handlePreviewBalancePlan()}
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                    >
                      Preview Balance Plan
                    </button>
                    <button
                      type="button"
                      className="fd-btn fd-btn-primary"
                      data-testid="watch-balance-apply-btn"
                      disabled={balanceBusy}
                      onClick={() => void handleApplyBalance()}
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                    >
                      Balance Now
                    </button>
                  </div>
                </div>

                {balanceMessage && (
                  <div
                    data-testid="watch-balance-status"
                    style={{ fontSize: "11px", color: "#34D399" }}
                  >
                    {balanceMessage}
                  </div>
                )}
                {balanceError && (
                  <div
                    role="alert"
                    data-testid="watch-balance-error"
                    style={{ fontSize: "11px", color: "#F87171" }}
                  >
                    {balanceError}
                  </div>
                )}

                {balancePlan && (
                  <div
                    data-testid="watch-balance-plan-results"
                    style={{
                      fontSize: "11px",
                      color: "var(--text-secondary)",
                      display: "flex",
                      flexDirection: "column",
                      gap: "4px",
                    }}
                  >
                    {balancePlan.switches.map((sw) => (
                      <div
                        key={`${sw.persona}-${sw.to_vendor}`}
                        className="fd-mono"
                        style={{ fontSize: "11px" }}
                      >
                        {sw.persona}: {sw.from_vendor} → {sw.to_vendor} (
                        {sw.reason})
                      </div>
                    ))}
                  </div>
                )}

                {balanceApplyRes && balanceApplyRes.results.length > 0 && (
                  <div
                    data-testid="watch-balance-apply-results"
                    style={{
                      fontSize: "11px",
                      color: "var(--text-secondary)",
                      display: "flex",
                      flexDirection: "column",
                      gap: "4px",
                    }}
                  >
                    {balanceApplyRes.results.map((r) => (
                      <div key={r.persona} className="fd-mono">
                        {r.persona}: {r.from_vendor} → {r.to_vendor} [
                        {r.ok ? "OK" : r.error || "failed"}]
                      </div>
                    ))}
                  </div>
                )}
              </div>
            </div>
          </div>
        )}

        {/* 3. Agent & Peer Liveness Table */}
        {showLiveness && (
          <div
            data-testid="watch-liveness-section"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "8px",
            }}
          >
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                fontSize: "11px",
                fontWeight: 700,
                textTransform: "uppercase",
                letterSpacing: "0.05em",
                color: "var(--text-muted)",
              }}
            >
              <span>Agent &amp; Peer Liveness ({healthAgents.length})</span>
              <span>
                Hosted: {summary?.health?.hosted_sessions ?? 0} · Stale &gt;{" "}
                {formatDurationMs(activeThresholds.stale_after_ms)}
              </span>
            </div>

            {healthAgents.length === 0 ? (
              <div
                data-testid="watch-liveness-empty"
                style={{
                  padding: "10px",
                  borderRadius: "6px",
                  backgroundColor: "var(--bg-elevated)",
                  color: "var(--text-muted)",
                  fontSize: "12px",
                }}
              >
                No agent heartbeats or hosted sessions recorded yet.
              </div>
            ) : (
              <div
                data-testid="watch-liveness-table"
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
                }}
              >
                {healthAgents.map((item) => {
                  const key = item.agent.trim().toLowerCase();
                  const hosted = item.hosted_session;
                  const vendor = hosted?.vendor ?? item.adapter ?? null;

                  return (
                    <div
                      key={item.agent}
                      data-testid={`watch-liveness-row-${key}`}
                      style={{
                        padding: "8px 10px",
                        borderRadius: "6px",
                        backgroundColor: "var(--bg-elevated)",
                        border: "1px solid var(--border-subtle)",
                        display: "flex",
                        flexDirection: "column",
                        gap: "4px",
                      }}
                    >
                      <div
                        style={{
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "space-between",
                          gap: "6px",
                          flexWrap: "wrap",
                        }}
                      >
                        <div
                          style={{
                            display: "flex",
                            alignItems: "center",
                            gap: "6px",
                            flexWrap: "wrap",
                          }}
                        >
                          <span
                            style={{
                              fontWeight: 700,
                              fontSize: "12px",
                              color: "var(--text-primary)",
                            }}
                          >
                            {item.agent}
                          </span>
                          <span
                            className="fd-badge"
                            data-testid={`watch-liveness-badge-${key}`}
                            data-liveness={item.liveness}
                            style={getLivenessBadgeStyle(item.liveness)}
                          >
                            {item.liveness}
                          </span>
                          <span
                            className="fd-badge"
                            data-testid={`watch-liveness-active-${key}`}
                            style={{ fontSize: "10px" }}
                          >
                            {item.active ? "Active" : "Inactive"}
                          </span>
                          {vendor && (
                            <span className={getAdapterBadgeClass(vendor)}>
                              {vendor}
                            </span>
                          )}
                        </div>

                        <button
                          type="button"
                          className="fd-btn"
                          data-testid={`watch-liveness-remodel-btn-${key}`}
                          style={{ fontSize: "10px", padding: "2px 6px" }}
                          onClick={() => {
                            setRemodelTarget(key);
                            if (vendor) {
                              setRemodelVendor(pickLeastLoadedVendor(vendor));
                            }
                            if (sectionFilter === "liveness") {
                              setSectionFilter("all");
                            }
                          }}
                        >
                          Re-Model
                        </button>
                      </div>

                      <div
                        style={{
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "space-between",
                          gap: "8px",
                          flexWrap: "wrap",
                          fontSize: "11px",
                          color: "var(--text-muted)",
                        }}
                      >
                        <span>
                          Project: {item.project || "workspace"} · Node:{" "}
                          {item.node || "local"}
                        </span>
                        <span className="fd-mono">
                          Heartbeat age:{" "}
                          {formatDurationMs(item.heartbeat_age_ms)}
                          {item.heartbeat_at_ms
                            ? ` (${formatTimestamp(new Date(item.heartbeat_at_ms).toISOString(), activeTimeZone)})`
                            : ""}
                        </span>
                      </div>

                      {hosted && (
                        <div
                          data-testid={`watch-liveness-hosted-${key}`}
                          className="fd-mono"
                          style={{
                            fontSize: "10px",
                            color: "var(--text-secondary)",
                          }}
                        >
                          Hosted session: {hosted.session_id} · status=
                          {hosted.status}
                          {hosted.model ? ` · model=${hosted.model}` : ""} ·
                          restarts={hosted.restart_count}
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        )}

        {/* 4. Spend & Token Rollups + Interactive Threshold Controls */}
        {showRollups && (
          <div
            data-testid="watch-rollups-section"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "10px",
            }}
          >
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "8px",
                flexWrap: "wrap",
              }}
            >
              <span
                style={{
                  fontSize: "11px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.05em",
                  color: "var(--text-muted)",
                }}
              >
                Spend &amp; Token Rollups
              </span>

              <select
                className="fd-input"
                aria-label="Filter rollups by project"
                data-testid="watch-rollups-project-select"
                value={selectedProjectSlug}
                onChange={(e) => setSelectedProjectSlug(e.target.value)}
                style={{ fontSize: "11px", padding: "3px 6px" }}
              >
                <option value="">All Projects (Workspace)</option>
                {(summary?.telemetry?.by_project ?? []).map((proj) => (
                  <option key={proj.project} value={proj.slug}>
                    {proj.slug} ({formatUsdFromMicros(proj.cost_usd_micros)})
                  </option>
                ))}
              </select>
            </div>

            {/* Totals Strip */}
            {activeTelemetry && (
              <div
                data-testid="watch-rollups-totals"
                style={{
                  padding: "8px 10px",
                  borderRadius: "6px",
                  backgroundColor: "var(--bg-elevated)",
                  border: "1px solid var(--border-subtle)",
                  display: "flex",
                  flexWrap: "wrap",
                  gap: "10px",
                  fontSize: "11px",
                  color: "var(--text-secondary)",
                }}
              >
                <span>
                  Spend:{" "}
                  <strong className="fd-mono">
                    {formatUsdFromMicros(activeTelemetry.totals.cost_usd_micros)}
                  </strong>
                </span>
                <span>
                  Attempts:{" "}
                  <strong className="fd-mono">
                    {activeTelemetry.totals.attempts}
                  </strong>{" "}
                  (ok {activeTelemetry.totals.succeeded} / fail{" "}
                  {activeTelemetry.totals.failed} / crash{" "}
                  {activeTelemetry.totals.crashed})
                </span>
                <span>
                  Tokens In/Out:{" "}
                  <strong className="fd-mono">
                    {formatCompactNumber(activeTelemetry.totals.tokens_in)} /{" "}
                    {formatCompactNumber(activeTelemetry.totals.tokens_out)}
                  </strong>
                </span>
                <span>
                  Gates:{" "}
                  <strong className="fd-mono">
                    {activeTelemetry.gate_runs.pass} pass /{" "}
                    {activeTelemetry.gate_runs.fail} fail
                  </strong>
                </span>
              </div>
            )}

            {/* By Project */}
            <div
              data-testid="watch-rollups-by-project"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "4px",
              }}
            >
              <div
                style={{
                  fontSize: "11px",
                  fontWeight: 600,
                  color: "var(--text-secondary)",
                }}
              >
                By Project
              </div>
              {(activeTelemetry?.by_project ?? []).map((proj) => (
                <div
                  key={proj.project}
                  data-testid={`watch-rollup-project-${proj.slug}`}
                  style={{
                    padding: "6px 8px",
                    borderRadius: "4px",
                    backgroundColor: "var(--bg-elevated)",
                    border: "1px solid var(--border-subtle)",
                    display: "flex",
                    justifyContent: "space-between",
                    gap: "8px",
                    fontSize: "11px",
                  }}
                >
                  <span style={{ fontWeight: 600 }}>{proj.slug}</span>
                  <span className="fd-mono">
                    {proj.attempts} attempts ·{" "}
                    {formatCompactNumber(proj.tokens_in + proj.tokens_out)} tok ·{" "}
                    {formatUsdFromMicros(proj.cost_usd_micros)}
                  </span>
                </div>
              ))}
            </div>

            {/* By Vendor / Adapter & Model */}
            <div
              data-testid="watch-rollups-by-adapter"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "4px",
              }}
            >
              <div
                style={{
                  fontSize: "11px",
                  fontWeight: 600,
                  color: "var(--text-secondary)",
                }}
              >
                By Vendor &amp; Model
              </div>
              {(activeTelemetry?.by_adapter ?? []).map((item) => {
                const models =
                  adapterModelsMap.get(item.key.toLowerCase()) ?? [];
                return (
                  <div
                    key={item.key}
                    data-testid={`watch-rollup-adapter-${item.key}`}
                    style={{
                      padding: "6px 8px",
                      borderRadius: "4px",
                      backgroundColor: "var(--bg-elevated)",
                      border: "1px solid var(--border-subtle)",
                      display: "flex",
                      justifyContent: "space-between",
                      gap: "8px",
                      fontSize: "11px",
                    }}
                  >
                    <span>
                      <strong>{item.key}</strong>
                      {models.length > 0 ? ` (${models.join(", ")})` : ""}
                    </span>
                    <span className="fd-mono">
                      {item.attempts} attempts ·{" "}
                      {formatCompactNumber(item.tokens_in + item.tokens_out)} tok ·{" "}
                      {formatUsdFromMicros(item.cost_usd_micros)}
                    </span>
                  </div>
                );
              })}
            </div>

            {/* By Agent */}
            <div
              data-testid="watch-rollups-by-agent"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "4px",
              }}
            >
              <div
                style={{
                  fontSize: "11px",
                  fontWeight: 600,
                  color: "var(--text-secondary)",
                }}
              >
                By Agent
              </div>
              {(activeTelemetry?.by_agent ?? []).map((item) => (
                <div
                  key={item.key}
                  data-testid={`watch-rollup-agent-${item.key}`}
                  style={{
                    padding: "6px 8px",
                    borderRadius: "4px",
                    backgroundColor: "var(--bg-elevated)",
                    border: "1px solid var(--border-subtle)",
                    display: "flex",
                    justifyContent: "space-between",
                    gap: "8px",
                    fontSize: "11px",
                  }}
                >
                  <span style={{ fontWeight: 600 }}>{item.key}</span>
                  <span className="fd-mono">
                    {item.attempts} attempts ·{" "}
                    {formatCompactNumber(item.tokens_in + item.tokens_out)} tok ·{" "}
                    {formatUsdFromMicros(item.cost_usd_micros)}
                  </span>
                </div>
              ))}
            </div>

            {/* Failure Streaks */}
            {(activeTelemetry?.failure_streaks?.length ?? 0) > 0 && (
              <div
                data-testid="watch-failure-streaks"
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "4px",
                }}
              >
                <div
                  style={{
                    fontSize: "11px",
                    fontWeight: 600,
                    color: "#FBBF24",
                  }}
                >
                  Active Failure Streaks
                </div>
                {activeTelemetry!.failure_streaks.map((st) => (
                  <div
                    key={st.record}
                    className="fd-mono"
                    style={{
                      padding: "6px 8px",
                      borderRadius: "4px",
                      backgroundColor: "rgba(245, 158, 11, 0.1)",
                      border: "1px solid rgba(245, 158, 11, 0.3)",
                      fontSize: "11px",
                    }}
                  >
                    {st.slug} · {st.record}: attempt_streak={st.attempt_streak},
                    gate_streak={st.gate_streak}
                    {st.gate_name ? ` (${st.gate_name})` : ""}
                  </div>
                ))}
              </div>
            )}

            {/* Interactive Threshold Controls */}
            <form
              data-testid="watch-thresholds-form"
              onSubmit={handleApplyThresholds}
              style={{
                padding: "10px",
                borderRadius: "6px",
                backgroundColor: "var(--bg-elevated)",
                border: "1px solid var(--border-subtle)",
                display: "flex",
                flexDirection: "column",
                gap: "8px",
              }}
            >
              <div
                style={{
                  fontSize: "11px",
                  fontWeight: 700,
                  color: "var(--text-primary)",
                }}
              >
                Alert Threshold Overrides
              </div>

              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
                  gap: "6px",
                }}
              >
                <label
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "2px",
                    fontSize: "10px",
                    color: "var(--text-secondary)",
                  }}
                >
                  <span>Warn Quota (%)</span>
                  <input
                    type="number"
                    step="any"
                    className="fd-input"
                    data-testid="threshold-warn-quota-input"
                    value={warnQuotaInput}
                    onChange={(e) => setWarnQuotaInput(e.target.value)}
                    style={{ fontSize: "12px", padding: "4px 6px" }}
                  />
                </label>

                <label
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "2px",
                    fontSize: "10px",
                    color: "var(--text-secondary)",
                  }}
                >
                  <span>Critical Quota (%)</span>
                  <input
                    type="number"
                    step="any"
                    className="fd-input"
                    data-testid="threshold-critical-quota-input"
                    value={critQuotaInput}
                    onChange={(e) => setCritQuotaInput(e.target.value)}
                    style={{ fontSize: "12px", padding: "4px 6px" }}
                  />
                </label>

                <label
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "2px",
                    fontSize: "10px",
                    color: "var(--text-secondary)",
                  }}
                >
                  <span>Warn Spend ($ USD)</span>
                  <input
                    type="number"
                    step="any"
                    className="fd-input"
                    data-testid="threshold-warn-cost-usd-input"
                    value={warnCostUsdInput}
                    onChange={(e) => setWarnCostUsdInput(e.target.value)}
                    style={{ fontSize: "12px", padding: "4px 6px" }}
                  />
                </label>

                <label
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "2px",
                    fontSize: "10px",
                    color: "var(--text-secondary)",
                  }}
                >
                  <span>Critical Spend ($ USD)</span>
                  <input
                    type="number"
                    step="any"
                    className="fd-input"
                    data-testid="threshold-critical-cost-usd-input"
                    value={critCostUsdInput}
                    onChange={(e) => setCritCostUsdInput(e.target.value)}
                    style={{ fontSize: "12px", padding: "4px 6px" }}
                  />
                </label>

                <label
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "2px",
                    fontSize: "10px",
                    color: "var(--text-secondary)",
                    gridColumn: "span 2",
                  }}
                >
                  <span>Stale Heartbeat Age (ms)</span>
                  <input
                    type="number"
                    step="1"
                    className="fd-input"
                    data-testid="threshold-stale-ms-input"
                    value={staleMsInput}
                    onChange={(e) => setStaleMsInput(e.target.value)}
                    style={{ fontSize: "12px", padding: "4px 6px" }}
                  />
                </label>
              </div>

              {thresholdError && (
                <div
                  role="alert"
                  data-testid="watch-thresholds-error"
                  style={{ fontSize: "11px", color: "#F87171" }}
                >
                  {thresholdError}
                </div>
              )}

              <div style={{ display: "flex", gap: "6px" }}>
                <button
                  type="submit"
                  className="fd-btn fd-btn-primary"
                  data-testid="watch-apply-thresholds-btn"
                  style={{ fontSize: "11px", padding: "4px 10px" }}
                >
                  Apply Thresholds
                </button>
                <button
                  type="button"
                  className="fd-btn"
                  data-testid="watch-reset-thresholds-btn"
                  onClick={handleResetThresholds}
                  style={{ fontSize: "11px", padding: "4px 10px" }}
                >
                  Reset Defaults
                </button>
              </div>
            </form>
          </div>
        )}
      </div>
    </section>
  );
};
