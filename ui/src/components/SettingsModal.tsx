import React, { useEffect, useState } from "react";
import { HubApiClient } from "../api/client";
import type {
  BulkSessionAction,
  BulkSessionActionResponse,
} from "../types/api";

export interface SettingsModalProps {
  isOpen: boolean;
  onClose: () => void;
  onBulkSessionAction?: (
    action: BulkSessionAction,
  ) => Promise<BulkSessionActionResponse>;
  apiClient?: HubApiClient;
}

const ACTION_LABELS: Record<BulkSessionAction, string> = {
  compact: "Compact All",
  clear: "Clear All",
  reforge: "Reforge All",
};

const ACTION_DESCRIPTIONS: Record<BulkSessionAction, string> = {
  compact:
    "Send /compact to every active session across the workspace fleet.",
  clear:
    "Clear conversation context across all active sessions, leaving each session blank.",
  reforge:
    "Clear conversation context and immediately re-prime every active session with its registered identity, project, and role prompt.",
};

export const SettingsModal: React.FC<SettingsModalProps> = ({
  isOpen,
  onClose,
  onBulkSessionAction,
  apiClient,
}) => {
  const [armedAction, setArmedAction] = useState<BulkSessionAction | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastResult, setLastResult] =
    useState<BulkSessionActionResponse | null>(null);

  useEffect(() => {
    setArmedAction(null);
    setError(null);
    setLastResult(null);
  }, [isOpen]);

  useEffect(() => {
    if (!isOpen) return;
    const handleKeyDownEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !submitting) {
        onClose();
      }
    };
    window.addEventListener("keydown", handleKeyDownEscape);
    return () => window.removeEventListener("keydown", handleKeyDownEscape);
  }, [isOpen, submitting, onClose]);

  if (!isOpen) {
    return null;
  }

  const handleConfirmBulkAction = async () => {
    if (!armedAction || submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      let res: BulkSessionActionResponse;
      if (onBulkSessionAction) {
        res = await onBulkSessionAction(armedAction);
      } else if (apiClient) {
        res = await apiClient.bulkSessionAction(armedAction);
      } else {
        const client = new HubApiClient();
        res = await client.bulkSessionAction(armedAction);
      }
      setLastResult(res);
      setArmedAction(null);
    } catch (err) {
      setError(
        err instanceof Error
          ? err.message
          : `Failed to execute bulk ${armedAction}`,
      );
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div
      className="fd-modal-backdrop"
      onClick={() => {
        if (!submitting) {
          onClose();
        }
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label="FerroDeck Settings"
        data-testid="settings-modal"
        className="fd-modal"
        onClick={(e) => e.stopPropagation()}
        style={{ maxWidth: "540px" }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            marginBottom: "14px",
          }}
        >
          <h2 style={{ margin: 0, fontSize: "16px", fontWeight: 700 }}>
            Settings &amp; Fleet Operations
          </h2>
          <button
            type="button"
            className="fd-btn"
            onClick={onClose}
            disabled={submitting}
            aria-label="Close Settings Modal"
          >
            ✕
          </button>
        </div>

        {error && (
          <div
            role="alert"
            style={{
              marginBottom: "12px",
              padding: "8px 10px",
              borderRadius: "var(--radius-md)",
              backgroundColor: "rgba(239, 68, 68, 0.14)",
              border: "1px solid rgba(239, 68, 68, 0.45)",
              color: "#FCA5A5",
              fontSize: "12px",
            }}
          >
            {error}
          </div>
        )}

        {lastResult && (
          <div
            role="status"
            data-testid="bulk-action-result"
            className="fd-card"
            style={{
              marginBottom: "12px",
              borderColor: "var(--diff-add-border)",
              backgroundColor: "var(--diff-add-bg)",
              color: "var(--diff-add-text)",
              fontSize: "12px",
            }}
          >
            <div>
              Completed <strong>{ACTION_LABELS[lastResult.action]}</strong>:{" "}
              {lastResult.succeeded} / {lastResult.total} sessions succeeded.
            </div>
            {lastResult.results.some((r) => !r.ok) && (
              <ul
                style={{
                  margin: "6px 0 0 0",
                  paddingLeft: "18px",
                  color: "#FCA5A5",
                }}
              >
                {lastResult.results
                  .filter((r) => !r.ok)
                  .map((r) => (
                    <li key={r.session_id}>
                      <code>{r.session_id}</code>:{" "}
                      {r.error || "Unknown failure"}
                    </li>
                  ))}
              </ul>
            )}
          </div>
        )}

        <div
          className="fd-card"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "12px",
            borderColor: "rgba(245, 158, 11, 0.35)",
          }}
        >
          <div>
            <h3
              style={{
                margin: "0 0 4px 0",
                fontSize: "13px",
                fontWeight: 700,
                textTransform: "uppercase",
                letterSpacing: "0.04em",
              }}
            >
              Fleet Bulk Session Operations
            </h3>
            <p
              style={{
                margin: 0,
                fontSize: "12px",
                color: "var(--text-secondary)",
              }}
            >
              Fleet-wide session context operations are isolated here and require
              explicit two-step confirmation before executing across all active
              agent sessions.
            </p>
          </div>

          <div style={{ display: "flex", gap: "8px", flexWrap: "wrap" }}>
            <button
              type="button"
              className={`fd-btn${armedAction === "compact" ? " fd-btn-active" : ""}`}
              data-testid="bulk-compact-btn"
              disabled={submitting}
              onClick={() => {
                setLastResult(null);
                setError(null);
                setArmedAction("compact");
              }}
            >
              Compact All
            </button>
            <button
              type="button"
              className="fd-btn fd-btn-danger"
              data-testid="bulk-clear-btn"
              disabled={submitting}
              onClick={() => {
                setLastResult(null);
                setError(null);
                setArmedAction("clear");
              }}
            >
              Clear All
            </button>
            <button
              type="button"
              className={`fd-btn fd-btn-primary${armedAction === "reforge" ? " fd-btn-active" : ""}`}
              data-testid="bulk-reforge-btn"
              disabled={submitting}
              onClick={() => {
                setLastResult(null);
                setError(null);
                setArmedAction("reforge");
              }}
            >
              Reforge All
            </button>
          </div>

          {armedAction && (
            <div
              data-testid="bulk-confirm-panel"
              className="fd-card"
              style={{
                backgroundColor: "var(--bg-elevated)",
                borderColor: "var(--border-accent)",
                display: "flex",
                flexDirection: "column",
                gap: "10px",
              }}
            >
              <div style={{ fontSize: "12px", color: "var(--text-primary)" }}>
                <strong>Confirm {ACTION_LABELS[armedAction]}:</strong>{" "}
                {ACTION_DESCRIPTIONS[armedAction]}
              </div>
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
                  className="fd-btn"
                  data-testid="bulk-cancel-btn"
                  disabled={submitting}
                  onClick={() => setArmedAction(null)}
                >
                  Cancel
                </button>
                <button
                  type="button"
                  className={
                    armedAction === "clear"
                      ? "fd-btn fd-btn-danger"
                      : "fd-btn fd-btn-primary"
                  }
                  data-testid="bulk-confirm-btn"
                  disabled={submitting}
                  onClick={() => void handleConfirmBulkAction()}
                >
                  {submitting
                    ? "Running..."
                    : `Confirm ${ACTION_LABELS[armedAction]}`}
                </button>
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
