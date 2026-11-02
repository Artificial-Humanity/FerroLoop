import React, { useEffect, useState } from "react";
import { HubApiClient, HubApiError } from "../api/client";
import type { SessionQuotaUsage } from "../types/api";
import { PinIcon, togglePinnedAgent, usePinnedState } from "./RosterSidebar";
import {
  CANONICAL_PROJECTS,
  CREATE_CUSTOM_ROLE_VALUE,
  extractRecastConflict,
  normalizeRecastRole,
  RECAST_ROLE_OPTIONS,
  type RecastConflictState,
} from "./SettingsModal";
import {
  formatQuotaResetCountdown,
  formatQuotaWindowRemaining,
  formatTokenCount,
  getVendorModelOptions,
  hasQuotaUsageData,
  type EstimatedContextUsage,
} from "./ChatPane";

export const THINKING_LEVEL_OPTIONS = [
  { value: "low", label: "low (Fast)" },
  { value: "medium", label: "medium (Balanced)" },
  { value: "high", label: "high (Default)" },
  { value: "xhigh", label: "xhigh (Maximum Reasoning)" },
] as const;

export interface AgentSettingsModalProps {
  isOpen: boolean;
  onClose: () => void;
  agentName?: string | null;
  agentDisplayName?: string | null;
  agentRole?: string | null;
  agentProject?: string | null;
  adapter?: string | null;
  sessionId?: string | null;
  selectedModel: string;
  selectedThinkingLevel: string;
  availableModels?: string[] | null;
  switchingModel?: boolean;
  onChangeModelAndThinking: (nextModel: string, nextThinkingLevel: string) => void;
  contextEstimate: EstimatedContextUsage;
  quotaUsage?: SessionQuotaUsage | null;
  turnCount?: number;
  canCompact: boolean;
  compacting: boolean;
  onCompactContext: () => void;
  canClear: boolean;
  clearing: boolean;
  onOpenClearModal: () => void;
  canDecommission: boolean;
  decommissioning: boolean;
  onOpenDecommissionModal: () => void;
  apiClient?: HubApiClient;
  onAgentsUpdated?: () => void;
}

