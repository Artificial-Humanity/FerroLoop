import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { HubApiClient } from "../api/client";
import type {
  CliRuntimeStatus,
  RuntimeMatrix,
  RuntimeUpdateResult,
} from "../types/api";
import { hasQuotaUsageData } from "./ChatPane";
import { QuotaUsageBadge } from "./QuotaUsageBadge";

/** Vendors in display order: the adapter id the hub keys `vendor_quotas` by, and its label. */
export const QUOTA_VENDORS: ReadonlyArray<{ adapter: string; label: string }> = [
  { adapter: "claude", label: "Claude" },
  { adapter: "codex", label: "Codex" },
  { adapter: "grok", label: "Grok" },
  { adapter: "antigravity", label: "Antigravity" },
  { adapter: "muse", label: "Muse" },
];

const SECTION_TITLE_STYLE: React.CSSProperties = {
  margin: 0,
  fontSize: "12.5px",
  fontWeight: 700,
  textTransform: "uppercase",
  letterSpacing: "0.04em",
};

const HINT_STYLE: React.CSSProperties = { fontSize: "11.5px", color: "var(--text-muted)" };

const ROW_STYLE: React.CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: "6px",
  padding: "8px 0",
  borderTop: "1px solid var(--border-subtle)",
};

const RESULT_LABELS: Partial<Record<RuntimeUpdateResult["status"], string>> = {
  unknown_version: "Updated (version unreadable)",
};

function resultLabel(status: RuntimeUpdateResult["status"]): string {
  return RESULT_LABELS[status] ?? status.replace("_", " ");
}

function resultTone(status: RuntimeUpdateResult["status"]): string {
  if (status === "updated") return "var(--status-alive)";
  if (
    status === "unchanged" ||
    status === "unknown_version" ||
    status === "unsupported" ||
    status === "not_installed"
  ) {
    return "var(--text-muted)";
  }
  return "var(--status-danger)";
}

/** Fold a (possibly partial) probe into the matrix already on screen. */
function mergeMatrix(prev: RuntimeMatrix | null, next: RuntimeMatrix): RuntimeMatrix {
  if (!prev) return next;
  const rows = new Map(next.cli_runtimes.map((row) => [row.name, row]));
  return {
    ...prev,
    generated_at_ms: next.generated_at_ms,
    cli_runtimes: prev.cli_runtimes.map((row) => rows.get(row.name) ?? row),
    vendor_quotas: { ...(prev.vendor_quotas ?? {}), ...(next.vendor_quotas ?? {}) },
  };
}

export interface RuntimesQuotasPanelProps {
  apiClient?: HubApiClient;
}

/**
 * Settings tab "Runtimes & Quotas": the vendor account allowances, and one row per subscription
 * CLI with its version, a re-check and a confirmed self-update. The hub answers all of it for the
 * owner only, so the caller mounts this panel for the owner only.
 */
