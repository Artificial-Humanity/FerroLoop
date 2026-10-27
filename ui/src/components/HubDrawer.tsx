import React, { useState } from "react";
import type { HubApiClient } from "../api/client";
import type {
  ArchitectureResponse,
  ArchViolation,
  HubStatsResponse,
  WorkflowDef,
  WorkflowRun,
} from "../types/api";
import { getAdapterBadgeClass } from "./RosterSidebar";

export type HubSubTab = "workflows" | "architecture" | "stats";

export interface HubDrawerProps {
  workflows?: WorkflowDef[];
  workflowRuns?: WorkflowRun[];
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
}

export function formatUsdFromMicros(micros?: number | null): string {
  const safe = typeof micros === "number" && Number.isFinite(micros) ? micros : 0;
  return `$${(safe / 1_000_000).toFixed(4)}`;
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
}) => {
  const [subTab, setSubTab] = useState<HubSubTab>(defaultSubTab);
  const [selectedWorkflowSlug, setSelectedWorkflowSlug] = useState<string>(
    workflows[0]?.slug ?? workflows[0]?.id ?? "",
  );
  const [runTitle, setRunTitle] = useState<string>("");
  const [actionStatus, setActionStatus] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState<boolean>(false);
  const [copiedC4, setCopiedC4] = useState<boolean>(false);

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
      setTimeout(() => setCopiedC4(false), 1800);
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
          alignItems: "center",
          justifyContent: "space-between",
          gap: "8px",
          padding: "8px 16px",
          borderBottom: "1px solid var(--border-subtle)",
          backgroundColor: "var(--bg-surface)",
          flexWrap: "wrap",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
          <button
            type="button"
            className={`fd-btn ${subTab === "workflows" ? "fd-btn-active" : ""}`}
            aria-pressed={subTab === "workflows"}
            onClick={() => setSubTab("workflows")}
          >
            Workflows (fl flow)
          </button>
          <button
            type="button"
            className={`fd-btn ${subTab === "architecture" ? "fd-btn-active" : ""}`}
            aria-pressed={subTab === "architecture"}
            onClick={() => setSubTab("architecture")}
          >
            Architecture Conformance (fl arch)
          </button>
          <button
            type="button"
            className={`fd-btn ${subTab === "stats" ? "fd-btn-active" : ""}`}
            aria-pressed={subTab === "stats"}
            onClick={() => setSubTab("stats")}
          >
            Telemetry & Regret Audit (fl stats)
          </button>
        </div>

        {onRefresh && (
          <button type="button" className="fd-btn" onClick={onRefresh} disabled={loading}>
            {loading ? "Refreshing..." : "Refresh"}
          </button>
        )}
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
              }}
            >
              <label style={{ display: "flex", flexDirection: "column", gap: "4px", flex: 1 }}>
                <span style={{ fontSize: "11px", fontWeight: 600, color: "var(--text-muted)" }}>
                  Workflow Definition
                </span>
                {workflows.length > 0 ? (
                  <select
                    aria-label="Workflow Definition"
                    className="fd-select"
                    value={selectedWorkflowSlug || workflows[0]?.slug || workflows[0]?.id || ""}
                    onChange={(e) => setSelectedWorkflowSlug(e.target.value)}
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
                    aria-label="Workflow Definition"
                    className="fd-input fd-mono"
                    placeholder="Enter workflow slug (e.g. code-review)"
                    value={selectedWorkflowSlug}
                    onChange={(e) => setSelectedWorkflowSlug(e.target.value)}
                  />
                )}
              </label>

              <label style={{ display: "flex", flexDirection: "column", gap: "4px", flex: 1 }}>
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
                />
              </label>

              <button type="submit" className="fd-btn fd-btn-primary" disabled={submitting}>
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
                  Components ({archModel.components.length}) & Edges ({archModel.edges.length})
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
                      {archModel.components.map((comp) => (
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
          </>
        )}
      </div>
    </section>
  );
};
