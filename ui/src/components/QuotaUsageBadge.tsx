import React from "react";
import type { QuotaWindowUsage, SessionQuotaUsage } from "../types/api";
import { formatQuotaResetCountdown, formatQuotaWindowRemaining } from "./ChatPane";

/** True when the vendor reports the account out of allowance, or a window at 100%. */
export function isQuotaExhausted(quota?: SessionQuotaUsage | null): boolean {
  return Boolean(
    quota &&
      ((quota.exhausted_reason && quota.exhausted_reason.trim().length > 0) ||
        (quota.primary_window?.used_percent ?? 0) >= 100 ||
        (quota.secondary_window?.used_percent ?? 0) >= 100),
  );
}

interface QuotaWindowBarProps {
  win: QuotaWindowUsage;
  /** Short label before the percentage, such as `5h` or `7d`. */
  label: string;
  ariaLabel: string;
  testId: string;
}

function QuotaWindowBar({ win, label, ariaLabel, testId }: QuotaWindowBarProps) {
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
      data-testid={testId}
      style={{ display: "inline-flex", alignItems: "center", gap: "5px", whiteSpace: "nowrap" }}
    >
      <span>
        {label}: {remaining}%
      </span>
      <span
        role="progressbar"
        aria-label={ariaLabel}
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
        <span style={{ color: "var(--text-dim)", fontSize: "10.5px" }}>({resetLabel})</span>
      )}
    </span>
  );
}

/**
 * The vendor account allowance as one badge: tier chip, the 5h window and the 7d window each
 * with a bar and a reset countdown, and the exhausted reason. Shared by the agent settings
 * dialog and the Runtimes & Quotas tab.
 */
export function QuotaUsageBadge({ quota }: { quota: SessionQuotaUsage }) {
  const exhausted = isQuotaExhausted(quota);
  return (
    <div
      data-testid="quota-usage-badge"
      data-exhausted={exhausted ? "true" : undefined}
      className={`fd-badge fd-quota-badge${exhausted ? " is-exhausted" : ""}`}
      title={quota.exhausted_reason?.trim() || "Vendor account usage allowance"}
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: "8px",
        padding: "4px 8px",
        flexWrap: "wrap",
        minWidth: 0,
        borderColor: exhausted
          ? "var(--diff-del-border, rgba(239, 68, 68, 0.45))"
          : "var(--border-subtle)",
        backgroundColor: exhausted ? "var(--diff-del-bg, rgba(239, 68, 68, 0.14))" : "var(--bg-surface)",
        color: exhausted ? "var(--diff-del-text, #FCA5A5)" : "var(--text-secondary)",
      }}
    >
      {quota.tier && quota.tier.trim().length > 0 && (
        <span
          data-testid="quota-tier-chip"
          style={{
            padding: "1px 5px",
            borderRadius: "3px",
            fontSize: "10px",
            fontWeight: 700,
            letterSpacing: "0.04em",
            backgroundColor: exhausted ? "rgba(239, 68, 68, 0.2)" : "var(--accent-subtle)",
            color: exhausted ? "var(--diff-del-text, #FCA5A5)" : "var(--accent-hover)",
            border: `1px solid ${exhausted ? "rgba(239, 68, 68, 0.45)" : "var(--border-accent)"}`,
          }}
        >
          {quota.tier.trim().toUpperCase()}
        </span>
      )}
      {quota.primary_window && (
        <QuotaWindowBar
          win={quota.primary_window}
          label="5h"
          ariaLabel="5h session allowance remaining"
          testId="quota-primary-window"
        />
      )}
      {quota.secondary_window && (
        <QuotaWindowBar
          win={quota.secondary_window}
          label="7d"
          ariaLabel="Weekly allowance remaining"
          testId="quota-secondary-window"
        />
      )}
      {exhausted && (
        <span
          data-testid="quota-exhausted-alert"
          style={{
            fontWeight: 600,
            color: "var(--diff-del-text, #FCA5A5)",
            whiteSpace: "nowrap",
          }}
        >
          {quota.exhausted_reason?.trim() || "Quota exhausted"}
        </span>
      )}
    </div>
  );
}