export const AgentSettingsModal: React.FC<AgentSettingsModalProps> = ({
  isOpen,
  onClose,
  agentName,
  agentDisplayName,
  agentRole,
  agentProject,
  adapter,
  sessionId,
  selectedModel,
  selectedThinkingLevel,
  availableModels,
  switchingModel = false,
  onChangeModelAndThinking,
  contextEstimate,
  quotaUsage = null,
  turnCount,
  canCompact,
  compacting,
  onCompactContext,
  canClear,
  clearing,
  onOpenClearModal,
  canDecommission,
  decommissioning,
  onOpenDecommissionModal,
  apiClient,
  onAgentsUpdated,
}) => {
  const { pinnedAgents } = usePinnedState();
  const [recastModalOpen, setRecastModalOpen] = useState(false);
  const [recastProject, setRecastProject] = useState(agentProject || "Workspace");
  const [recastRole, setRecastRole] = useState(agentRole || "assistant");
  const [recastCustomRoleInput, setRecastCustomRoleInput] = useState("");
  const [recastConflict, setRecastConflict] = useState<RecastConflictState | null>(null);
  const [recasting, setRecasting] = useState(false);
  const [recastError, setRecastError] = useState<string | null>(null);
  const [recastStatusBanner, setRecastStatusBanner] = useState<string | null>(null);

  useEffect(() => {
    setRecastProject(agentProject || "Workspace");
    setRecastRole(agentRole || "assistant");
    setRecastConflict(null);
    setRecastError(null);
    setRecastStatusBanner(null);
    setRecastModalOpen(false);
  }, [isOpen, agentName, agentProject, agentRole]);
  useEffect(() => {
    if (!isOpen) return;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        if (recastModalOpen) {
          if (!recasting) {
            setRecastModalOpen(false);
          }
          return;
        }
        onClose();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [isOpen, recastModalOpen, recasting, onClose]);

  if (!isOpen) {
    return null;
  }

  const displayTarget = agentDisplayName || agentName || sessionId || "Agent";
  const agentKey = (agentName || displayTarget).trim().toLowerCase();
  const targetSessionOrAgent = sessionId || agentName || "";
  const modelOptions = getVendorModelOptions(adapter, selectedModel, availableModels);
  const effectiveThinking = (selectedThinkingLevel || "high").trim().toLowerCase();

  const handleRecastSubmit = async (confirmOverrides = false) => {
    if (recasting || !agentKey) return;
    const effectiveRole = normalizeRecastRole(recastRole, recastCustomRoleInput);
    setRecasting(true);
    setRecastError(null);
    setRecastStatusBanner(null);
    try {
      const client = apiClient ?? new HubApiClient();
      const res = await client.recastAgent(agentKey, {
        project: recastProject,
        role: effectiveRole,
        replace_lead: confirmOverrides
          ? (recastConflict ? recastConflict.replaceLead : true)
          : false,
        create_area: confirmOverrides
          ? (recastConflict ? recastConflict.createArea : true)
          : false,
      });
      if (
        (res?.retires_lead ||
          (Array.isArray(res?.creates_areas) && res.creates_areas.length > 0)) &&
        !confirmOverrides
      ) {
        setRecastConflict(extractRecastConflict(res));
        return;
      }
      setRecastConflict(null);
      setRecastModalOpen(false);
      setRecastStatusBanner(
        `Re-cast ${displayTarget} to ${recastProject} as ${effectiveRole}.`,
      );
      onAgentsUpdated?.();
    } catch (err) {
      if (
        err instanceof HubApiError &&
        (err.status === 409 || err.status === 400) &&
        !confirmOverrides
      ) {
        setRecastConflict(extractRecastConflict(err));
      } else {
        setRecastError(
          err instanceof Error ? err.message : `Failed to re-cast ${displayTarget}`,
        );
      }
    } finally {
      setRecasting(false);
    }
  };
  const thinkingOptions: ReadonlyArray<{ value: string; label: string }> =
    effectiveThinking &&
    !THINKING_LEVEL_OPTIONS.some((opt) => opt.value === effectiveThinking)
      ? [{ value: effectiveThinking, label: effectiveThinking }, ...THINKING_LEVEL_OPTIONS]
      : THINKING_LEVEL_OPTIONS;
  const showQuota = hasQuotaUsageData(quotaUsage);
  const quotaExhausted = Boolean(
    quotaUsage &&
      ((quotaUsage.exhausted_reason && quotaUsage.exhausted_reason.trim().length > 0) ||
        (quotaUsage.primary_window?.used_percent ?? 0) >= 100 ||
        (quotaUsage.secondary_window?.used_percent ?? 0) >= 100),
  );

  return (
    <div className="fd-modal-backdrop" onClick={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        aria-label={`Agent Settings — ${displayTarget}`}
        data-testid="agent-settings-modal"
        className="fd-modal"
        onClick={(e) => e.stopPropagation()}
        style={{
          maxWidth: "520px",
          display: "flex",
          flexDirection: "column",
          gap: "14px",
        }}
      >
        {/* Header */}
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: "8px",
          }}
        >
          <div>
            <h2 style={{ margin: 0, fontSize: "16px", fontWeight: 700 }}>
              Agent Settings — {displayTarget}
            </h2>
            <p
              style={{
                margin: "3px 0 0 0",
                fontSize: "12px",
                color: "var(--text-secondary)",
              }}
            >
              Configure live model &amp; thinking level, inspect context &amp; quota telemetry, and
              manage session lifecycle.
            </p>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
            <button
              type="button"
              data-testid="agent-pin-toggle-btn"
              aria-pressed={pinnedAgents.has((agentName || displayTarget).trim().toLowerCase())}
              title={
                pinnedAgents.has((agentName || displayTarget).trim().toLowerCase())
                  ? "Unpin Agent"
                  : "Pin Agent"
              }
              aria-label={
                pinnedAgents.has((agentName || displayTarget).trim().toLowerCase())
                  ? "Unpin Agent"
                  : "Pin Agent"
              }
              className={`fd-btn${pinnedAgents.has((agentName || displayTarget).trim().toLowerCase()) ? " fd-btn-active" : ""}`}
              style={{
                padding: "4px 6px",
                display: "inline-flex",
                alignItems: "center",
                justifyContent: "center",
                lineHeight: 1,
              }}
              onClick={() => togglePinnedAgent(agentName || displayTarget)}
            >
              <PinIcon
                pinned={pinnedAgents.has((agentName || displayTarget).trim().toLowerCase())}
                circled={true}
                size={16}
              />
            </button>
            <button
              type="button"
              className="fd-btn"
              data-testid="close-agent-settings-btn"
              aria-label="Close Agent Settings"
              onClick={onClose}
            >
              ✕
            </button>
          </div>
        </div>

        {/* Agent Identity Summary */}
        <div
          data-testid="agent-settings-identity"
          className="fd-card"
          style={{
            padding: "10px 12px",
            display: "grid",
            gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
            gap: "8px",
            backgroundColor: "var(--bg-elevated)",
            fontSize: "12px",
          }}
        >
          <div>
            <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
              Persona
            </div>
            <div className="fd-mono" style={{ fontWeight: 600 }}>
              {displayTarget} {agentName ? `(@${agentName})` : ""}
            </div>
          </div>
          {agentProject && (
            <div>
              <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                Project
              </div>
              <div style={{ fontWeight: 600 }}>{agentProject}</div>
            </div>
          )}
          {agentRole && (
            <div>
              <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                Role
              </div>
              <div className="fd-mono">{agentRole}</div>
            </div>
          )}
          <div>
            <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
              Harness / Adapter
            </div>
            <div className="fd-mono">{adapter || "unknown"}</div>
          </div>
          <div>
            <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
              Session ID
            </div>
            <div
              style={{
                display: "flex",
                alignItems: "center",
                gap: "6px",
                minWidth: 0,
              }}
            >
              {sessionId && (
                <span
                  className="fd-badge fd-mono"
                  data-testid="session-id-pill"
                  title={sessionId}
                  style={{ flexShrink: 0 }}
                >
                  #{sessionId.slice(-8)}
                </span>
              )}
              <span
                className="fd-mono"
                title={sessionId || targetSessionOrAgent || "none"}
                style={{
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  whiteSpace: "nowrap",
                }}
              >
                {sessionId || targetSessionOrAgent || "unrouted"}
              </span>
            </div>
          </div>
          <div>
            <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
              Active Model
            </div>
            <span className="fd-badge fd-mono" data-testid="chat-active-model-badge">
              {adapter ? `${adapter} · ` : ""}
              {selectedModel}
              {selectedThinkingLevel ? ` (${selectedThinkingLevel})` : ""}
            </span>
          </div>
          {typeof turnCount === "number" && (
            <div>
              <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                Transcript Turns
              </div>
              <span className="fd-badge" data-testid="turn-count-badge">
                {turnCount} {turnCount === 1 ? "turn" : "turns"}
              </span>
            </div>
          )}
        </div>

        {/* Model & Thinking Level Selectors */}
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "1fr 1fr",
            gap: "10px",
          }}
        >
          <div>
            <label
              htmlFor="fd-agent-settings-model"
              style={{
                display: "block",
                fontSize: "11px",
                fontWeight: 600,
                color: "var(--text-muted)",
                marginBottom: "4px",
                textTransform: "uppercase",
              }}
            >
              Session Model
            </label>
            <select
              id="fd-agent-settings-model"
              aria-label="Session Model"
              data-testid="session-model-select"
              className="fd-select fd-mono"
              disabled={!targetSessionOrAgent || switchingModel}
              value={selectedModel}
              onChange={(e) => onChangeModelAndThinking(e.target.value, effectiveThinking)}
              style={{ width: "100%" }}
            >
              {modelOptions.map((opt) => (
                <option key={opt} value={opt}>
                  {opt}
                </option>
              ))}
            </select>
          </div>

          <div>
            <label
              htmlFor="fd-agent-settings-thinking"
              style={{
                display: "block",
                fontSize: "11px",
                fontWeight: 600,
                color: "var(--text-muted)",
                marginBottom: "4px",
                textTransform: "uppercase",
              }}
            >
              Thinking Level
            </label>
            <select
              id="fd-agent-settings-thinking"
              aria-label="Thinking Level"
              data-testid="session-thinking-select"
              className="fd-select fd-mono"
              disabled={!targetSessionOrAgent || switchingModel}
              value={effectiveThinking}
              onChange={(e) => onChangeModelAndThinking(selectedModel, e.target.value)}
              style={{ width: "100%" }}
            >
              {thinkingOptions.map((opt) => (
                <option key={opt.value} value={opt.value}>
                  {opt.label}
                </option>
              ))}
            </select>
          </div>
        </div>

        {/* Context Meter & Quota Summary */}
        <div
          className="fd-card"
          style={{
            padding: "10px 12px",
            display: "flex",
            flexDirection: "column",
            gap: "8px",
            backgroundColor: "var(--bg-elevated)",
          }}
        >
          <div
            data-testid="context-level-meter"
            style={{
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
              }}
            >
              <span
                style={{
                  fontSize: "11px",
                  fontWeight: 600,
                  textTransform: "uppercase",
                  color: "var(--text-muted)",
                }}
              >
                Context Window Usage
              </span>
              <span className="fd-mono" style={{ fontSize: "12px", color: "var(--text-primary)" }}>
                Context: {formatTokenCount(contextEstimate.usedTokens)} /{" "}
                {formatTokenCount(contextEstimate.maxTokens)} ({contextEstimate.percent}%)
              </span>
            </div>
            <div
              role="progressbar"
              aria-label="Context level"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={contextEstimate.percent}
              style={{
                width: "100%",
                height: "8px",
                borderRadius: "4px",
                backgroundColor: "var(--bg-input)",
                border: "1px solid var(--border-subtle)",
                overflow: "hidden",
              }}
            >
              <div
                style={{
                  width: `${contextEstimate.percent}%`,
                  height: "100%",
                  backgroundColor:
                    contextEstimate.percent >= 80
                      ? "var(--status-danger)"
                      : contextEstimate.percent >= 50
                        ? "var(--status-stale)"
                        : "var(--status-alive)",
                }}
              />
            </div>
          </div>

          {showQuota && quotaUsage && (
            <div
              data-testid="agent-settings-quota-summary"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "6px",
                paddingTop: "4px",
                borderTop: "1px solid var(--border-subtle)",
              }}
            >
              <span
                style={{
                  fontSize: "11px",
                  fontWeight: 600,
                  textTransform: "uppercase",
                  color: "var(--text-muted)",
                }}
              >
                Vendor Account Quota
              </span>
              <div
                data-testid="quota-usage-badge"
                data-exhausted={quotaExhausted ? "true" : undefined}
                className={`fd-badge fd-quota-badge${quotaExhausted ? " is-exhausted" : ""}`}
                title={
                  quotaUsage.exhausted_reason?.trim() ||
                  "Vendor account usage allowance"
                }
                style={{
                  display: "inline-flex",
                  alignItems: "center",
                  gap: "8px",
                  padding: "4px 8px",
                  flexWrap: "wrap",
                  borderColor: quotaExhausted
                    ? "var(--diff-del-border, rgba(239, 68, 68, 0.45))"
                    : "var(--border-subtle)",
                  backgroundColor: quotaExhausted
                    ? "var(--diff-del-bg, rgba(239, 68, 68, 0.14))"
                    : "var(--bg-surface)",
                  color: quotaExhausted ? "var(--diff-del-text, #FCA5A5)" : "var(--text-secondary)",
                }}
              >
                {quotaUsage.tier && quotaUsage.tier.trim().length > 0 && (
                  <span
                    data-testid="quota-tier-chip"
                    style={{
                      padding: "1px 5px",
                      borderRadius: "3px",
                      fontSize: "10px",
                      fontWeight: 700,
                      letterSpacing: "0.04em",
                      backgroundColor: quotaExhausted
                        ? "rgba(239, 68, 68, 0.2)"
                        : "var(--accent-subtle)",
                      color: quotaExhausted ? "var(--diff-del-text, #FCA5A5)" : "var(--accent-hover)",
                      border: `1px solid ${
                        quotaExhausted
                          ? "rgba(239, 68, 68, 0.45)"
                          : "var(--border-accent)"
                      }`,
                    }}
                  >
                    {quotaUsage.tier.trim().toUpperCase()}
                  </span>
                )}
                {quotaUsage.primary_window && (() => {
                  const win = quotaUsage.primary_window;
                  const remaining = formatQuotaWindowRemaining(win);
                  const resetLabel = formatQuotaResetCountdown(win.resets_at_ms);
                  const barColor =
                    win.used_percent >= 100 || remaining <= 10
                      ? "var(--status-danger)"
                      : remaining <= 25
                        ? "var(--status-stale)"
                        : "var(--status-alive)";
                  return (
                    <span
                      data-testid="quota-primary-window"
                      style={{ display: "inline-flex", alignItems: "center", gap: "5px", whiteSpace: "nowrap" }}
                    >
                      <span>5h: {remaining}% left</span>
                      <span
                        role="progressbar"
                        aria-label="5h session allowance remaining"
                        aria-valuemin={0}
                        aria-valuemax={100}
                        aria-valuenow={remaining}
                        style={{
                          width: "48px",
                          height: "6px",
                          borderRadius: "3px",
                          backgroundColor: "var(--bg-input)",
                          border: "1px solid var(--border-subtle)",
                          overflow: "hidden",
                          display: "inline-block",
                        }}
                      >
                        <span
                          style={{
                            display: "block",
                            width: `${remaining}%`,
                            height: "100%",
                            backgroundColor: barColor,
                          }}
                        />
                      </span>
                      {resetLabel && (
                        <span style={{ color: "var(--text-dim)", fontSize: "10.5px" }}>
                          ({resetLabel})
                        </span>
                      )}
                    </span>
                  );
                })()}
                {quotaUsage.secondary_window && (() => {
                  const win = quotaUsage.secondary_window;
                  const remaining = formatQuotaWindowRemaining(win);
                  const resetLabel = formatQuotaResetCountdown(win.resets_at_ms);
                  const barColor =
                    win.used_percent >= 100 || remaining <= 10
                      ? "var(--status-danger)"
                      : remaining <= 25
                        ? "var(--status-stale)"
                        : "var(--status-alive)";
                  return (
                    <span
                      data-testid="quota-secondary-window"
                      style={{ display: "inline-flex", alignItems: "center", gap: "5px", whiteSpace: "nowrap" }}
                    >
                      <span>7d: {remaining}% left</span>
                      <span
                        role="progressbar"
                        aria-label="Weekly allowance remaining"
                        aria-valuemin={0}
                        aria-valuemax={100}
                        aria-valuenow={remaining}
                        style={{
                          width: "48px",
                          height: "6px",
                          borderRadius: "3px",
                          backgroundColor: "var(--bg-input)",
                          border: "1px solid var(--border-subtle)",
                          overflow: "hidden",
                          display: "inline-block",
                        }}
                      >
                        <span
                          style={{
                            display: "block",
                            width: `${remaining}%`,
                            height: "100%",
                            backgroundColor: barColor,
                          }}
                        />
                      </span>
                      {resetLabel && (
                        <span style={{ color: "var(--text-dim)", fontSize: "10.5px" }}>
                          ({resetLabel})
                        </span>
                      )}
                    </span>
                  );
                })()}
                {quotaExhausted && (
                  <span
                    data-testid="quota-exhausted-alert"
                    style={{
                      fontWeight: 600,
                      color: "var(--diff-del-text, #FCA5A5)",
                      whiteSpace: "nowrap",
                    }}
                  >
                    {quotaUsage.exhausted_reason?.trim() || "Quota exhausted"}
                  </span>
                )}
              </div>
            </div>
          )}
        </div>

        {/* Session Lifecycle Actions */}
        <div>
          <div
            style={{
              fontSize: "11px",
              fontWeight: 600,
              color: "var(--text-muted)",
              marginBottom: "6px",
              textTransform: "uppercase",
            }}
          >
            Session Lifecycle Actions
          </div>
          <div
            style={{
              display: "flex",
              alignItems: "center",
              gap: "8px",
              flexWrap: "wrap",
            }}
          >
            <button
              type="button"
              className="fd-btn fd-btn-primary"
              data-testid={`recast-agent-btn-${agentKey}`}
              onClick={() => {
                setRecastProject(agentProject || "Workspace");
                setRecastRole(agentRole || "assistant");
                setRecastConflict(null);
                setRecastError(null);
                setRecastModalOpen(true);
              }}
            >
              Re-Cast
            </button>

            <button
              type="button"
              className={`fd-btn${canCompact ? " fd-btn-primary" : ""}`}
              data-testid="compact-context-btn"
              disabled={!canCompact}
              title={
                contextEstimate.percent >= 50
                  ? "Snapshot a FerroState checkpoint, then send /compact to the session"
                  : "Available when context reaches 50%"
              }
              onClick={onCompactContext}
            >
              {compacting ? "Compacting..." : "Compact"}
            </button>

            <button
              type="button"
              className="fd-btn"
              data-testid="open-clear-reforge-modal-btn"
              disabled={!canClear}
              title="Clear session context or reforge with registered agent identity"
              onClick={() => {
                onClose();
                onOpenClearModal();
              }}
            >
              {clearing ? "Clearing..." : "Clear / Reforge"}
            </button>

            <button
              type="button"
              className="fd-btn fd-btn-danger"
              data-testid="open-decommission-modal-btn"
              disabled={!canDecommission}
              title="Prepare graceful decommission handshake or tear down session immediately"
              onClick={() => {
                onClose();
                onOpenDecommissionModal();
              }}
            >
              {decommissioning ? "Decommissioning..." : "Decommission"}
            </button>
          </div>
        </div>

        {recastStatusBanner && (
          <div
            role="status"
            data-testid="recast-status-banner"
            className="fd-card"
            style={{
              borderColor: "var(--diff-add-border)",
              backgroundColor: "var(--diff-add-bg)",
              color: "var(--diff-add-text)",
              fontSize: "12px",
              padding: "8px 12px",
            }}
          >
            {recastStatusBanner}
          </div>
        )}

        {recastModalOpen && (() => {
          const projectOptions = CANONICAL_PROJECTS.includes(recastProject)
            ? CANONICAL_PROJECTS
            : [recastProject, ...CANONICAL_PROJECTS];
          const hasRoleOption =
            recastRole === CREATE_CUSTOM_ROLE_VALUE ||
            RECAST_ROLE_OPTIONS.some((opt) => opt.value === recastRole);

          return (
            <div
              className="fd-modal-backdrop"
              onClick={() => {
                if (!recasting) {
                  setRecastModalOpen(false);
                }
              }}
            >
              <div
                role="dialog"
                aria-modal="true"
                aria-label={`Re-Cast Agent — ${displayTarget}`}
                data-testid="recast-modal"
                className="fd-modal"
                onClick={(e) => e.stopPropagation()}
                style={{
                  maxWidth: "460px",
                  width: "92vw",
                  display: "flex",
                  flexDirection: "column",
                  gap: "12px",
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
                  <h3 style={{ margin: 0, fontSize: "15px", fontWeight: 700 }}>
                    Re-Cast Agent — {displayTarget}
                  </h3>
                  <button
                    type="button"
                    className="fd-btn"
                    aria-label="Close Re-Cast Modal"
                    disabled={recasting}
                    onClick={() => setRecastModalOpen(false)}
                  >
                    ✕
                  </button>
                </div>

                {recastError && (
                  <div
                    role="alert"
                    style={{
                      padding: "8px 10px",
                      borderRadius: "var(--radius-md)",
                      backgroundColor: "rgba(239, 68, 68, 0.14)",
                      border: "1px solid rgba(239, 68, 68, 0.45)",
                      color: "#FCA5A5",
                      fontSize: "12px",
                    }}
                  >
                    {recastError}
                  </div>
                )}

                <div style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
                  <div>
                    <label
                      htmlFor={`recast-project-select-${agentKey}`}
                      style={{
                        display: "block",
                        fontSize: "11px",
                        fontWeight: 600,
                        color: "var(--text-muted)",
                        marginBottom: "4px",
                        textTransform: "uppercase",
                      }}
                    >
                      Project
                    </label>
                    <select
                      id={`recast-project-select-${agentKey}`}
                      aria-label={`Project for ${displayTarget}`}
                      data-testid={`recast-project-select-${agentKey}`}
                      className="fd-select fd-mono"
                      style={{ width: "100%", fontSize: "12px", padding: "6px 8px" }}
                      value={recastProject}
                      onChange={(e) => {
                        setRecastProject(e.target.value);
                        setRecastConflict(null);
                      }}
                    >
                      {projectOptions.map((proj) => (
                        <option key={proj} value={proj}>
                          {proj}
                        </option>
                      ))}
                    </select>
                  </div>

                  <div>
                    <label
                      htmlFor={`recast-role-select-${agentKey}`}
                      style={{
                        display: "block",
                        fontSize: "11px",
                        fontWeight: 600,
                        color: "var(--text-muted)",
                        marginBottom: "4px",
                        textTransform: "uppercase",
                      }}
                    >
                      Role
                    </label>
                    <select
                      id={`recast-role-select-${agentKey}`}
                      aria-label={`Role for ${displayTarget}`}
                      data-testid={`recast-role-select-${agentKey}`}
                      className="fd-select fd-mono"
                      style={{ width: "100%", fontSize: "12px", padding: "6px 8px" }}
                      value={recastRole}
                      onChange={(e) => {
                        setRecastRole(e.target.value);
                        setRecastConflict(null);
                      }}
                    >
                      {!hasRoleOption && (
                        <option value={recastRole}>{recastRole}</option>
                      )}
                      {RECAST_ROLE_OPTIONS.map((opt) => (
                        <option key={opt.value} value={opt.value}>
                          {opt.label}
                        </option>
                      ))}
                      <option value={CREATE_CUSTOM_ROLE_VALUE}>
                        + Create custom role...
                      </option>
                    </select>
                  </div>

                  {recastRole === CREATE_CUSTOM_ROLE_VALUE && (
                    <div>
                      <label
                        htmlFor={`recast-custom-role-input-${agentKey}`}
                        style={{
                          display: "block",
                          fontSize: "11px",
                          fontWeight: 600,
                          color: "var(--text-muted)",
                          marginBottom: "4px",
                          textTransform: "uppercase",
                        }}
                      >
                        Custom Role Name
                      </label>
                      <input
                        id={`recast-custom-role-input-${agentKey}`}
                        type="text"
                        aria-label={`Custom role for ${displayTarget}`}
                        data-testid={`recast-custom-role-input-${agentKey}`}
                        className="fd-input fd-mono"
                        placeholder="e.g. reviewer or custom:reviewer"
                        style={{ width: "100%", fontSize: "12px", padding: "6px 8px" }}
                        value={recastCustomRoleInput}
                        onChange={(e) => setRecastCustomRoleInput(e.target.value)}
                      />
                    </div>
                  )}
                </div>

                {recastConflict && (
                  <div
                    data-testid={`recast-lead-warning-${agentKey}`}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: "8px",
                      padding: "8px 10px",
                      borderRadius: "4px",
                      backgroundColor: "rgba(245, 158, 11, 0.14)",
                      border: "1px solid rgba(245, 158, 11, 0.4)",
                      fontSize: "11.5px",
                    }}
                  >
                    <span>
                      {recastConflict.replaceLead
                        ? `Re-casting ${displayTarget} as Lead will replace the active Lead (${recastConflict.message}).`
                        : `Re-casting ${displayTarget} requires creating a new coverage area (${recastConflict.message}).`}
                    </span>
                    <button
                      type="button"
                      data-testid={`recast-confirm-lead-btn-${agentKey}`}
                      className="fd-btn fd-btn-danger"
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                      disabled={recasting}
                      onClick={() => void handleRecastSubmit(true)}
                    >
                      {recastConflict.replaceLead
                        ? "Confirm Replace Lead"
                        : "Confirm Create Area"}
                    </button>
                  </div>
                )}

                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "flex-end",
                    gap: "8px",
                  }}
                >
                  <button
                    type="button"
                    data-testid={`recast-cancel-btn-${agentKey}`}
                    className="fd-btn"
                    disabled={recasting}
                    onClick={() => setRecastModalOpen(false)}
                  >
                    Cancel
                  </button>
                  <button
                    type="button"
                    data-testid={`recast-submit-btn-${agentKey}`}
                    className="fd-btn fd-btn-primary"
                    disabled={recasting}
                    onClick={() => void handleRecastSubmit(false)}
                  >
                    {recasting ? "Re-Casting..." : "Confirm Re-Cast"}
                  </button>
                </div>
              </div>
            </div>
          );
        })()}
      </div>
    </div>
  );
};
