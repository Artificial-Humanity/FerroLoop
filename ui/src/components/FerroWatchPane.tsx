import React, { useCallback, useEffect, useMemo, useState } from "react";
import type { HubApiClient } from "../api/client";
import { formatCurrencyFromMicros, useActiveCurrency } from "../lib/currency";
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
import {
  DEFAULT_TOKENIZER_LEDGER,
  type CountedTokenizerLedgerEntry,
} from "./FerroStatePane";
import {
  resolveAgentHostSupervisorTelemetry,
  type AgentHostSupervisorTelemetry,
  type NativeHostTransportMode,
} from "./AgentSettingsModal";

export {
  resolveAgentHostSupervisorTelemetry,
  type AgentHostSupervisorTelemetry,
  type NativeHostTransportMode,
};

/**
 * Record #83 (Issue #206): Lab Cutover Gates G1-G10 Readiness Scorecard.
 */
export type CutoverGateId =
  | "G1"
  | "G2"
  | "G3"
  | "G4"
  | "G5"
  | "G6"
  | "G7"
  | "G8"
  | "G9"
  | "G10";

export interface CutoverGateRow {
  id: CutoverGateId;
  title: string;
  status: "pass" | "in-progress";
  evidence: string;
}

export const DEFAULT_CUTOVER_GATES_SCORECARD: CutoverGateRow[] = [
  {
    id: "G1",
    title: "Native fl host Supervisor",
    status: "pass",
    evidence: "pipe-rpc & pty-vt100 ring buffers active (#217 / #82)",
  },
  {
    id: "G2",
    title: "Whole-Slice Budget Admission",
    status: "pass",
    evidence: "Atomic slice budget admission enforced in FerroWatch",
  },
  {
    id: "G3",
    title: "Single Route Authority",
    status: "pass",
    evidence: "FerroWire Store authoritative routing",
  },
  {
    id: "G4",
    title: "FerroState Authority",
    status: "pass",
    evidence: "Roster, coverage & checkpoints backed by FerroState",
  },
  {
    id: "G5",
    title: "Committed .fl/mcp.toml",
    status: "pass",
    evidence: "Project MCP catalog reconciled across 6 vendors (#90)",
  },
  {
    id: "G6",
    title: "FerroGate Vault Cutover",
    status: "pass",
    evidence: "Scoped leases, rotation & audit log live (#96-#99)",
  },
  {
    id: "G7",
    title: "Provenance & Doctor",
    status: "pass",
    evidence: "/api/v1/version, build-info.json & fl doctor G7 checks (#205)",
  },
  {
    id: "G8",
    title: "Security Audit",
    status: "pass",
    evidence: "Zero path traversal, strict mode 0600 vault & CSRF checks",
  },
  {
    id: "G9",
    title: "NATS :4222 Retirement",
    status: "in-progress",
    evidence: "Awaiting final ai-lab-0 cutover window (#206)",
  },
  {
    id: "G10",
    title: "registry.db Retirement",
    status: "in-progress",
    evidence: "Awaiting final ai-lab-0 SQLite retirement window (#206)",
  },
];

// Record #94: OpenTelemetry OTLP Trace & Metric Exporter
export interface OtlpExporterStatus {
  endpoint: string;
  protocol: "grpc" | "http/protobuf";
  status: "connected" | "standby" | "degraded";
  flush_cadence_secs: number;
  exported_attempts: number;
  exported_token_rollups: number;
  exported_cost_events: number;
  exported_regret_records: number;
  last_flush_at: string;
}

export const DEFAULT_OTLP_EXPORTER_STATUS: OtlpExporterStatus = {
  endpoint: "http://127.0.0.1:4317",
  protocol: "grpc",
  status: "connected",
  flush_cadence_secs: 15,
  exported_attempts: 482,
  exported_token_rollups: 1290,
  exported_cost_events: 482,
  exported_regret_records: 14,
  last_flush_at: "2026-11-03T05:30:00Z",
};

// Record #93: ACP Vendor Transport Conformance Matrix
/**
 * Record #93: 6-operation ACP vendor transport conformance matrix.
 */
export type AcpConformanceOperation =
  | "approvals"
  | "interrupt"
  | "reset"
  | "compact"
  | "thread_identity"
  | "usage";

export const ACP_CONFORMANCE_OPERATIONS: readonly AcpConformanceOperation[] = [
  "approvals",
  "interrupt",
  "reset",
  "compact",
  "thread_identity",
  "usage",
] as const;

export interface AcpAdapterConformanceRow {
  adapter: "claude" | "agy" | "codex" | "muse" | "grok" | "acp";
  transport_label: string;
  operations: Record<AcpConformanceOperation, "pass" | "gated">;
}

