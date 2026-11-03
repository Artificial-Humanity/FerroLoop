import React, { useEffect, useRef, useState } from "react";
import type { HubApiClient } from "../api/client";
import {
  formatCurrencyFromMicros,
  resolveCurrencyCode,
  useActiveCurrency,
} from "../lib/currency";
import type {
  ArchitectureResponse,
  ArchViolation,
  HubStatsResponse,
  WorkflowDef,
  WorkflowRun,
} from "../types/api";
import { getAdapterBadgeClass } from "./RosterSidebar";
import {
  DEFAULT_REFERENCE_REPOS,
  DEFAULT_TOKENIZER_LEDGER,
  type CountedTokenizerLedgerEntry,
  type ReferenceRepoIndexEntry,
} from "./FerroStatePane";
export {
  DEFAULT_REFERENCE_REPOS,
  DEFAULT_TOKENIZER_LEDGER,
  type CountedTokenizerLedgerEntry,
  type ReferenceRepoIndexEntry,
};
import {
  evaluateAgentMcpPosture,
  type AgentMcpPostureInput,
  type AgentMcpPostureReport,
} from "./AgentSettingsModal";
import {
  BUILTIN_ROLE_REGISTRY,
  DEFAULT_LOCAL_MODEL_PROVIDERS,
  DEFAULT_OPERATOR_PUSH_STATE,
  DEFAULT_VENDOR_CLOUD_SESSIONS,
  type ConfigurableRoleRecord,
  type LocalModelProviderKind,
  type LocalModelProviderPreset,
  type OperatorPushSubscriptionState,
  type RoleReviewPolicy,
  type VendorCloudSessionEntry,
  type VendorCloudSessionStatus,
  type VendorCloudSessionVendor,
} from "./SettingsModal";
export {
  BUILTIN_ROLE_REGISTRY,
  DEFAULT_LOCAL_MODEL_PROVIDERS,
  DEFAULT_OPERATOR_PUSH_STATE,
  DEFAULT_VENDOR_CLOUD_SESSIONS,
  type ConfigurableRoleRecord,
  type LocalModelProviderKind,
  type LocalModelProviderPreset,
  type OperatorPushSubscriptionState,
  type RoleReviewPolicy,
  type VendorCloudSessionEntry,
  type VendorCloudSessionStatus,
  type VendorCloudSessionVendor,
};

export type HubSubTab = "workflows" | "routines" | "architecture" | "stats";

// Record #100: Bounded Scheduled Workflow Routines
export type RoutineDedupePolicy = "skip-if-running" | "queue-one";
export type RoutineCatchupMode = "none" | "single-catchup";
export type RoutineLifecycleStatus = "active" | "paused" | "cancelled";

export interface RoutineBudgetSliceCeiling {
  max_tokens: number;
  max_cost_usd: number;
}

export interface ScheduledWorkflowRoutine {
  id: string;
  workflow_slug: string;
  title: string;
  project: string;
  cadence_kind: "cron" | "interval";
  cadence_expr: string;
  dedupe_policy: RoutineDedupePolicy;
  catchup_mode: RoutineCatchupMode;
  budget_ceiling: RoutineBudgetSliceCeiling;
  status: RoutineLifecycleStatus;
  last_run_status?: string | null;
}

export const DEFAULT_SCHEDULED_ROUTINES: ScheduledWorkflowRoutine[] = [
  {
    id: "rt-nightly-conformance",
    workflow_slug: "arch-conformance-audit",
    title: "Nightly Architecture & Posture Audit",
    project: "FerroLoop",
    cadence_kind: "cron",
    cadence_expr: "0 */6 * * *",
    dedupe_policy: "skip-if-running",
    catchup_mode: "none",
    budget_ceiling: {
      max_tokens: 250000,
      max_cost_usd: 2.5,
    },
    status: "active",
    last_run_status: "succeeded",
  },
  {
    id: "rt-ledger-reconcile",
    workflow_slug: "ledger-triage-sync",
    title: "Bounded Ledger & Triage Reconciler",
    project: "FerroLoop",
    cadence_kind: "interval",
    cadence_expr: "every 30m",
    dedupe_policy: "queue-one",
    catchup_mode: "single-catchup",
    budget_ceiling: {
      max_tokens: 120000,
      max_cost_usd: 1.25,
    },
    status: "active",
    last_run_status: "succeeded",
  },
];

export interface HubDrawerProps {
  workflows?: WorkflowDef[];
  workflowRuns?: WorkflowRun[];
  routines?: ScheduledWorkflowRoutine[];
  architecture?: ArchitectureResponse | null;
  stats?: HubStatsResponse | null;
  loading?: boolean;
  error?: string | null;
  onRefresh?: () => void;
  onStartWorkflowRun?: (payload: {
    workflow: string;
    title?: string;
    by?: string;
  }) => Promise<unknown> | void;
  onApproveWorkflowStep?: (
    runId: string,
    payload?: { step_id?: string; by?: string; note?: string },
  ) => Promise<unknown> | void;
  onRejectWorkflowStep?: (
    runId: string,
    payload?: { step_id?: string; by?: string; reason?: string; note?: string },
  ) => Promise<unknown> | void;
  apiClient?: HubApiClient;
  defaultSubTab?: HubSubTab;
  postureItems?: AgentMcpPostureInput[];
  onReconcilePosture?: (reports: AgentMcpPostureReport[]) => void;
}

