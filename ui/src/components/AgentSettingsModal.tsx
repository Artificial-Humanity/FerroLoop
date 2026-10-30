import React, { useEffect } from "react";
import type { SessionQuotaUsage } from "../types/api";
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
  canCompact: boolean;
  compacting: boolean;
  onCompactContext: () => void;
  canClear: boolean;
  clearing: boolean;
  onOpenClearModal: () => void;
  canDecommission: boolean;
  decommissioning: boolean;
  onOpenDecommissionModal: () => void;
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
  canCompact,
  compacting,
  onCompactContext,
  canClear,
  clearing,
  onOpenClearModal,
  canDecommission,
  decommissioning,
  onOpenDecommissionModal,
}) => {
  useEffect(() => {
    if (!isOpen) return;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        onClose();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [isOpen, onClose]);

  if (!isOpen) {
    return null;
  }

  const displayTarget = agentDisplayName || agentName || sessionId || "Agent";
  const targetSessionOrAgent = sessionId || agentName || "";
  const modelOptions = getVendorModelOptions(adapter, selectedModel, availableModels);
  const effectiveThinking = (selectedThinkingLevel || "high").trim().toLowerCase();
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
              className="fd-mono"
              title={sessionId || targetSessionOrAgent || "none"}
              style={{
                overflow: "hidden",
                textOverflow: "ellipsis",
                whiteSpace: "nowrap",
              }}
            >
              {sessionId || targetSessionOrAgent || "unrouted"}
            </div>
          </div>
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
              {formatTokenCount(contextEstimate.usedTokens)} /{" "}
              {formatTokenCount(contextEstimate.maxTokens)} ({contextEstimate.percent}%)
            </span>
          </div>
          <div
            role="progressbar"
            aria-label="Modal context level"
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

          {showQuota && quotaUsage && (
            <div
              data-testid="agent-settings-quota-summary"
              data-exhausted={quotaExhausted ? "true" : "false"}
              className={`fd-quota-badge${quotaExhausted ? " is-exhausted" : ""}`}
              title={
                quotaUsage.exhausted_reason && quotaUsage.exhausted_reason.trim().length > 0
                  ? quotaUsage.exhausted_reason.trim()
                  : "Vendor account allowance and renewal window"
              }
              style={{
                display: "flex",
                alignItems: "center",
                gap: "8px",
                flexWrap: "wrap",
                paddingTop: "4px",
                fontSize: "11.5px",
                color: quotaExhausted ? "var(--diff-del-text, #FCA5A5)" : "var(--text-secondary)",
              }}
            >
              {quotaUsage.tier && quotaUsage.tier.trim().length > 0 && (
                <span className="fd-badge fd-mono">
                  {quotaUsage.tier.trim().toUpperCase()}
                </span>
              )}
              {quotaUsage.primary_window && (() => {
                const win = quotaUsage.primary_window;
                const remaining = formatQuotaWindowRemaining(win);
                const resetLabel = formatQuotaResetCountdown(win.resets_at_ms);
                return (
                  <span className="fd-mono">
                    5h: {remaining}% left {resetLabel ? `(${resetLabel})` : ""}
                  </span>
                );
              })()}
              {quotaUsage.secondary_window && (() => {
                const win = quotaUsage.secondary_window;
                const remaining = formatQuotaWindowRemaining(win);
                const resetLabel = formatQuotaResetCountdown(win.resets_at_ms);
                return (
                  <span className="fd-mono">
                    7d: {remaining}% left {resetLabel ? `(${resetLabel})` : ""}
                  </span>
                );
              })()}
              {quotaExhausted && (
                <span style={{ fontWeight: 600, color: "var(--diff-del-text, #FCA5A5)" }}>
                  {quotaUsage.exhausted_reason?.trim() || "Quota exhausted"}
                </span>
              )}
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
      </div>
    </div>
  );
};