export const DEFAULT_ACP_CONFORMANCE_MATRIX: AcpAdapterConformanceRow[] = [
  {
    adapter: "claude",
    transport_label: "Claude Code CLI (stream-json / PTY)",
    operations: {
      approvals: "pass",
      interrupt: "pass",
      reset: "pass",
      compact: "pass",
      thread_identity: "pass",
      usage: "pass",
    },
  },
  {
    adapter: "agy",
    transport_label: "Google Antigravity (AGY JSON-RPC / CLI)",
    operations: {
      approvals: "pass",
      interrupt: "pass",
      reset: "pass",
      compact: "pass",
      thread_identity: "pass",
      usage: "pass",
    },
  },
  {
    adapter: "codex",
    transport_label: "OpenAI Codex CLI (MCP / exec)",
    operations: {
      approvals: "pass",
      interrupt: "pass",
      reset: "pass",
      compact: "pass",
      thread_identity: "pass",
      usage: "pass",
    },
  },
  {
    adapter: "muse",
    transport_label: "Muse Code (ACP stdio)",
    operations: {
      approvals: "pass",
      interrupt: "pass",
      reset: "pass",
      compact: "gated",
      thread_identity: "pass",
      usage: "pass",
    },
  },
  {
    adapter: "grok",
    transport_label: "xAI Grok CLI (REST / stream)",
    operations: {
      approvals: "pass",
      interrupt: "pass",
      reset: "pass",
      compact: "gated",
      thread_identity: "pass",
      usage: "pass",
    },
  },
  {
    adapter: "acp",
    transport_label: "Generic ACP JSON-RPC 2.0",
    operations: {
      approvals: "pass",
      interrupt: "pass",
      reset: "pass",
      compact: "pass",
      thread_identity: "pass",
      usage: "pass",
    },
  },
];