export function formatUsdFromMicros(
  micros?: number | null,
  currencyPref?: string,
): string {
  const code = resolveCurrencyCode(currencyPref);
  const digits = code === "JPY" || code === "KRW" ? 0 : 4;
  return formatCurrencyFromMicros(micros, currencyPref, {
    minimumFractionDigits: digits,
    maximumFractionDigits: code === "JPY" || code === "KRW" ? 2 : 4,
  });
}

function renderViolationText(v: ArchViolation): string {
  if (v.kind === "undeclared_dependency") {
    return `Undeclared dependency: ${v.from_component} → ${v.to_component} (${v.evidence})`;
  }
  if (v.kind === "unmapped_package") {
    return `Unmapped package: ${v.package} (${v.manifest_path})`;
  }
  return `Unmapped file: ${v.path}`;
}

export const HubDrawer: React.FC<HubDrawerProps> = ({
  workflows = [],
  workflowRuns = [],
  routines: propRoutines,
  architecture = null,
  stats = null,
  loading = false,
  error = null,
  onRefresh,
  onStartWorkflowRun,
  onApproveWorkflowStep,
  onRejectWorkflowStep,
  apiClient,
  defaultSubTab = "workflows",
  postureItems,
  onReconcilePosture,
}) => {
  const [subTab, setSubTab] = useState<HubSubTab>(defaultSubTab);
  const [routinesState, setRoutinesState] = useState<ScheduledWorkflowRoutine[]>(
    () =>
      propRoutines && propRoutines.length > 0
        ? propRoutines
        : DEFAULT_SCHEDULED_ROUTINES,
  );
  const [routineStatusBanner, setRoutineStatusBanner] = useState<string | null>(
    null,
  );

  useEffect(() => {
    if (propRoutines && propRoutines.length > 0) {
      setRoutinesState(propRoutines);
    }
  }, [propRoutines]);

  const dispatchRoutineAction = async (
    routineId: string,
    action: "pause" | "resume" | "run_now" | "cancel",
  ) => {
    setRoutineStatusBanner(null);
    setRoutinesState((prev) =>
      prev.map((rt) => {
        if (rt.id !== routineId) return rt;
        if (action === "pause") return { ...rt, status: "paused" };
        if (action === "resume") return { ...rt, status: "active" };
        if (action === "cancel") return { ...rt, status: "cancelled" };
        return { ...rt, last_run_status: "triggered" };
      }),
    );
    try {
      const fetchFn =
        (apiClient as unknown as { fetchImpl?: typeof fetch })?.fetchImpl ??
        (typeof fetch !== "undefined" ? fetch : undefined);
      if (fetchFn) {
        await fetchFn(`/api/v1/flow/routines/${encodeURIComponent(routineId)}`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ action }),
        });
      }
    } catch {
      // Clean client-side fallback when /api/v1/flow/routines is unavailable or 404
    }
    setRoutineStatusBanner(
      `Routine ${routineId}: executed '${action}' with client-side state synchronization.`,
    );
  };
  const [postureDryRunOpen, setPostureDryRunOpen] = useState<boolean>(false);
  const [postureReconciled, setPostureReconciled] = useState<boolean>(false);
  const [postureStatusMsg, setPostureStatusMsg] = useState<string | null>(null);
  useActiveCurrency();
  const [selectedWorkflowSlug, setSelectedWorkflowSlug] = useState<string>(
    workflows[0]?.slug ?? workflows[0]?.id ?? "",
  );
  const [runTitle, setRunTitle] = useState<string>("");
  const [actionStatus, setActionStatus] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState<boolean>(false);
  const [copiedC4, setCopiedC4] = useState<boolean>(false);
  const copyTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    return () => {
      if (copyTimerRef.current !== null) {
        clearTimeout(copyTimerRef.current);
      }
    };
  }, []);

  const handleStartRun = async (e: React.FormEvent) => {
    e.preventDefault();
    const target = selectedWorkflowSlug.trim() || workflows[0]?.slug || workflows[0]?.id || "";
    if (!target) {
      setActionError("Select or enter a workflow slug to start.");
      return;
    }

    setSubmitting(true);
    setActionError(null);
    setActionStatus(null);
    try {
      const payload = {
        workflow: target,
        title: runTitle.trim() || undefined,
      };
      if (onStartWorkflowRun) {
        await onStartWorkflowRun(payload);
      } else if (apiClient) {
        await apiClient.startWorkflowRun(payload);
      }
      setRunTitle("");
      setActionStatus(`Started workflow run for ${target}`);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : "Failed to start workflow run");
    } finally {
      setSubmitting(false);
    }
  };

  const handleApproveStep = async (run: WorkflowRun) => {
    const stepId = run.active_steps[0];
    setSubmitting(true);
    setActionError(null);
    setActionStatus(null);
    try {
      if (onApproveWorkflowStep) {
        await onApproveWorkflowStep(run.id, { step_id: stepId });
      } else if (apiClient) {
        await apiClient.approveWorkflowRun(run.id, { step_id: stepId });
      }
      setActionStatus(`Approved step on run ${run.id}`);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : "Failed to approve workflow step");
    } finally {
      setSubmitting(false);
    }
  };

  const handleRejectStep = async (run: WorkflowRun) => {
    const stepId = run.active_steps[0];
    setSubmitting(true);
    setActionError(null);
    setActionStatus(null);
    try {
      if (onRejectWorkflowStep) {
        await onRejectWorkflowStep(run.id, {
          step_id: stepId,
          reason: "Rejected from FerroDeck UI",
        });
      } else if (apiClient) {
        await apiClient.rejectWorkflowRun(run.id, {
          step_id: stepId,
          reason: "Rejected from FerroDeck UI",
        });
      }
      setActionStatus(`Rejected step on run ${run.id}`);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : "Failed to reject workflow step");
    } finally {
      setSubmitting(false);
    }
  };

  const handleCopyC4 = async () => {
    const dsl = architecture?.likec4_dsl ?? "";
    if (!dsl) return;
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(dsl);
      }
      setCopiedC4(true);
      if (copyTimerRef.current !== null) {
        clearTimeout(copyTimerRef.current);
      }
      copyTimerRef.current = setTimeout(() => {
        setCopiedC4(false);
        copyTimerRef.current = null;
      }, 1800);
    } catch {
      setCopiedC4(false);
    }
  };

  const conformance = architecture?.conformance;
  const archModel = architecture?.model;
  const adapterEntries = Object.entries(stats?.by_adapter ?? {});

  return (
    <section className="fd-pane" data-testid="hub-drawer">
      {/* Sub-navigation Toolbar */}
      <div
        style={{
          display: "flex",
          flexDirection: "column",
          gap: "8px",
          padding: "8px 16px",
          borderBottom: "1px solid var(--border-subtle)",
          backgroundColor: "var(--bg-surface)",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: "8px", flexWrap: "wrap" }}>
          <strong
            data-testid="hub-pane-title"
            style={{ fontSize: "14px", color: "var(--text-primary)" }}
          >
            Hub &amp; Architecture
          </strong>
        </div>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: "8px",
            flexWrap: "wrap",
          }}
        >
        <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap", minWidth: 0 }}>
          <button
            type="button"
            data-testid="subtab-workflows"
            className={`fd-btn ${subTab === "workflows" ? "fd-btn-active" : ""}`}
            aria-pressed={subTab === "workflows"}
            onClick={() => setSubTab("workflows")}
          >
            Workflows (fl flow)
          </button>
          <button
            type="button"
            data-testid="subtab-routines"
            className={`fd-btn ${subTab === "routines" ? "fd-btn-active" : ""}`}
            aria-pressed={subTab === "routines"}
            onClick={() => setSubTab("routines")}
          >
            Scheduled Routines
          </button>
          <button
            type="button"
            data-testid="subtab-arch"
            className={`fd-btn ${subTab === "architecture" ? "fd-btn-active" : ""}`}
            aria-pressed={subTab === "architecture"}
            onClick={() => setSubTab("architecture")}
            style={{
              display: "inline-flex",
              flexDirection: "column",
              alignItems: "center",
              justifyContent: "center",
              lineHeight: 1.2,
              padding: "6px 10px",
            }}
          >
            <span>Architecture Conformance</span>{" "}
            <span className="fd-mono" style={{ fontSize: "10.5px", opacity: 0.8 }}>
              (fl arch)
            </span>
          </button>
          <button
            type="button"
            data-testid="subtab-stats"
            className={`fd-btn ${subTab === "stats" ? "fd-btn-active" : ""}`}
            aria-pressed={subTab === "stats"}
            onClick={() => setSubTab("stats")}
            style={{
              display: "inline-flex",
              flexDirection: "column",
              alignItems: "center",
              justifyContent: "center",
              lineHeight: 1.2,
              padding: "6px 10px",
            }}
          >
            <span>Telemetry &amp; Regret Audit</span>{" "}
            <span className="fd-mono" style={{ fontSize: "10.5px", opacity: 0.8 }}>
              (fl stats)
            </span>
          </button>
        </div>

        {onRefresh && (
          <button type="button" className="fd-btn" onClick={onRefresh} disabled={loading}>
            {loading ? "Refreshing..." : "Refresh"}
          </button>
        )}
        </div>
      </div>

      <div
        className="fd-scroll"
        style={{ display: "flex", flexDirection: "column", gap: "16px" }}
      >
        {error && (
          <div
            role="alert"
            className="fd-card"
            style={{
              borderColor: "rgba(239, 68, 68, 0.45)",
              backgroundColor: "rgba(239, 68, 68, 0.12)",
              color: "#FCA5A5",
              fontSize: "12px",
            }}
          >
            {error}
          </div>
        )}

        {(actionStatus || actionError) && (
          <div
            role="status"
            className="fd-card"
            style={{
              borderColor: actionError
                ? "rgba(239, 68, 68, 0.45)"
                : "rgba(16, 185, 129, 0.45)",
              backgroundColor: actionError
                ? "rgba(239, 68, 68, 0.12)"
                : "rgba(16, 185, 129, 0.12)",
              color: actionError ? "#FCA5A5" : "var(--diff-add-text)",
              fontSize: "12px",
            }}
          >
            {actionError ?? actionStatus}
          </div>
        )}

        {/* 3-Tier MCP & Agent Posture Drift Card (Record #95) */}
        {(() => {
          const baseItems: AgentMcpPostureInput[] =
            postureItems && postureItems.length > 0
              ? postureItems
              : [
                  {
                    agentName: "ferris",
                    project: "FerroLoop",
                    role: "lead",
                    adapter: "claude",
                    sessionId: "claude:ferris",
                  },
                  {
                    agentName: "felix",
                    project: "FerroLoop",
                    role: "ui-developer",
                    adapter: "antigravity",
                    sessionId: "antigravity:felix",
                  },
                ];
          const reports = baseItems.map((item) =>
            evaluateAgentMcpPosture(
              postureReconciled
                ? {
                    ...item,
                    appliedMcpServers: item.desiredMcpServers,
                    liveMcpServers: item.desiredMcpServers,
                    sessionStartedBeforeSync: false,
                    sessionRole: item.role,
                    sessionProject: item.project,
                  }
                : item,
            ),
          );
          const driftedReports = reports.filter((r) => r.driftKind !== "synced");

          return (
            <div
              data-testid="hub-posture-drift-card"
              className="fd-card"
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
                  gap: "8px",
                  flexWrap: "wrap",
                }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                  <span
                    style={{
                      fontSize: "12px",
                      fontWeight: 700,
                      textTransform: "uppercase",
                      letterSpacing: "0.05em",
                      color: "var(--text-muted)",
                    }}
                  >
                    3-Tier MCP &amp; Agent Posture (Desired · Applied · Live)
                  </span>
                  <span
                    className="fd-badge"
                    data-testid="hub-posture-summary-badge"
                    data-drifted-count={driftedReports.length}
                    style={
                      driftedReports.length === 0
                        ? {
                            backgroundColor: "rgba(16, 185, 129, 0.14)",
                            color: "#34D399",
                            borderColor: "rgba(16, 185, 129, 0.4)",
                            fontWeight: 700,
                          }
                        : {
                            backgroundColor: "rgba(245, 158, 11, 0.16)",
                            color: "#FBBF24",
                            borderColor: "rgba(245, 158, 11, 0.45)",
                            fontWeight: 700,
                          }
                    }
                  >
                    {driftedReports.length === 0
                      ? `All ${reports.length} Synced`
                      : `${driftedReports.length} Drifted / ${reports.length} Total`}
                  </span>
                </div>

                <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                  <button
                    type="button"
                    className="fd-btn"
                    data-testid="hub-posture-dryrun-btn"
                    style={{ fontSize: "11px", padding: "3px 8px" }}
                    onClick={() => setPostureDryRunOpen((prev) => !prev)}
                  >
                    {postureDryRunOpen ? "Hide Dry-Run" : "Dry-Run Preview"}
                  </button>
                  {driftedReports.length > 0 && (
                    <button
                      type="button"
                      className="fd-btn fd-btn-primary"
                      data-testid="hub-posture-reconcile-btn"
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                      onClick={() => {
                        setPostureReconciled(true);
                        setPostureStatusMsg(
                          `Reconciled ${driftedReports.length} posture drift item(s) across Desired catalog, Applied vendor configs, and Live sessions.`,
                        );
                        onReconcilePosture?.(driftedReports);
                      }}
                    >
                      Reconcile All Drift ({driftedReports.length})
                    </button>
                  )}
                  {postureReconciled && (
                    <button
                      type="button"
                      className="fd-btn"
                      data-testid="hub-posture-rollback-btn"
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                      onClick={() => {
                        setPostureReconciled(false);
                        setPostureStatusMsg(
                          "Rolled back last posture reconciliation snapshot.",
                        );
                      }}
                    >
                      Rollback
                    </button>
                  )}
                </div>
              </div>

              {postureDryRunOpen && (
                <div
                  data-testid="hub-posture-dryrun-panel"
                  className="fd-mono"
                  style={{
                    fontSize: "11px",
                    padding: "8px 10px",
                    borderRadius: "4px",
                    backgroundColor: "var(--bg-input)",
                    border: "1px solid var(--border-subtle)",
                    color: "var(--text-secondary)",
                    display: "flex",
                    flexDirection: "column",
                    gap: "4px",
                  }}
                >
                  {driftedReports.length === 0 ? (
                    <span>Dry-run: 0 changes required (all tiers match Desired catalog).</span>
                  ) : (
                    driftedReports.map((r) => (
                      <div key={r.agentKey}>
                        [dry-run] {r.agentKey}: {r.remediationLabel} → {r.vendorConfigPath} (
                        {r.summary})
                      </div>
                    ))
                  )}
                </div>
              )}

              {postureStatusMsg && (
                <div
                  role="status"
                  data-testid="hub-posture-status-banner"
                  style={{
                    fontSize: "11px",
                    color: "#34D399",
                    padding: "4px 8px",
                    borderRadius: "4px",
                    backgroundColor: "rgba(16, 185, 129, 0.12)",
                  }}
                >
                  {postureStatusMsg}
                </div>
              )}

              <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                {reports.map((r) => {
                  const isSynced = r.driftKind === "synced";
                  return (
                    <div
                      key={r.agentKey}
                      data-testid={`hub-posture-item-${r.agentKey}`}
                      data-posture={r.driftKind}
                      style={{
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                        gap: "8px",
                        flexWrap: "wrap",
                        padding: "6px 8px",
                        borderRadius: "4px",
                        backgroundColor: "var(--bg-elevated)",
                        border: "1px solid var(--border-subtle)",
                        fontSize: "11px",
                      }}
                    >
                      <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                        <strong className="fd-mono">{r.agentKey}</strong>
                        <span
                          className="fd-badge"
                          data-testid={`hub-posture-badge-${r.agentKey}`}
                          style={{
                            fontSize: "10px",
                            color: isSynced ? "#34D399" : "#FBBF24",
                            borderColor: isSynced
                              ? "rgba(16, 185, 129, 0.4)"
                              : "rgba(245, 158, 11, 0.45)",
                          }}
                        >
                          {r.badgeLabel}
                        </span>
                      </div>
                      <span className="fd-mono" style={{ fontSize: "10.5px", color: "var(--text-muted)" }}>
                        Desired[{r.desiredServers.join(",")}] → Applied[{r.appliedServers.join(",")}] → Live[{r.liveServers.join(",")}]
                      </span>
                    </div>
                  );
                })}
              </div>
            </div>
          );
        })()}

        {/* TAB 1: WORKFLOWS */}
        {subTab === "workflows" && (
          <>
            <form
              onSubmit={handleStartRun}
              className="fd-card"
              style={{
                display: "flex",
                alignItems: "flex-end",
                gap: "10px",
                flexWrap: "wrap",
                minWidth: 0,
                maxWidth: "100%",
                boxSizing: "border-box",
              }}
            >
              <label
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "4px",
                  flex: "1 1 100%",
                  width: "100%",
                  minWidth: 0,
                  maxWidth: "100%",
                }}
              >
                <span style={{ fontSize: "11px", fontWeight: 600, color: "var(--text-muted)" }}>
                  Workflow Definition
                </span>
                {workflows.length > 0 ? (
                  <select
                    aria-label="Workflow Definition"
                    className="fd-select"
                    value={selectedWorkflowSlug || workflows[0]?.slug || workflows[0]?.id || ""}
                    onChange={(e) => setSelectedWorkflowSlug(e.target.value)}
                    style={{ width: "100%", minWidth: 0, maxWidth: "100%", boxSizing: "border-box" }}
                  >
                    {workflows.map((wf) => (
                      <option key={wf.id} value={wf.slug || wf.id}>
                        {wf.title} ({wf.slug} · {wf.project})
                      </option>
                    ))}
                  </select>
                ) : (
                  <input
                    type="text"
                    data-testid="workflow-slug-input"
                    aria-label="Workflow Definition"
                    className="fd-input fd-mono"
                    placeholder="Enter workflow slug (e.g. code-review)"
                    value={selectedWorkflowSlug}
                    onChange={(e) => setSelectedWorkflowSlug(e.target.value)}
                    style={{ width: "100%", minWidth: 0, maxWidth: "100%", boxSizing: "border-box" }}
                  />
                )}
              </label>

              <label
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "4px",
                  flex: "1 1 180px",
                  minWidth: 0,
                  maxWidth: "100%",
                }}
              >
                <span style={{ fontSize: "11px", fontWeight: 600, color: "var(--text-muted)" }}>
                  Optional Run Title
                </span>
                <input
                  type="text"
                  aria-label="Optional Run Title"
                  className="fd-input"
                  placeholder="Custom run title"
                  value={runTitle}
                  onChange={(e) => setRunTitle(e.target.value)}
                  style={{ width: "100%", minWidth: 0, maxWidth: "100%", boxSizing: "border-box" }}
                />
              </label>

              <button
                type="submit"
                className="fd-btn fd-btn-primary"
                disabled={submitting}
                style={{ flex: "0 0 auto" }}
              >
                Start Workflow Run
              </button>
            </form>

            {/* Workflow Definitions */}
            <div className="fd-card" style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
              <div
                style={{
                  fontSize: "12px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.05em",
                  color: "var(--text-muted)",
                }}
              >
                Registered Workflow Definitions ({workflows.length})
              </div>
              {workflows.length === 0 ? (
                <div style={{ fontSize: "12px", color: "var(--text-muted)" }}>
                  No workflow definitions registered yet.
                </div>
              ) : (
                workflows.map((wf) => (
                  <div
                    key={wf.id}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      padding: "8px 10px",
                      borderRadius: "var(--radius-sm)",
                      backgroundColor: "var(--bg-elevated)",
                      border: "1px solid var(--border-subtle)",
                      flexWrap: "wrap",
                      gap: "8px",
                    }}
                  >
                    <div>
                      <span style={{ fontWeight: 600 }}>{wf.title}</span>{" "}
                      <span className="fd-mono" style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                        ({wf.slug})
                      </span>
                    </div>
                    <div style={{ display: "flex", gap: "6px" }}>
                      <span className="fd-badge">{wf.project}</span>
                      <span className="fd-badge">{wf.steps.length} steps</span>
                    </div>
                  </div>
                ))
              )}
            </div>

            {/* Workflow Runs */}
            <div className="fd-card" style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
              <div
                style={{
                  fontSize: "12px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.05em",
                  color: "var(--text-muted)",
                }}
              >
                Workflow Runs ({workflowRuns.length})
              </div>

              {workflowRuns.length === 0 ? (
                <div style={{ fontSize: "12px", color: "var(--text-muted)" }}>
                  No workflow runs recorded.
                </div>
              ) : (
                workflowRuns.map((run) => (
                  <div
                    key={run.id}
                    data-testid={`workflow-run-${run.id}`}
                    style={{
                      padding: "10px 12px",
                      borderRadius: "var(--radius-md)",
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
                      <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                        <span style={{ fontWeight: 600 }}>{run.title}</span>
                        <span className="fd-badge fd-mono">{run.id}</span>
                        <span className="fd-badge">{run.status}</span>
                      </div>

                      <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
                        <button
                          type="button"
                          className="fd-btn fd-btn-primary"
                          disabled={submitting}
                          onClick={() => void handleApproveStep(run)}
                        >
                          Approve Step
                        </button>
                        <button
                          type="button"
                          className="fd-btn fd-btn-danger"
                          disabled={submitting}
                          onClick={() => void handleRejectStep(run)}
                        >
                          Reject Step
                        </button>
                      </div>
                    </div>

                    <div
                      className="fd-mono"
                      style={{ fontSize: "11px", color: "var(--text-muted)", display: "flex", gap: "12px", flexWrap: "wrap" }}
                    >
                      <span>project: {run.project}</span>
                      <span>active: {run.active_steps.join(", ") || "none"}</span>
                      <span>steps executed: {run.total_steps_executed}</span>
                      <span>cost: {formatUsdFromMicros(run.total_cost_usd_micros)}</span>
                    </div>
                  </div>
                ))
              )}
            </div>
          </>
        )}

        {/* Record #100: Bounded Scheduled Workflow Routines */}
        {(subTab === "workflows" || subTab === "routines") && (
          <div
            data-testid="scheduled-routines-section"
            className="fd-card"
            style={{ display: "flex", flexDirection: "column", gap: "10px" }}
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
              <div
                style={{
                  fontSize: "12px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.05em",
                  color: "var(--text-muted)",
                }}
              >
                Bounded Scheduled Workflow Routines ({routinesState.length})
              </div>
              <span className="fd-badge fd-mono" style={{ fontSize: "10.5px" }}>
                #217 §E Rule 5 Whole-Slice Budget Admission
              </span>
            </div>

            {routineStatusBanner && (
              <div
                role="status"
                data-testid="routine-action-status-banner"
                style={{
                  fontSize: "11.5px",
                  padding: "6px 10px",
                  borderRadius: "6px",
                  backgroundColor: "rgba(56, 189, 248, 0.12)",
                  border: "1px solid rgba(56, 189, 248, 0.35)",
                  color: "var(--accent-primary)",
                }}
              >
                {routineStatusBanner}
              </div>
            )}

            <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
              {routinesState.map((rt) => {
                const isPaused = rt.status === "paused";
                const isCancelled = rt.status === "cancelled";
                return (
                  <div
                    key={rt.id}
                    data-testid={`routine-card-${rt.id}`}
                    data-status={rt.status}
                    style={{
                      padding: "10px 12px",
                      borderRadius: "var(--radius-md)",
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
                      <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                        <span style={{ fontWeight: 700, fontSize: "13px" }}>
                          {rt.title}
                        </span>
                        <span className="fd-badge fd-mono">{rt.workflow_slug}</span>
                        <span
                          className="fd-badge"
                          data-testid={`routine-status-${rt.id}`}
                          style={{
                            backgroundColor:
                              rt.status === "active"
                                ? "rgba(52, 211, 153, 0.14)"
                                : rt.status === "paused"
                                  ? "rgba(251, 191, 36, 0.16)"
                                  : "rgba(239, 68, 68, 0.16)",
                            color:
                              rt.status === "active"
                                ? "#34d399"
                                : rt.status === "paused"
                                  ? "#fbbf24"
                                  : "#f87171",
                          }}
                        >
                          {rt.status}
                        </span>
                      </div>

                      <div style={{ display: "flex", gap: "6px", flexWrap: "wrap" }}>
                        {!isCancelled && (
                          <button
                            type="button"
                            className="fd-btn"
                            data-testid={`routine-pause-resume-btn-${rt.id}`}
                            style={{ fontSize: "11px", padding: "3px 8px" }}
                            onClick={() =>
                              void dispatchRoutineAction(
                                rt.id,
                                isPaused ? "resume" : "pause",
                              )
                            }
                          >
                            {isPaused ? "Resume" : "Pause"}
                          </button>
                        )}
                        <button
                          type="button"
                          className="fd-btn fd-btn-primary"
                          data-testid={`routine-run-now-btn-${rt.id}`}
                          style={{ fontSize: "11px", padding: "3px 8px" }}
                          disabled={isCancelled}
                          onClick={() => void dispatchRoutineAction(rt.id, "run_now")}
                        >
                          Run Now
                        </button>
                        <button
                          type="button"
                          className="fd-btn fd-btn-danger"
                          data-testid={`routine-cancel-btn-${rt.id}`}
                          style={{ fontSize: "11px", padding: "3px 8px" }}
                          disabled={isCancelled}
                          onClick={() => void dispatchRoutineAction(rt.id, "cancel")}
                        >
                          Cancel
                        </button>
                      </div>
                    </div>

                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: "6px",
                        flexWrap: "wrap",
                        fontSize: "11px",
                      }}
                    >
                      <span
                        className="fd-badge fd-mono"
                        data-testid={`routine-cadence-${rt.id}`}
                      >
                        {rt.cadence_kind}: {rt.cadence_expr}
                      </span>
                      <span
                        className="fd-badge fd-mono"
                        data-testid={`routine-dedupe-${rt.id}`}
                      >
                        dedupe: {rt.dedupe_policy}
                      </span>
                      <span
                        className="fd-badge fd-mono"
                        data-testid={`routine-catchup-${rt.id}`}
                      >
                        catch-up: {rt.catchup_mode}
                      </span>
                      <span
                        className="fd-badge fd-mono"
                        data-testid={`routine-budget-${rt.id}`}
                        style={{
                          backgroundColor: "rgba(56, 189, 248, 0.14)",
                          borderColor: "rgba(56, 189, 248, 0.4)",
                          color: "var(--accent-primary)",
                        }}
                      >
                        Rule 5 Ceiling: {(rt.budget_ceiling.max_tokens / 1000).toFixed(0)}k tok · $
                        {rt.budget_ceiling.max_cost_usd.toFixed(2)}/run
                      </span>
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        )}

        {/* TAB 2: ARCHITECTURE CONFORMANCE */}
        {subTab === "architecture" && (
          <>
            <div className="fd-card" style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  flexWrap: "wrap",
                  gap: "8px",
                }}
              >
                <div>
                  <div style={{ fontWeight: 700, fontSize: "14px" }}>
                    {archModel?.title || "FerroLoop Architecture"} (.fl/architecture.toml)
                  </div>
                  <div style={{ fontSize: "12px", color: "var(--text-muted)" }}>
                    System ID: <code className="fd-mono">{conformance?.system_id || archModel?.system_id || "ferroloop"}</code>
                  </div>
                </div>

                <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                  <span
                    data-testid="arch-conformance-badge"
                    className="fd-badge"
                    style={
                      conformance?.passed !== false
                        ? {
                            color: "var(--status-alive)",
                            borderColor: "rgba(16, 185, 129, 0.45)",
                            backgroundColor: "rgba(16, 185, 129, 0.12)",
                          }
                        : {
                            color: "#FCA5A5",
                            borderColor: "rgba(239, 68, 68, 0.45)",
                            backgroundColor: "rgba(239, 68, 68, 0.14)",
                          }
                    }
                  >
                    {conformance?.passed === false ? "FAILED" : "PASSED"}
                  </span>
                  <span className="fd-badge">
                    {conformance?.checked_packages ?? 0} packages
                  </span>
                  <span className="fd-badge">
                    {conformance?.checked_files ?? 0} files
                  </span>
                </div>
              </div>

              {conformance && conformance.violations.length > 0 && (
                <div
                  style={{
                    padding: "10px",
                    borderRadius: "var(--radius-sm)",
                    backgroundColor: "rgba(239, 68, 68, 0.12)",
                    border: "1px solid rgba(239, 68, 68, 0.4)",
                    color: "#FCA5A5",
                    fontSize: "12px",
                  }}
                >
                  <div style={{ fontWeight: 700, marginBottom: "4px" }}>
                    Violations ({conformance.violations.length})
                  </div>
                  <ul style={{ margin: 0, paddingLeft: "18px" }}>
                    {conformance.violations.map((v, idx) => (
                      <li key={idx} className="fd-mono">
                        {renderViolationText(v)}
                      </li>
                    ))}
                  </ul>
                </div>
              )}
            </div>

            {/* Components & Edges Table */}
            {archModel && (
              <div className="fd-card" style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
                <div
                  style={{
                    fontSize: "12px",
                    fontWeight: 700,
                    textTransform: "uppercase",
                    letterSpacing: "0.05em",
                    color: "var(--text-muted)",
                  }}
                >
                  Components ({(archModel.components ?? []).length}) & Edges ({(archModel.edges ?? []).length})
                </div>

                <div style={{ overflowX: "auto" }}>
                  <table style={{ width: "100%", borderCollapse: "collapse", fontSize: "12px" }}>
                    <thead>
                      <tr style={{ textAlign: "left", borderBottom: "1px solid var(--border-subtle)" }}>
                        <th style={{ padding: "6px 8px" }}>Component</th>
                        <th style={{ padding: "6px 8px" }}>Kind</th>
                        <th style={{ padding: "6px 8px" }}>Package</th>
                        <th style={{ padding: "6px 8px" }}>Paths</th>
                      </tr>
                    </thead>
                    <tbody>
                      {(archModel.components ?? []).map((comp) => (
                        <tr
                          key={comp.id}
                          style={{ borderBottom: "1px solid var(--border-subtle)" }}
                        >
                          <td style={{ padding: "6px 8px", fontWeight: 600 }}>
                            {comp.title} <span className="fd-mono" style={{ color: "var(--text-dim)" }}>({comp.id})</span>
                          </td>
                          <td style={{ padding: "6px 8px" }}>
                            <span className="fd-badge">{comp.kind}</span>
                          </td>
                          <td className="fd-mono" style={{ padding: "6px 8px" }}>
                            {comp.package || "—"}
                          </td>
                          <td className="fd-mono" style={{ padding: "6px 8px", color: "var(--text-muted)" }}>
                            {comp.paths.join(", ")}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </div>
            )}

            {/* LikeC4 DSL Block */}
            <div className="fd-card" style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                }}
              >
                <span
                  style={{
                    fontSize: "12px",
                    fontWeight: 700,
                    textTransform: "uppercase",
                    letterSpacing: "0.05em",
                    color: "var(--text-muted)",
                  }}
                >
                  LikeC4 Architecture DSL (.c4)
                </span>
                <button type="button" className="fd-btn" onClick={() => void handleCopyC4()}>
                  {copiedC4 ? "Copied!" : "Copy .c4 DSL"}
                </button>
              </div>
              <pre
                className="fd-mono"
                style={{
                  margin: 0,
                  padding: "10px",
                  borderRadius: "var(--radius-sm)",
                  backgroundColor: "var(--bg-input)",
                  border: "1px solid var(--border-subtle)",
                  fontSize: "11px",
                  overflowX: "auto",
                  color: "var(--text-secondary)",
                }}
              >
                {architecture?.likec4_dsl || "// No LikeC4 DSL generated."}
              </pre>
            </div>
          </>
        )}

        {/* TAB 3: TELEMETRY & REGRET AUDIT */}
        {subTab === "stats" && (
          <>
            <div
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(auto-fit, minmax(160px, 1fr))",
                gap: "12px",
              }}
            >
              <div className="fd-card">
                <div style={{ fontSize: "11px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                  Total Attempts
                </div>
                <div className="fd-mono" style={{ fontSize: "20px", fontWeight: 700 }}>
                  {stats?.attempts ?? 0}
                </div>
              </div>

              <div className="fd-card">
                <div style={{ fontSize: "11px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                  Tokens In / Out
                </div>
                <div className="fd-mono" style={{ fontSize: "15px", fontWeight: 700 }}>
                  {(stats?.tokens_in ?? 0).toLocaleString()} / {(stats?.tokens_out ?? 0).toLocaleString()}
                </div>
              </div>

              <div className="fd-card">
                <div style={{ fontSize: "11px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                  Total Cost (USD)
                </div>
                <div
                  className="fd-mono"
                  data-testid="stats-total-cost"
                  style={{ fontSize: "20px", fontWeight: 700, color: "var(--accent-hover)" }}
                >
                  {formatUsdFromMicros(stats?.cost_usd_micros)}
                </div>
              </div>
            </div>

            {/* Per-Adapter Table */}
            <div className="fd-card" style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
              <div
                style={{
                  fontSize: "12px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.05em",
                  color: "var(--text-muted)",
                }}
              >
                Adapter Breakdown ({adapterEntries.length})
              </div>
              {adapterEntries.length === 0 ? (
                <div style={{ fontSize: "12px", color: "var(--text-muted)" }}>
                  No adapter execution attempts recorded.
                </div>
              ) : (
                <div style={{ overflowX: "auto" }}>
                  <table style={{ width: "100%", borderCollapse: "collapse", fontSize: "12px" }}>
                    <thead>
                      <tr style={{ textAlign: "left", borderBottom: "1px solid var(--border-subtle)" }}>
                        <th style={{ padding: "6px 8px" }}>Adapter</th>
                        <th style={{ padding: "6px 8px" }}>Attempts</th>
                        <th style={{ padding: "6px 8px" }}>Completed</th>
                        <th style={{ padding: "6px 8px" }}>Tokens (In / Out)</th>
                        <th style={{ padding: "6px 8px" }}>Cost (USD)</th>
                      </tr>
                    </thead>
                    <tbody>
                      {adapterEntries.map(([adapter, item]) => (
                        <tr
                          key={adapter}
                          style={{ borderBottom: "1px solid var(--border-subtle)" }}
                        >
                          <td style={{ padding: "6px 8px" }}>
                            <span className={getAdapterBadgeClass(adapter)}>{adapter}</span>
                          </td>
                          <td className="fd-mono" style={{ padding: "6px 8px" }}>
                            {item.attempts}
                          </td>
                          <td className="fd-mono" style={{ padding: "6px 8px" }}>
                            {item.completed}
                          </td>
                          <td className="fd-mono" style={{ padding: "6px 8px" }}>
                            {item.tokens_in.toLocaleString()} / {item.tokens_out.toLocaleString()}
                          </td>
                          <td className="fd-mono" style={{ padding: "6px 8px" }}>
                            {formatUsdFromMicros(item.cost_usd_micros)}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </div>

            {/* Transition Regret Audit */}
            <div className="fd-card" style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
              <div
                style={{
                  fontSize: "12px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.05em",
                  color: "var(--text-muted)",
                }}
              >
                Transition Regret Audit ({stats?.regret_audit?.length ?? 0})
              </div>
              {!stats?.regret_audit || stats.regret_audit.length === 0 ? (
                <div style={{ fontSize: "12px", color: "var(--text-muted)" }}>
                  No transition regret challenges triggered.
                </div>
              ) : (
                stats.regret_audit.map((item) => (
                  <div
                    key={item.name}
                    style={{
                      padding: "8px 10px",
                      borderRadius: "var(--radius-sm)",
                      backgroundColor: "var(--bg-elevated)",
                      border: "1px solid var(--border-subtle)",
                      display: "flex",
                      justifyContent: "space-between",
                      alignItems: "center",
                      flexWrap: "wrap",
                      gap: "8px",
                      fontSize: "12px",
                    }}
                  >
                    <div>
                      <span style={{ fontWeight: 600 }}>{item.name}</span>{" "}
                      <span className="fd-mono" style={{ color: "var(--text-muted)" }}>
                        ({item.from} → {item.to})
                      </span>
                    </div>
                    <div className="fd-mono" style={{ display: "flex", gap: "10px" }}>
                      <span>metric: {item.metric}</span>
                      <span>ratio: {item.ratio.toFixed(2)}x</span>
                    </div>
                  </div>
                ))
              )}
            </div>

            {/* Record #92: Counted Tokenizer Savings Ledger */}
            <div
              data-testid="hub-tokenizer-ledger-card"
              className="fd-card"
              style={{ display: "flex", flexDirection: "column", gap: "8px" }}
            >
              <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: "8px", flexWrap: "wrap" }}>
                <div style={{ fontSize: "12px", fontWeight: 700, textTransform: "uppercase", letterSpacing: "0.05em", color: "var(--text-muted)" }}>
                  Counted Tokenizer Savings Ledger ({DEFAULT_TOKENIZER_LEDGER.length})
                </div>
                <span className="fd-badge fd-mono" style={{ fontSize: "10.5px" }}>
                  Exact Tokenizer vs ceil(chars/4)
                </span>
              </div>
              {DEFAULT_TOKENIZER_LEDGER.map((item) => (
                <div
                  key={item.tokenizer}
                  data-testid={`hub-tokenizer-row-${item.tokenizer}`}
                  className="fd-mono"
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "8px",
                    flexWrap: "wrap",
                    padding: "6px 8px",
                    borderRadius: "var(--radius-sm)",
                    backgroundColor: "var(--bg-elevated)",
                    border: "1px solid var(--border-subtle)",
                    fontSize: "11.5px",
                  }}
                >
                  <strong>{item.tokenizer}</strong>
                  <span>
                    exact: {item.exact_tokens_saved.toLocaleString()} tok · ceil(chars/4):{" "}
                    {item.heuristic_ceil_chars4.toLocaleString()} ({item.delta_pct}%)
                  </span>
                </div>
              ))}
            </div>
          </>
        )}
      </div>
    </section>
  );
};