export const RuntimesQuotasPanel: React.FC<RuntimesQuotasPanelProps> = ({ apiClient }) => {
  const client = useMemo(() => apiClient ?? new HubApiClient(), [apiClient]);
  const [matrix, setMatrix] = useState<RuntimeMatrix | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** The runtime being re-checked, or `null`. */
  const [checking, setChecking] = useState<string | null>(null);
  /** The runtime (or `all`) being updated, or `null`. */
  const [updating, setUpdating] = useState<string | null>(null);
  /** The runtime (or `all`) awaiting the owner's confirmation, or `null`. */
  const [armed, setArmed] = useState<string | null>(null);
  const [results, setResults] = useState<Record<string, RuntimeUpdateResult>>({});
  const [updateError, setUpdateError] = useState<string | null>(null);
  const alive = useRef(true);

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const busy = loading || checking !== null || updating !== null;

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const next = await client.fetchRuntimeMatrix();
      if (alive.current) setMatrix(next);
    } catch (err) {
      if (alive.current) {
        setError(err instanceof Error ? err.message : "Failed to load runtimes");
      }
    } finally {
      if (alive.current) setLoading(false);
    }
  }, [client]);

  useEffect(() => {
    void load();
  }, [load]);

  const check = async (name: string) => {
    setChecking(name);
    setError(null);
    try {
      const next = await client.probeRuntimes(name);
      if (alive.current) setMatrix((prev) => mergeMatrix(prev, next));
    } catch (err) {
      if (alive.current) {
        setError(err instanceof Error ? err.message : `Failed to check ${name}`);
      }
    } finally {
      if (alive.current) setChecking(null);
    }
  };

  const runUpdate = async (target: string) => {
    setArmed(null);
    setUpdating(target);
    setUpdateError(null);
    try {
      const outcome = await client.updateRuntime(target);
      if (!alive.current) return;
      setResults((prev) => {
        const next = { ...prev };
        for (const r of outcome) next[r.runtime] = r;
        return next;
      });
      // The hub dropped its probe cache, so this read shows the versions after the update.
      await load();
    } catch (err) {
      if (alive.current) {
        setUpdateError(err instanceof Error ? err.message : `Failed to update ${target}`);
      }
    } finally {
      if (alive.current) setUpdating(null);
    }
  };

  const canUpdate = (row: CliRuntimeStatus) => row.installed && row.update_supported === true;
  const updatable = (matrix?.cli_runtimes ?? []).filter(canUpdate);
  const quotas = matrix?.vendor_quotas ?? {};

  const renderRow = (row: CliRuntimeStatus) => {
    const result = results[row.name];
    const isArmed = armed === row.name;
    const command = row.update_command ?? `${row.name} update`;
    return (
      <div key={row.name} data-testid={`runtime-row-${row.name}`} style={ROW_STYLE}>
        <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
          <span
            className={`fd-dot ${row.installed ? "fd-dot-alive" : "fd-dot-dead"}`}
            aria-hidden="true"
          />
          <strong style={{ fontSize: "13px" }}>{row.name}</strong>
          <span data-testid={`runtime-version-${row.name}`} style={{ fontSize: "12px" }}>
            {row.installed ? (row.version ?? "version unknown") : "not installed"}
          </span>
          <span style={{ flex: 1 }} />
          <button
            type="button"
            className="fd-btn"
            data-testid={`runtime-check-${row.name}`}
            disabled={busy}
            onClick={() => void check(row.name)}
          >
            {checking === row.name ? "Checking…" : "Check"}
          </button>
          <button
            type="button"
            className="fd-btn"
            data-testid={`runtime-update-${row.name}`}
            disabled={busy || !canUpdate(row)}
            title={
              !row.update_supported
                ? `${row.name} has no update command`
                : !row.installed
                  ? `${row.name} is not installed`
                  : `Runs \`${command}\` on the hub`
            }
            onClick={() => setArmed(row.name)}
          >
            {updating === row.name || (updating === "all" && canUpdate(row))
              ? "Updating…"
              : "Update CLI"}
          </button>
        </div>
        {row.path && (
          <span style={{ ...HINT_STYLE, wordBreak: "break-all" }}>{row.path}</span>
        )}
        {row.error && (
          <span
            data-testid={`runtime-error-${row.name}`}
            style={{ fontSize: "11.5px", color: "var(--status-danger)" }}
          >
            {row.error}
          </span>
        )}
        {isArmed && (
          <div
            role="alertdialog"
            aria-label={`Confirm ${row.name} update`}
            data-testid={`runtime-update-confirm-${row.name}`}
            style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}
          >
            <span style={{ fontSize: "12px" }}>
              Run <code>{command}</code> on the hub now? This changes the installed CLI.
            </span>
            <button
              type="button"
              className="fd-btn fd-btn-primary"
              data-testid={`runtime-update-confirm-btn-${row.name}`}
              disabled={busy}
              onClick={() => void runUpdate(row.name)}
            >
              Confirm update
            </button>
            <button
              type="button"
              className="fd-btn"
              data-testid={`runtime-update-cancel-btn-${row.name}`}
              onClick={() => setArmed(null)}
            >
              Cancel
            </button>
          </div>
        )}
        {result && (
          <div data-testid={`runtime-update-result-${row.name}`} style={{ fontSize: "12px" }}>
            <span style={{ color: resultTone(result.status), fontWeight: 600 }}>
              {resultLabel(result.status)}
            </span>
            {(result.version_before || result.version_after) && (
              <span>
                {" "}
                {result.version_before ?? "?"} → {result.version_after ?? "?"}
              </span>
            )}
            {result.output_tail && (
              <pre
                data-testid={`runtime-update-output-${row.name}`}
                style={{
                  margin: "4px 0 0",
                  padding: "6px 8px",
                  maxHeight: "140px",
                  overflow: "auto",
                  whiteSpace: "pre-wrap",
                  wordBreak: "break-word",
                  fontSize: "11px",
                  borderRadius: "var(--radius-md)",
                  border: "1px solid var(--border-subtle)",
                  backgroundColor: "var(--bg-input)",
                }}
              >
                {result.output_tail}
              </pre>
            )}
          </div>
        )}
      </div>
    );
  };

  return (
    <div
      data-testid="settings-runtimes-panel"
      style={{ display: "flex", flexDirection: "column", gap: "12px" }}
    >
      {error && (
        <div
          role="alert"
          data-testid="runtimes-error"
          style={{ fontSize: "12px", color: "var(--status-danger)" }}
        >
          {error}
        </div>
      )}

      <div className="fd-card" style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
        <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
          <div style={{ flex: 1 }}>
            <h3 style={SECTION_TITLE_STYLE}>Vendor Account Quotas</h3>
            <span style={HINT_STYLE}>
              The allowance each vendor reports for the account the hub runs as.
            </span>
          </div>
          <button
            type="button"
            className="fd-btn"
            data-testid="runtimes-refresh-btn"
            disabled={busy}
            onClick={() => void load()}
          >
            {loading ? "Loading…" : "Refresh"}
          </button>
        </div>
        {QUOTA_VENDORS.map(({ adapter, label }) => {
          const quota = quotas[adapter];
          return (
            <div
              key={adapter}
              data-testid={`vendor-quota-${adapter}`}
              style={{ display: "flex", alignItems: "center", gap: "10px", flexWrap: "wrap" }}
            >
              <strong style={{ fontSize: "12.5px", minWidth: "84px" }}>{label}</strong>
              {quota && hasQuotaUsageData(quota) ? (
                <QuotaUsageBadge quota={quota} />
              ) : (
                <span style={HINT_STYLE}>{matrix ? "No allowance data" : "Loading…"}</span>
              )}
            </div>
          );
        })}
      </div>

      <div className="fd-card" style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
        <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
          <div style={{ flex: 1 }}>
            <h3 style={SECTION_TITLE_STYLE}>CLI Runtime Versions</h3>
            <span style={HINT_STYLE}>
              Check re-probes one CLI. Update CLI runs that CLI&apos;s own update command on the
              hub.
            </span>
          </div>
          <button
            type="button"
            className="fd-btn"
            data-testid="runtime-update-all"
            disabled={busy || updatable.length === 0}
            title={
              updatable.length === 0
                ? "No installed CLI has an update command"
                : `Updates ${updatable.map((r) => r.name).join(", ")}, one at a time`
            }
            onClick={() => setArmed("all")}
          >
            {updating === "all" ? "Updating…" : "Update all"}
          </button>
        </div>
        {armed === "all" && (
          <div
            role="alertdialog"
            aria-label="Confirm update of all CLIs"
            data-testid="runtime-update-confirm-all"
            style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}
          >
            <span style={{ fontSize: "12px" }}>
              Update {updatable.map((r) => r.name).join(", ")} on the hub now? This changes the
              installed CLIs.
            </span>
            <button
              type="button"
              className="fd-btn fd-btn-primary"
              data-testid="runtime-update-confirm-btn-all"
              disabled={busy}
              onClick={() => void runUpdate("all")}
            >
              Confirm update
            </button>
            <button
              type="button"
              className="fd-btn"
              data-testid="runtime-update-cancel-btn-all"
              onClick={() => setArmed(null)}
            >
              Cancel
            </button>
          </div>
        )}
        {updateError && (
          <div
            role="alert"
            data-testid="runtime-update-error"
            style={{ fontSize: "12px", color: "var(--status-danger)" }}
          >
            {updateError}
          </div>
        )}
        {matrix ? (
          matrix.cli_runtimes.map(renderRow)
        ) : (
          <span style={HINT_STYLE}>{loading ? "Loading runtimes…" : "No runtime data."}</span>
        )}
      </div>
    </div>
  );
};

export default RuntimesQuotasPanel;