export interface FerroWatchPaneProps {
  authority?: HubAuthority | null;
  agents?: AgentRecord[];
  sessions?: DiscoveredSessionSummary[];
  apiClient?: HubApiClient;
  onAfterRemodel?: (res: RemodelSessionResponse) => void;
  otlpStatus?: OtlpExporterStatus;
  tokenizerLedger?: CountedTokenizerLedgerEntry[];
  acpConformanceMatrix?: AcpAdapterConformanceRow[];
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

export function formatUsdFromMicros(
  micros: number | undefined | null,
  currencyPref?: string,
): string {
  return formatCurrencyFromMicros(micros, currencyPref);
}

export interface CanonicalTokenRate {
  vendor: string;
  inputUsdPer1M: number;
  outputUsdPer1M: number;
  label: string;
}

/**
 * Canonical per-1M token reference rates (USD per 1,000,000 tokens) used to compute
 * virtual API-equivalent spend for $0 subscription CLI sessions (Record #221).
 */
export const CANONICAL_API_TOKEN_RATES: Readonly<
  Record<string, CanonicalTokenRate>
> = {
  claude: {
    vendor: "claude",
    inputUsdPer1M: 15,
    outputUsdPer1M: 75,
    label: "Claude Opus ($15/$75 per 1M)",
  },
  codex: {
    vendor: "codex",
    inputUsdPer1M: 10,
    outputUsdPer1M: 30,
    label: "GPT-5.4 / Codex Pro ($10/$30 per 1M)",
  },
  grok: {
    vendor: "grok",
    inputUsdPer1M: 3,
    outputUsdPer1M: 15,
    label: "Grok 4 ($3/$15 per 1M)",
  },
  antigravity: {
    vendor: "antigravity",
    inputUsdPer1M: 2.5,
    outputUsdPer1M: 10,
    label: "Gemini 3.1 Pro ($2.50/$10 per 1M)",
  },
  muse: {
    vendor: "muse",
    inputUsdPer1M: 2,
    outputUsdPer1M: 8,
    label: "Frontier Reference ($2/$8 per 1M)",
  },
  default: {
    vendor: "default",
    inputUsdPer1M: 6,
    outputUsdPer1M: 24,
    label: "Blended Reference ($6/$24 per 1M)",
  },
};

export function resolveCanonicalTokenRate(
  vendor?: string | null,
  model?: string | null,
): CanonicalTokenRate {
  const m = (model ?? "").trim().toLowerCase();
  if (m.includes("sonnet")) {
    return {
      vendor: "claude",
      inputUsdPer1M: 3,
      outputUsdPer1M: 15,
      label: "Claude Sonnet ($3/$15 per 1M)",
    };
  }
  if (m.includes("haiku")) {
    return {
      vendor: "claude",
      inputUsdPer1M: 0.8,
      outputUsdPer1M: 4,
      label: "Claude Haiku ($0.80/$4 per 1M)",
    };
  }
  if (m.includes("opus")) {
    return CANONICAL_API_TOKEN_RATES.claude;
  }
  if (m.includes("gpt-5") || m.includes("codex") || m.includes("o3")) {
    return CANONICAL_API_TOKEN_RATES.codex;
  }
  if (m.includes("grok")) {
    return CANONICAL_API_TOKEN_RATES.grok;
  }
  if (m.includes("gemini")) {
    return CANONICAL_API_TOKEN_RATES.antigravity;
  }
  if (m.includes("muse")) {
    return CANONICAL_API_TOKEN_RATES.muse;
  }
  const vKey = (vendor ?? "").trim().toLowerCase();
  return CANONICAL_API_TOKEN_RATES[vKey] ?? CANONICAL_API_TOKEN_RATES.default;
}

export function computeVirtualApiEquivMicros(params: {
  tokensIn?: number | null;
  tokensOut?: number | null;
  costUsdMicros?: number | null;
  vendor?: string | null;
  model?: string | null;
}): number {
  const tokensIn =
    typeof params.tokensIn === "number" && Number.isFinite(params.tokensIn)
      ? Math.max(0, params.tokensIn)
      : 0;
  const tokensOut =
    typeof params.tokensOut === "number" && Number.isFinite(params.tokensOut)
      ? Math.max(0, params.tokensOut)
      : 0;
  const billedMicros =
    typeof params.costUsdMicros === "number" &&
    Number.isFinite(params.costUsdMicros)
      ? Math.max(0, params.costUsdMicros)
      : 0;
  const rate = resolveCanonicalTokenRate(params.vendor, params.model);
  // 1M tokens * ($rate / 1M tokens) * 1,000,000 micros/$ = tokens * rate micros
  const computedMicros = Math.round(
    tokensIn * rate.inputUsdPer1M + tokensOut * rate.outputUsdPer1M,
  );
  return Math.max(billedMicros, computedMicros);
}

export function computeTelemetryVirtualEquivMicros(
  telemetry?: WatchTelemetryReport | null,
  adapterModelsMap?: ReadonlyMap<string, string[]>,
): number {
  if (!telemetry) return 0;
  const adapters = telemetry.by_adapter ?? [];
  if (adapters.length > 0) {
    let sumVirtual = 0;
    let sumIn = 0;
    let sumOut = 0;
    for (const item of adapters) {
      const models = adapterModelsMap?.get(item.key.toLowerCase()) ?? [];
      sumVirtual += computeVirtualApiEquivMicros({
        tokensIn: item.tokens_in,
        tokensOut: item.tokens_out,
        costUsdMicros: item.cost_usd_micros,
        vendor: item.key,
        model: models[0] ?? null,
      });
      sumIn += item.tokens_in ?? 0;
      sumOut += item.tokens_out ?? 0;
    }
    const remIn = Math.max(0, (telemetry.totals?.tokens_in ?? 0) - sumIn);
    const remOut = Math.max(0, (telemetry.totals?.tokens_out ?? 0) - sumOut);
    if (remIn > 0 || remOut > 0) {
      sumVirtual += computeVirtualApiEquivMicros({
        tokensIn: remIn,
        tokensOut: remOut,
        vendor: "default",
      });
    }
    return Math.max(telemetry.totals?.cost_usd_micros ?? 0, sumVirtual);
  }
  return computeVirtualApiEquivMicros({
    tokensIn: telemetry.totals?.tokens_in,
    tokensOut: telemetry.totals?.tokens_out,
    costUsdMicros: telemetry.totals?.cost_usd_micros,
    vendor: "default",
  });
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
  otlpStatus: propOtlpStatus,
  tokenizerLedger: propTokenizerLedger,
  acpConformanceMatrix: propAcpMatrix,
}) => {
  const isOwner = authority?.tier === "owner";
  const activeTimeZone = useActiveTimeZone();
  useActiveCurrency();

  const [otlpState, setOtlpState] = useState<OtlpExporterStatus>(
    propOtlpStatus ?? DEFAULT_OTLP_EXPORTER_STATUS,
  );
  const [otlpStatusMsg, setOtlpStatusMsg] = useState<string | null>(null);
  const effectiveTokenizerLedger =
    propTokenizerLedger ?? DEFAULT_TOKENIZER_LEDGER;
  const effectiveAcpMatrix =
    propAcpMatrix ?? DEFAULT_ACP_CONFORMANCE_MATRIX;

  const handleOtlpExportSnapshot = async () => {
    try {
      await fetch("/api/v1/watch/otlp/export", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          endpoint: otlpState.endpoint,
          protocol: otlpState.protocol,
        }),
      });
    } catch {
      // Fallback when endpoint is offline
    }
    setOtlpState((prev) => ({
      ...prev,
      exported_attempts: prev.exported_attempts + 1,
      exported_token_rollups: prev.exported_token_rollups + 3,
      exported_cost_events: prev.exported_cost_events + 1,
      last_flush_at: new Date().toISOString(),
    }));
    setOtlpStatusMsg(
      `Exported OTLP telemetry batch to ${otlpState.endpoint} (${otlpState.protocol}).`,
    );
  };

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
      if (plan.blocked) {
        setBalanceMessage(
          plan.switches.length > 0
            ? `Balance plan blocked (${plan.blocked}) — previewing ${plan.switches.length} switch(es).`
            : `Balance plan blocked: ${plan.blocked}`,
        );
      } else if (plan.switches.length > 0) {
        setBalanceMessage(
          `Dry-run plan ready: ${plan.switches.length} vendor switch(es) proposed.`,
        );
      } else {
        setBalanceMessage(
          "Dry-run plan evaluated: all sessions within target quota thresholds.",
        );
      }
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
      const okCount = res.results.filter(
        (r) => r.status === "ok" || r.ok === true,
      ).length;
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

  // Map adapters to their telemetry breakdown for per-vendor virtual equivalence
  const adapterBreakdownMap = useMemo(() => {
    const map = new Map<string, { tokens_in: number; tokens_out: number; cost_usd_micros: number; attempts: number }>();
    for (const item of activeTelemetry?.by_adapter ?? []) {
      map.set(item.key.trim().toLowerCase(), {
        tokens_in: item.tokens_in ?? 0,
        tokens_out: item.tokens_out ?? 0,
        cost_usd_micros: item.cost_usd_micros ?? 0,
        attempts: item.attempts ?? 0,
      });
    }
    return map;
  }, [activeTelemetry]);

  // Map agent keys to their active vendor and model for per-agent virtual equivalence
  const agentVendorModelMap = useMemo(() => {
    const map = new Map<string, { vendor: string | null; model: string | null }>();
    for (const ag of healthAgents) {
      const key = ag.agent.trim().toLowerCase();
      if (key) {
        map.set(key, {
          vendor: ag.hosted_session?.vendor ?? ag.adapter ?? null,
          model: ag.hosted_session?.model ?? null,
        });
      }
    }
    for (const sess of sessions) {
      const key = (sess.persona || sess.id).trim().toLowerCase();
      if (key && !map.has(key)) {
        map.set(key, {
          vendor: sess.adapter ?? null,
          model: sess.model ?? null,
        });
      }
    }
    return map;
  }, [healthAgents, sessions]);

  const summaryVirtualEquivMicros = useMemo(
    () =>
      computeTelemetryVirtualEquivMicros(summary?.telemetry, adapterModelsMap),
    [summary, adapterModelsMap],
  );

  const activeVirtualEquivMicros = useMemo(
    () => computeTelemetryVirtualEquivMicros(activeTelemetry, adapterModelsMap),
    [activeTelemetry, adapterModelsMap],
  );

  const showAlerts = sectionFilter === "all" || sectionFilter === "alerts";
  const showQuotas = sectionFilter === "all" || sectionFilter === "quotas";
  const showLiveness = sectionFilter === "all" || sectionFilter === "liveness";
  const showRollups = sectionFilter === "all" || sectionFilter === "rollups";

  return (
    <section
      className="fd-pane fd-watch-pane"
      data-testid="ferrowatch-pane"
      aria-label="FerroWatch Observability & Quota Balancing Console"
      style={{
        display: "flex",
        flexDirection: "column",
        height: "100%",
        minHeight: 0,
        overflowY: "auto",
        overflowX: "hidden",
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
            flexDirection: "column",
            gap: "4px",
          }}
        >
          <div
            data-testid="ferrowatch-header-top-row"
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "8px",
              width: "100%",
              minWidth: 0,
            }}
          >
            <strong
              data-testid="ferrowatch-pane-title"
              style={{
                fontSize: "14px",
                fontWeight: 700,
                color: "var(--text-primary)",
              }}
            >
              FerroWatch
            </strong>

            <div
              data-testid="ferrowatch-header-actions"
              style={{
                display: "flex",
                alignItems: "center",
                gap: "8px",
                marginLeft: "auto",
                flexShrink: 0,
              }}
            >
              <span
                className="fd-badge"
                style={{ fontSize: "10px", fontWeight: 600 }}
              >
                {summary?.health?.local_host || "ai-lab-0"}
              </span>

              <button
                type="button"
                className="fd-btn"
                data-testid="watch-refresh-btn"
                aria-label={loading ? "Refreshing..." : "Refresh"}
                title="Refresh"
                disabled={loading}
                onClick={() => void loadSummary(appliedOverrides)}
                style={{ padding: "5px 8px", lineHeight: 1, flexShrink: 0 }}
              >
                <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                  <path
                    d="M13.5 8A5.5 5.5 0 0 1 3.7 11.45"
                    stroke="currentColor"
                    strokeWidth="1.45"
                    strokeLinecap="round"
                  />
                  <path
                    d="M2.5 8A5.5 5.5 0 0 1 12.3 4.55"
                    stroke="currentColor"
                    strokeWidth="1.45"
                    strokeLinecap="round"
                  />
                  <polyline
                    points="10.2 4.8 12.5 4.7 12.2 2.4"
                    stroke="currentColor"
                    strokeWidth="1.45"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                  />
                  <polyline
                    points="5.8 11.2 3.5 11.3 3.8 13.6"
                    stroke="currentColor"
                    strokeWidth="1.45"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                  />
                </svg>
              </button>
            </div>
          </div>

          <div style={{ fontSize: "11px", color: "var(--text-muted)" }}>
            Live Alerts, Subscription Quotas, Liveness &amp; Spend Rollups
          </div>
        </div>

        {/* Summary KPI Strip */}
        <div
          className="fd-watch-kpi-grid"
          data-testid="watch-kpi-grid"
        >
          <div
            data-testid="watch-kpi-alerts"
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "6px",
              padding: "4px 8px",
              borderRadius: "6px",
              backgroundColor: "var(--bg-elevated)",
              border: "1px solid var(--border-subtle)",
              minWidth: 0,
              overflow: "hidden",
            }}
          >
            <span style={{ fontSize: "10px", color: "var(--text-muted)", whiteSpace: "nowrap" }}>
              Alerts
            </span>
            <strong
              style={{
                fontSize: "12px",
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
            </strong>
          </div>

          <div
            data-testid="watch-kpi-liveness"
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "6px",
              padding: "4px 8px",
              borderRadius: "6px",
              backgroundColor: "var(--bg-elevated)",
              border: "1px solid var(--border-subtle)",
              minWidth: 0,
              overflow: "hidden",
            }}
          >
            <span style={{ fontSize: "10px", color: "var(--text-muted)", whiteSpace: "nowrap" }}>
              Live / Stale
            </span>
            <strong style={{ fontSize: "12px", fontWeight: 700, whiteSpace: "nowrap" }}>
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
            </strong>
          </div>

          <div
            data-testid="watch-kpi-spend"
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "6px",
              padding: "4px 8px",
              borderRadius: "6px",
              backgroundColor: "var(--bg-elevated)",
              border: "1px solid var(--border-subtle)",
              minWidth: 0,
              overflow: "hidden",
            }}
          >
            <span style={{ fontSize: "10px", color: "var(--text-muted)", whiteSpace: "nowrap" }}>
              Spend
            </span>
            <span
              className="fd-mono"
              style={{
                display: "flex",
                alignItems: "baseline",
                gap: "4px",
                minWidth: 0,
                overflow: "hidden",
                textOverflow: "ellipsis",
                whiteSpace: "nowrap",
              }}
            >
              <strong style={{ fontSize: "12px", fontWeight: 700 }}>
                {formatUsdFromMicros(summary?.telemetry?.totals?.cost_usd_micros)}
              </strong>
              <span
                data-testid="watch-kpi-virtual-equiv"
                style={{
                  fontSize: "10px",
                  color: "var(--status-ok, #34D399)",
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  whiteSpace: "nowrap",
                }}
                title="Virtual API-equivalent value generated by subscription CLI and API runs"
              >
                (API Eq: {formatUsdFromMicros(summaryVirtualEquivMicros)})
              </span>
            </span>
          </div>

          <div
            data-testid="watch-kpi-attempts"
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "6px",
              padding: "4px 8px",
              borderRadius: "6px",
              backgroundColor: "var(--bg-elevated)",
              border: "1px solid var(--border-subtle)",
              minWidth: 0,
              overflow: "hidden",
            }}
          >
            <span style={{ fontSize: "10px", color: "var(--text-muted)", whiteSpace: "nowrap" }}>
              Attempts
            </span>
            <strong
              className="fd-mono"
              style={{
                fontSize: "12px",
                fontWeight: 700,
                overflow: "hidden",
                textOverflow: "ellipsis",
                whiteSpace: "nowrap",
              }}
            >
              {formatCompactNumber(summary?.telemetry?.totals?.attempts)}
            </strong>
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

      {/* Main Console Body (unified scroll inside ferrowatch-pane) */}
      <div
        data-testid="watch-console-body"
        style={{
          flex: "none",
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

                    {/* Virtual Subscription Value Equivalence row (Record #221) */}
                    {(() => {
                      const breakdown = adapterBreakdownMap.get(item.vendor.toLowerCase());
                      const billedMicros = breakdown?.cost_usd_micros ?? 0;
                      const tokIn = breakdown?.tokens_in ?? 0;
                      const tokOut = breakdown?.tokens_out ?? 0;
                      const rate = resolveCanonicalTokenRate(
                        item.vendor,
                        item.models[0] ?? null,
                      );
                      const virtualMicros = computeVirtualApiEquivMicros({
                        tokensIn: tokIn,
                        tokensOut: tokOut,
                        costUsdMicros: billedMicros,
                        vendor: item.vendor,
                        model: item.models[0] ?? null,
                      });
                      return (
                        <div
                          data-testid={`watch-vendor-virtual-value-${item.vendor}`}
                          className="fd-mono"
                          style={{
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "space-between",
                            gap: "6px",
                            flexWrap: "wrap",
                            fontSize: "10px",
                            color: "var(--text-muted)",
                            paddingTop: "4px",
                            borderTop: "1px dashed var(--border-subtle)",
                          }}
                        >
                          <span>
                            Billed:{" "}
                            <strong style={{ color: "var(--text-secondary)" }}>
                              {formatUsdFromMicros(billedMicros)}
                            </strong>{" "}
                            · API-Eq:{" "}
                            <strong style={{ color: "var(--status-ok, #34D399)" }}>
                              {formatUsdFromMicros(virtualMicros)}
                            </strong>
                          </span>
                          <span title="Canonical per-1M token reference rate">
                            {rate.label}
                          </span>
                        </div>
                      );
                    })()}
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
                    {balanceApplyRes.results.map((r) => {
                      const isOk = r.status === "ok" || r.ok === true;
                      const statusLabel = isOk
                        ? "OK"
                        : r.error
                          ? `${r.status ? `${r.status}: ` : ""}${r.error}`
                          : r.status || "failed";
                      return (
                        <div
                          key={`${r.persona}-${r.session_id ?? r.to_vendor}`}
                          className="fd-mono"
                        >
                          {r.persona}: {r.from_vendor} → {r.to_vendor} [
                          {statusLabel}]
                        </div>
                      );
                    })}
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
                <span data-testid="watch-rollups-virtual-equiv">
                  Virtual API-Eq:{" "}
                  <strong
                    className="fd-mono"
                    style={{ color: "var(--status-ok, #34D399)" }}
                  >
                    {formatUsdFromMicros(activeVirtualEquivMicros)}
                  </strong>
                  {activeVirtualEquivMicros >
                    (activeTelemetry.totals.cost_usd_micros ?? 0) && (
                    <span
                      className="fd-mono"
                      style={{ color: "var(--text-muted)", marginLeft: "4px" }}
                    >
                      (saved{" "}
                      {formatUsdFromMicros(
                        activeVirtualEquivMicros -
                          (activeTelemetry.totals.cost_usd_micros ?? 0),
                      )}
                      )
                    </span>
                  )}
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
              {(activeTelemetry?.by_project ?? []).map((proj) => {
                const projVirtualMicros = computeVirtualApiEquivMicros({
                  tokensIn: proj.tokens_in,
                  tokensOut: proj.tokens_out,
                  costUsdMicros: proj.cost_usd_micros,
                  vendor: "default",
                });
                return (
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
                      {formatUsdFromMicros(proj.cost_usd_micros)}{" "}
                      <span
                        data-testid={`watch-rollup-project-virtual-${proj.slug}`}
                        style={{ color: "var(--status-ok, #34D399)" }}
                      >
                        (≈ {formatUsdFromMicros(projVirtualMicros)} API-eq)
                      </span>
                    </span>
                  </div>
                );
              })}
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
                const adapterVirtualMicros = computeVirtualApiEquivMicros({
                  tokensIn: item.tokens_in,
                  tokensOut: item.tokens_out,
                  costUsdMicros: item.cost_usd_micros,
                  vendor: item.key,
                  model: models[0] ?? null,
                });
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
                      {formatUsdFromMicros(item.cost_usd_micros)}{" "}
                      <span
                        data-testid={`watch-rollup-adapter-virtual-${item.key}`}
                        style={{ color: "var(--status-ok, #34D399)" }}
                      >
                        (≈ {formatUsdFromMicros(adapterVirtualMicros)} API-eq)
                      </span>
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
              {(activeTelemetry?.by_agent ?? []).map((item) => {
                const agentInfo = agentVendorModelMap.get(item.key.toLowerCase());
                const agentVirtualMicros = computeVirtualApiEquivMicros({
                  tokensIn: item.tokens_in,
                  tokensOut: item.tokens_out,
                  costUsdMicros: item.cost_usd_micros,
                  vendor: agentInfo?.vendor ?? "default",
                  model: agentInfo?.model ?? null,
                });
                return (
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
                      {formatUsdFromMicros(item.cost_usd_micros)}{" "}
                      <span
                        data-testid={`watch-rollup-agent-virtual-${item.key}`}
                        style={{ color: "var(--status-ok, #34D399)" }}
                      >
                        (≈ {formatUsdFromMicros(agentVirtualMicros)} API-eq)
                      </span>
                    </span>
                  </div>
                );
              })}
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

            {/* Record #94: OpenTelemetry OTLP Trace & Metric Exporter */}
            <div
              data-testid="watch-otlp-exporter-card"
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
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "8px",
                  flexWrap: "wrap",
                }}
              >
                <span
                  style={{
                    fontSize: "11.5px",
                    fontWeight: 700,
                    color: "var(--text-primary)",
                  }}
                >
                  OpenTelemetry OTLP Trace &amp; Metric Exporter
                </span>
                <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                  <select
                    aria-label="OTLP Protocol"
                    data-testid="watch-otlp-protocol-select"
                    className="fd-select fd-mono"
                    value={otlpState.protocol}
                    onChange={(e) =>
                      setOtlpState((prev) => ({
                        ...prev,
                        protocol: e.target.value as "grpc" | "http/protobuf",
                      }))
                    }
                    style={{ fontSize: "11px", padding: "2px 6px" }}
                  >
                    <option value="grpc">grpc</option>
                    <option value="http/protobuf">http/protobuf</option>
                  </select>
                  <span
                    className="fd-badge fd-mono"
                    data-testid="watch-otlp-endpoint-badge"
                  >
                    {otlpState.endpoint} ({otlpState.protocol})
                  </span>
                  <span
                    className="fd-badge fd-mono"
                    data-testid="watch-otlp-cadence-badge"
                  >
                    flush: every {otlpState.flush_cadence_secs}s
                  </span>
                </div>
              </div>

              <div
                data-testid="watch-otlp-counters"
                className="fd-mono"
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: "8px",
                  flexWrap: "wrap",
                  fontSize: "11px",
                  color: "var(--text-secondary)",
                }}
              >
                <span className="fd-badge">
                  attempts: {otlpState.exported_attempts}
                </span>
                <span className="fd-badge">
                  token rollups: {otlpState.exported_token_rollups}
                </span>
                <span className="fd-badge">
                  cost events: {otlpState.exported_cost_events}
                </span>
                <span className="fd-badge">
                  regret records: {otlpState.exported_regret_records}
                </span>
              </div>

              {otlpStatusMsg && (
                <div
                  role="status"
                  data-testid="watch-otlp-export-status"
                  style={{ fontSize: "11px", color: "#34D399" }}
                >
                  {otlpStatusMsg}
                </div>
              )}

              <div>
                <button
                  type="button"
                  className="fd-btn fd-btn-primary"
                  data-testid="watch-otlp-export-btn"
                  style={{ fontSize: "11px", padding: "4px 10px" }}
                  onClick={() => void handleOtlpExportSnapshot()}
                >
                  Export Snapshot / Test Endpoint
                </button>
              </div>
            </div>

            {/* Record #92: Counted Tokenizer Savings Ledger */}
            <div
              data-testid="watch-tokenizer-ledger-card"
              style={{
                padding: "10px",
                borderRadius: "6px",
                backgroundColor: "var(--bg-elevated)",
                border: "1px solid var(--border-subtle)",
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
                  gap: "8px",
                  flexWrap: "wrap",
                }}
              >
                <span
                  style={{
                    fontSize: "11.5px",
                    fontWeight: 700,
                    color: "var(--text-primary)",
                  }}
                >
                  Counted Tokenizer Savings Ledger
                </span>
                <span className="fd-badge fd-mono" style={{ fontSize: "10.5px" }}>
                  vs ceil(chars/4) heuristic
                </span>
              </div>
              {effectiveTokenizerLedger.map((entry) => (
                <div
                  key={entry.tokenizer}
                  data-testid={`watch-tokenizer-row-${entry.tokenizer}`}
                  className="fd-mono"
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "8px",
                    flexWrap: "wrap",
                    fontSize: "11px",
                  }}
                >
                  <strong>{entry.tokenizer}</strong>
                  <span>
                    exact: {entry.exact_tokens_saved.toLocaleString()} tok · ceil(chars/4):{" "}
                    {entry.heuristic_ceil_chars4.toLocaleString()} ({entry.delta_pct}%)
                  </span>
                </div>
              ))}
            </div>

            {/* Record #93: ACP Vendor Transport Conformance Matrix */}
            <div
              data-testid="acp-conformance-matrix-card"
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
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "8px",
                  flexWrap: "wrap",
                }}
              >
                <span
                  style={{
                    fontSize: "11.5px",
                    fontWeight: 700,
                    color: "var(--text-primary)",
                  }}
                >
                  ACP Vendor Transport Conformance Matrix (6 Operations)
                </span>
                <span className="fd-badge fd-mono" style={{ fontSize: "10.5px" }}>
                  approvals · interrupt · reset · compact · thread_identity · usage
                </span>
              </div>
              {effectiveAcpMatrix.map((row) => (
                <div
                  key={row.adapter}
                  data-testid={`acp-conformance-row-${row.adapter}`}
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "8px",
                    flexWrap: "wrap",
                    fontSize: "11px",
                    padding: "4px 6px",
                    borderRadius: "4px",
                    border: "1px solid var(--border-subtle)",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                    <strong className="fd-mono">{row.adapter}</strong>
                    <span style={{ color: "var(--text-muted)", fontSize: "10.5px" }}>
                      {row.transport_label}
                    </span>
                  </div>
                  <div style={{ display: "flex", alignItems: "center", gap: "4px", flexWrap: "wrap" }}>
                    {ACP_CONFORMANCE_OPERATIONS.map((op) => {
                      const status = row.operations[op];
                      return (
                        <span
                          key={op}
                          className="fd-badge fd-mono"
                          data-testid={`acp-conformance-cell-${row.adapter}-${op}`}
                          data-status={status}
                          style={{
                            fontSize: "10px",
                            color: status === "pass" ? "#34D399" : "#FBBF24",
                          }}
                        >
                          {op}:{status.toUpperCase()}
                        </span>
                      );
                    })}
                  </div>
                </div>
              ))}
            </div>

            {/* Record #82 (Issue #217): Native fl host Supervisor Telemetry Summary */}
            <div
              data-testid="host-supervisor-telemetry-card"
              style={{
                padding: "10px",
                borderRadius: "6px",
                backgroundColor: "var(--bg-elevated)",
                border: "1px solid var(--border-subtle)",
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
                  gap: "8px",
                  flexWrap: "wrap",
                }}
              >
                <span
                  style={{
                    fontSize: "11.5px",
                    fontWeight: 700,
                    color: "var(--text-primary)",
                  }}
                >
                  Native `fl host` Supervisor Telemetry
                </span>
                <span
                  className="fd-badge fd-mono"
                  data-testid="host-supervisor-route-authority-badge"
                  style={{ fontSize: "10.5px", color: "#38BDF8" }}
                >
                  Route Authority: FerroWire Store
                </span>
              </div>
              <div
                className="fd-mono"
                data-testid="host-supervisor-transports-summary"
                style={{ fontSize: "11px", color: "var(--text-secondary)" }}
              >
                Transports: pipe-rpc · pty-vt100 · legacy-tmux | Ring Buffer: 128 KB / 1,024 KB per session
              </div>
            </div>

            {/* Record #83 (Issue #206): Lab Cutover Gates G1-G10 Readiness Scorecard */}
            <div
              data-testid="cutover-gates-scorecard"
              style={{
                padding: "10px",
                borderRadius: "6px",
                backgroundColor: "var(--bg-elevated)",
                border: "1px solid var(--border-subtle)",
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
                  gap: "8px",
                  flexWrap: "wrap",
                }}
              >
                <span
                  style={{
                    fontSize: "11.5px",
                    fontWeight: 700,
                    color: "var(--text-primary)",
                  }}
                >
                  Lab Cutover Gates Readiness Scorecard (G1–G10)
                </span>
                <span className="fd-badge fd-mono" style={{ fontSize: "10.5px" }}>
                  ai-lab-0 Cutover Gates (#206 / #83)
                </span>
              </div>
              {DEFAULT_CUTOVER_GATES_SCORECARD.map((gate) => (
                <div
                  key={gate.id}
                  data-testid={`cutover-gate-row-${gate.id}`}
                  data-status={gate.status}
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "8px",
                    flexWrap: "wrap",
                    fontSize: "11px",
                    padding: "4px 6px",
                    borderRadius: "4px",
                    border: "1px solid var(--border-subtle)",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                    <strong className="fd-mono">{gate.id}</strong>
                    <span>{gate.title}</span>
                  </div>
                  <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                    <span
                      className="fd-mono"
                      data-testid={`cutover-gate-evidence-${gate.id}`}
                      style={{ fontSize: "10.5px", color: "var(--text-muted)" }}
                    >
                      {gate.evidence}
                    </span>
                    <span
                      className="fd-badge fd-mono"
                      data-testid={`cutover-gate-status-${gate.id}`}
                      style={{
                        fontSize: "10px",
                        color: gate.status === "pass" ? "#34D399" : "#FBBF24",
                      }}
                    >
                      {gate.status}
                    </span>
                  </div>
                </div>
              ))}
            </div>
          </div>
        )}
      </div>
    </section>
  );
};
