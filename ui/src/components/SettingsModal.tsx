import React, { useEffect, useState } from "react";
import { HubApiClient } from "../api/client";
import {
  DEFAULT_HUB_SETTINGS,
  type BulkSessionAction,
  type BulkSessionActionResponse,
  type HubSettings,
} from "../types/api";

export interface SettingsModalProps {
  isOpen: boolean;
  onClose: () => void;
  onBulkSessionAction?: (
    action: BulkSessionAction,
  ) => Promise<BulkSessionActionResponse>;
  onSettingsUpdated?: (settings: HubSettings) => void;
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
  onSettingsUpdated,
  apiClient,
}) => {
  const [settings, setSettings] = useState<HubSettings>(DEFAULT_HUB_SETTINGS);
  const [savingSettings, setSavingSettings] = useState(false);
  const [settingsSaved, setSettingsSaved] = useState(false);
  const [armedAction, setArmedAction] = useState<BulkSessionAction | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastResult, setLastResult] =
    useState<BulkSessionActionResponse | null>(null);

  useEffect(() => {
    setArmedAction(null);
    setError(null);
    setLastResult(null);
    setSettingsSaved(false);
    if (!isOpen) {
      return;
    }
    let cancelled = false;
    const client = apiClient ?? new HubApiClient();
    client
      .getSettings()
      .then((loaded) => {
        if (!cancelled) {
          setSettings(loaded);
        }
      })
      .catch(() => {
        // Keep DEFAULT_HUB_SETTINGS if endpoint is unreachable
      });
    return () => {
      cancelled = true;
    };
  }, [isOpen, apiClient]);

  useEffect(() => {
    if (!isOpen) return;
    const handleKeyDownEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !submitting && !savingSettings) {
        onClose();
      }
    };
    window.addEventListener("keydown", handleKeyDownEscape);
    return () => window.removeEventListener("keydown", handleKeyDownEscape);
  }, [isOpen, submitting, savingSettings, onClose]);

  if (!isOpen) {
    return null;
  }

  const handleSaveSettings = async () => {
    if (savingSettings) return;
    setSavingSettings(true);
    setError(null);
    setSettingsSaved(false);
    try {
      const client = apiClient ?? new HubApiClient();
      const updated = await client.updateSettings(settings);
      setSettings(updated);
      setSettingsSaved(true);
      onSettingsUpdated?.(updated);
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to save hub settings",
      );
    } finally {
      setSavingSettings(false);
    }
  };

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
        if (!submitting && !savingSettings) {
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
        style={{
          maxWidth: "600px",
          maxHeight: "88vh",
          overflowY: "auto",
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
          }}
        >
          <h2 style={{ margin: 0, fontSize: "16px", fontWeight: 700 }}>
            Settings &amp; Fleet Operations
          </h2>
          <button
            type="button"
            className="fd-btn"
            onClick={onClose}
            disabled={submitting || savingSettings}
            aria-label="Close Settings Modal"
          >
            ✕
          </button>
        </div>

        {error && (
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
            {error}
          </div>
        )}

        {settingsSaved && (
          <div
            role="status"
            data-testid="settings-saved-banner"
            className="fd-card"
            style={{
              borderColor: "var(--diff-add-border)",
              backgroundColor: "var(--diff-add-bg)",
              color: "var(--diff-add-text)",
              fontSize: "12px",
              padding: "8px 12px",
            }}
          >
            Settings saved successfully.
          </div>
        )}

        {lastResult && (
          <div
            role="status"
            data-testid="bulk-action-result"
            className="fd-card"
            style={{
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

        {/* Unified Settings Panel (Record 4) */}
        <div
          className="fd-card"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "12px",
          }}
        >
          {/* Appearance & Theme */}
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "12px",
              flexWrap: "wrap",
            }}
          >
            <div>
              <h3
                style={{
                  margin: 0,
                  fontSize: "12.5px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.04em",
                }}
              >
                Appearance &amp; Theme
              </h3>
              <span style={{ fontSize: "11.5px", color: "var(--text-muted)" }}>
                Choose the default color scheme for FerroDeck.
              </span>
            </div>
            <select
              aria-label="Theme Preference"
              data-testid="settings-theme-select"
              className="fd-select fd-mono"
              value={settings.theme_preference}
              onChange={(e) => {
                setSettingsSaved(false);
                setSettings((prev) => ({
                  ...prev,
                  theme_preference: e.target.value as HubSettings["theme_preference"],
                }));
              }}
              style={{ fontSize: "12px", padding: "5px 10px" }}
            >
              <option value="dark">Dark</option>
              <option value="light">Light</option>
              <option value="system">System</option>
            </select>
          </div>

          <hr style={{ border: "none", borderTop: "1px solid var(--border-subtle)", margin: 0 }} />

          {/* Subsystem Toggles */}
          <div style={{ display: "flex", flexDirection: "column", gap: "6px" }}>
            <h3
              style={{
                margin: 0,
                fontSize: "12.5px",
                fontWeight: 700,
                textTransform: "uppercase",
                letterSpacing: "0.04em",
              }}
            >
              Subsystem Toggles
            </h3>
            <div style={{ display: "flex", gap: "16px", flexWrap: "wrap", fontSize: "12.5px" }}>
              <label style={{ display: "inline-flex", alignItems: "center", gap: "6px", cursor: "pointer" }}>
                <input
                  type="checkbox"
                  data-testid="toggle-ferrogate"
                  checked={settings.subsystems.ferrogate_enabled}
                  onChange={(e) => {
                    setSettingsSaved(false);
                    setSettings((prev) => ({
                      ...prev,
                      subsystems: {
                        ...prev.subsystems,
                        ferrogate_enabled: e.target.checked,
                      },
                    }));
                  }}
                />
                <span>FerroGate Vault &amp; Proxy</span>
              </label>

              <label style={{ display: "inline-flex", alignItems: "center", gap: "6px", cursor: "pointer" }}>
                <input
                  type="checkbox"
                  data-testid="toggle-ferrostate"
                  checked={settings.subsystems.ferrostate_enabled}
                  onChange={(e) => {
                    setSettingsSaved(false);
                    setSettings((prev) => ({
                      ...prev,
                      subsystems: {
                        ...prev.subsystems,
                        ferrostate_enabled: e.target.checked,
                      },
                    }));
                  }}
                />
                <span>FerroState Memory &amp; Index</span>
              </label>

              <label style={{ display: "inline-flex", alignItems: "center", gap: "6px", cursor: "pointer" }}>
                <input
                  type="checkbox"
                  data-testid="toggle-ferrostep"
                  checked={settings.subsystems.ferrostep_enabled}
                  onChange={(e) => {
                    setSettingsSaved(false);
                    setSettings((prev) => ({
                      ...prev,
                      subsystems: {
                        ...prev.subsystems,
                        ferrostep_enabled: e.target.checked,
                      },
                    }));
                  }}
                />
                <span>FerroStep Workflows</span>
              </label>
            </div>
          </div>

          <hr style={{ border: "none", borderTop: "1px solid var(--border-subtle)", margin: 0 }} />

          {/* Notification Rules */}
          <div style={{ display: "flex", flexDirection: "column", gap: "6px" }}>
            <h3
              style={{
                margin: 0,
                fontSize: "12.5px",
                fontWeight: 700,
                textTransform: "uppercase",
                letterSpacing: "0.04em",
              }}
            >
              Notification Rules
            </h3>
            <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(210px, 1fr))", gap: "6px", fontSize: "12.5px" }}>
              <label style={{ display: "inline-flex", alignItems: "center", gap: "6px", cursor: "pointer" }}>
                <input
                  type="checkbox"
                  data-testid="toggle-notify-pr"
                  checked={settings.notifications.notify_on_pr_review}
                  onChange={(e) => {
                    setSettingsSaved(false);
                    setSettings((prev) => ({
                      ...prev,
                      notifications: {
                        ...prev.notifications,
                        notify_on_pr_review: e.target.checked,
                      },
                    }));
                  }}
                />
                <span>PR Review Requests</span>
              </label>

              <label style={{ display: "inline-flex", alignItems: "center", gap: "6px", cursor: "pointer" }}>
                <input
                  type="checkbox"
                  data-testid="toggle-notify-error"
                  checked={settings.notifications.notify_on_session_error}
                  onChange={(e) => {
                    setSettingsSaved(false);
                    setSettings((prev) => ({
                      ...prev,
                      notifications: {
                        ...prev.notifications,
                        notify_on_session_error: e.target.checked,
                      },
                    }));
                  }}
                />
                <span>Session Errors</span>
              </label>

              <label style={{ display: "inline-flex", alignItems: "center", gap: "6px", cursor: "pointer" }}>
                <input
                  type="checkbox"
                  data-testid="toggle-notify-wire"
                  checked={settings.notifications.notify_on_wire_urgent}
                  onChange={(e) => {
                    setSettingsSaved(false);
                    setSettings((prev) => ({
                      ...prev,
                      notifications: {
                        ...prev.notifications,
                        notify_on_wire_urgent: e.target.checked,
                      },
                    }));
                  }}
                />
                <span>Urgent FerroWire Mail</span>
              </label>

              <label style={{ display: "inline-flex", alignItems: "center", gap: "6px", cursor: "pointer" }}>
                <input
                  type="checkbox"
                  data-testid="toggle-notify-sound"
                  checked={settings.notifications.sound_enabled}
                  onChange={(e) => {
                    setSettingsSaved(false);
                    setSettings((prev) => ({
                      ...prev,
                      notifications: {
                        ...prev.notifications,
                        sound_enabled: e.target.checked,
                      },
                    }));
                  }}
                />
                <span>Sound Alerts</span>
              </label>
            </div>
          </div>

          <hr style={{ border: "none", borderTop: "1px solid var(--border-subtle)", margin: 0 }} />

          {/* Hub & Polling Connection */}
          <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
            <h3
              style={{
                margin: 0,
                fontSize: "12.5px",
                fontWeight: 700,
                textTransform: "uppercase",
                letterSpacing: "0.04em",
              }}
            >
              Hub &amp; Polling Connection
            </h3>
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "12px",
                flexWrap: "wrap",
                fontSize: "12.5px",
              }}
            >
              <label style={{ display: "inline-flex", alignItems: "center", gap: "6px", cursor: "pointer" }}>
                <input
                  type="checkbox"
                  data-testid="toggle-auto-reconnect"
                  checked={settings.hub_connection.auto_reconnect_ws}
                  onChange={(e) => {
                    setSettingsSaved(false);
                    setSettings((prev) => ({
                      ...prev,
                      hub_connection: {
                        ...prev.hub_connection,
                        auto_reconnect_ws: e.target.checked,
                      },
                    }));
                  }}
                />
                <span>Auto-reconnect WebSocket</span>
              </label>

              <label style={{ display: "inline-flex", alignItems: "center", gap: "6px" }}>
                <span>Poll Interval (secs):</span>
                <input
                  type="number"
                  min={1}
                  max={60}
                  aria-label="Poll Interval (secs)"
                  data-testid="input-poll-interval"
                  className="fd-input fd-mono"
                  value={settings.hub_connection.poll_interval_secs}
                  onChange={(e) => {
                    const parsed = Number(e.target.value);
                    setSettingsSaved(false);
                    setSettings((prev) => ({
                      ...prev,
                      hub_connection: {
                        ...prev.hub_connection,
                        poll_interval_secs:
                          Number.isFinite(parsed) && parsed > 0 ? parsed : 1,
                      },
                    }));
                  }}
                  style={{ width: "68px", fontSize: "12px", padding: "4px 8px" }}
                />
              </label>
            </div>
          </div>

          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <button
              type="button"
              data-testid="save-settings-btn"
              className="fd-btn fd-btn-primary"
              disabled={savingSettings}
              onClick={() => void handleSaveSettings()}
            >
              {savingSettings ? "Saving..." : "Save Settings"}
            </button>
          </div>
        </div>

        {/* Fleet Bulk Session Operations */}
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
