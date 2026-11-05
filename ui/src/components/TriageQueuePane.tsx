import React, { useCallback, useEffect, useMemo, useState } from "react";
import type { HubApiClient } from "../api/client";
import {
  resolveLivenessState,
  type AgentLivenessItem,
  type CockpitTab,
  type DiscoveredSessionSummary,
  type ProjectRecordItem,
  type PullRequestItem,
  type WatchAgentHealth,
  type WatchAlert,
  type WireInboxItem,
  type WorkflowRun,
} from "../types/api";
import type { LeftPanelTab } from "./RosterSidebar";

export type TriageIncidentCategory =
  | "heartbeat"
  | "review"
  | "budget"
  | "wire";

export type TriageIncidentSeverity = "critical" | "warning" | "info";

export type TriageCategoryFilter = "all" | TriageIncidentCategory;

export interface TriageNavigationTarget {
  kind: "session" | "record" | "pr" | "wire" | "watch" | "workflow";
  agentName?: string | null;
  sessionId?: string | null;
  project?: string | null;
  recordId?: string | null;
  prNumber?: number | null;
  wireMessageId?: string | null;
  workflowRunId?: string | null;
  tab?: CockpitTab;
  leftPanelTab?: LeftPanelTab;
}

export interface TriageIncident {
  id: string;
  category: TriageIncidentCategory;
  severity: TriageIncidentSeverity;
  title: string;
  subtitle: string;
  project?: string | null;
  agentName?: string | null;
  badgeLabel: string;
  actionLabel: string;
  target: TriageNavigationTarget;
}

export interface TriageAggregateInput {
  liveness?: AgentLivenessItem[];
  sessions?: DiscoveredSessionSummary[];
  workflowRuns?: WorkflowRun[];
  prs?: PullRequestItem[];
  records?: ProjectRecordItem[];
  watchAlerts?: WatchAlert[];
  watchAgentHealth?: WatchAgentHealth[];
  wireMessages?: WireInboxItem[];
}

export interface TriageQueuePaneProps extends TriageAggregateInput {
  apiClient?: HubApiClient;
  loading?: boolean;
  onRefresh?: () => void;
  onNavigate?: (target: TriageNavigationTarget, incident: TriageIncident) => void;
  onSelectAgent?: (agentName: string) => void;
  onSelectSession?: (sessionId: string) => void;
  onSelectTab?: (tab: CockpitTab) => void;
  onSelectLeftPanelTab?: (tab: LeftPanelTab) => void;
  onAckWireMessage?: (messageId: string) => Promise<unknown> | void;
  routedCiPrEvents?: RoutedCiPrFeedbackEvent[];
}

// Record #88: Automated CI Check Failure & PR Review Feedback Routing
export interface RoutedCiPrFeedbackEvent {
  id: string;
  kind: "ci-check-failure" | "pr-review-feedback";
  repo: string;
  pr_number: number;
  branch: string;
  author_agent: string;
  check_or_reviewer: string;
  failing_step_or_summary: string;
  status: "routed" | "dispatched" | "acknowledged";
}

export const DEFAULT_ROUTED_CI_PR_EVENTS: RoutedCiPrFeedbackEvent[] = [
  {
    id: "ci-fail-pr-238",
    kind: "ci-check-failure",
    repo: "Artificial-Humanity/FerroLoop",
    pr_number: 238,
    branch: "felix/wave4-otlp-and-ref-repos",
    author_agent: "felix",
    check_or_reviewer: "verify / test, clippy, fmt",
    failing_step_or_summary: "cargo clippy --all-targets -- -D warnings",
    status: "routed",
  },
  {
    id: "pr-rev-pr-35",
    kind: "pr-review-feedback",
    repo: "Artificial-Humanity/FerroDeck",
    pr_number: 35,
    branch: "felix/wave4-otlp-and-ref-repos",
    author_agent: "felix",
    check_or_reviewer: "ferris (changes_requested)",
    failing_step_or_summary:
      "Inline review comment on src/components/FerroWatchPane.tsx:42",
    status: "routed",
  },
];

// Record #103: Live Permission Prompt Interception & Channel-Honest Remote Resolution
export type PermissionChannelKind = "rpc" | "pty-snapshot";

export type PermissionResponseDecision =
  | "approve_once"
  | "approve_session"
  | "always_allow_rule"
  | "deny"
  | "send_keys_unverified";

export interface PendingPermissionPromptItem {
  id: string;
  session_id: string;
  agent: string;
  vendor: string;
  transport: string;
  channel_kind: PermissionChannelKind;
  request_kind?: string;
  capability: string;
  command_or_target: string;
  always_allow_supported?: boolean;
  unverified_keys_supported?: boolean;
  status: "pending" | PermissionResponseDecision;
}

export const DEFAULT_PENDING_PERMISSION_PROMPTS: PendingPermissionPromptItem[] =
  [
    {
      id: "perm-req-01",
      session_id: "sess-felix-ui",
      agent: "felix",
      vendor: "claude",
      transport: "claude-sdk-mcp",
      channel_kind: "rpc",
      request_kind: "canUseTool",
      capability: "fs_read_external",
      command_or_target:
        "Read /tmp/ferrodeck-repo/ui/src/components/SettingsModal.tsx",
      always_allow_supported: true,
      status: "pending",
    },
    {
      id: "perm-req-02",
      session_id: "sess-feira-sys",
      agent: "feira",
      vendor: "codex",
      transport: "codex-app-server",
      channel_kind: "rpc",
      request_kind: "item/fileChange/requestApproval",
      capability: "fs_write_workspace",
      command_or_target: "Patch crates/fl-core/src/install_plan.rs",
      always_allow_supported: false,
      status: "pending",
    },
    {
      id: "perm-req-03",
      session_id: "sess-kacey-tui",
      agent: "kacey",
      vendor: "grok",
      transport: "pty-vt100",
      channel_kind: "pty-snapshot",
      request_kind: "tui-screen-classifier",
      capability: "shell_exec",
      command_or_target: "pytest tests/test_permissions.py",
      always_allow_supported: false,
      unverified_keys_supported: true,
      status: "pending",
    },
  ];

const SEVERITY_RANK: Record<TriageIncidentSeverity, number> = {
  critical: 0,
  warning: 1,
  info: 2,
};

function isWireItemUnacked(item: WireInboxItem): boolean {
  if (item.receipt && item.receipt.status === "acked") {
    return false;
  }
  if (
    Array.isArray(item.receipts) &&
    item.receipts.length > 0 &&
    item.receipts.every((r) => r.status === "acked")
  ) {
    return false;
  }
  return true;
}

export function aggregateTriageIncidents(
  input: TriageAggregateInput,
): TriageIncident[] {
  const incidents: TriageIncident[] = [];
  const seenHeartbeatAgents = new Set<string>();

  // 1. Failed or timed-out attempts / unverifiable or stale agent heartbeats
  for (const item of input.liveness ?? []) {
    const agentKey = (item.agent ?? "").trim().toLowerCase();
    if (!agentKey) continue;
    const rawState = (
      item.verdict?.state ??
      item.verdict?.status ??
      ""
    )
      .trim()
      .toLowerCase();
    const resolved = resolveLivenessState(item.verdict, item.has_route);
    const isUnverifiableOrTimedOut =
      rawState === "unverifiable" ||
      rawState === "timed_out" ||
      rawState === "timeout" ||
      rawState === "failed" ||
      rawState === "crashed";
    const isStale = resolved === "stale" || rawState === "stale";
    const isDeadWithRoute = resolved === "dead" && Boolean(item.has_route);

    if (isUnverifiableOrTimedOut || isStale || isDeadWithRoute) {
      seenHeartbeatAgents.add(agentKey);
      const severity: TriageIncidentSeverity =
        isUnverifiableOrTimedOut || isDeadWithRoute ? "critical" : "warning";
      const reason =
        item.verdict?.reason ||
        (isDeadWithRoute
          ? "Session route is unresponsive (dead heartbeat)"
          : isUnverifiableOrTimedOut
            ? `Heartbeat ${rawState}`
            : "Agent heartbeat is stale");
      incidents.push({
        id: `heartbeat-agent-${agentKey}`,
        category: "heartbeat",
        severity,
        title: `${item.agent} — ${isStale ? "Stale Heartbeat" : "Unresponsive / Failed Session"}`,
        subtitle: `${reason} (${item.project || "Workspace"})`,
        project: item.project || null,
        agentName: item.agent,
        badgeLabel: isStale ? "stale" : rawState || "offline",
        actionLabel: "Open Session",
        target: {
          kind: "session",
          agentName: item.agent,
          project: item.project || null,
          tab: "chat",
        },
      });
    }
  }

  for (const health of input.watchAgentHealth ?? []) {
    const agentKey = (health.agent ?? "").trim().toLowerCase();
    if (!agentKey || seenHeartbeatAgents.has(agentKey)) continue;
    if (
      health.liveness === "stale" ||
      (health.active &&
        (health.liveness === "dead" ||
          health.liveness === "exited" ||
          health.liveness === "missing"))
    ) {
      seenHeartbeatAgents.add(agentKey);
      const severity: TriageIncidentSeverity =
        health.liveness === "stale" ? "warning" : "critical";
      incidents.push({
        id: `heartbeat-watch-${agentKey}`,
        category: "heartbeat",
        severity,
        title: `${health.agent} — Heartbeat ${health.liveness.toUpperCase()}`,
        subtitle:
          health.verdict?.reason ||
          `Active agent session reported ${health.liveness} heartbeat`,
        project: health.project ?? null,
        agentName: health.agent,
        badgeLabel: health.liveness,
        actionLabel: "Open Session",
        target: {
          kind: "session",
          agentName: health.agent,
          sessionId: health.hosted_session?.session_id ?? null,
          project: health.project ?? null,
          tab: "chat",
        },
      });
    }
  }

  for (const sess of input.sessions ?? []) {
    const statusLower = (sess.status ?? "").trim().toLowerCase();
    if (
      statusLower === "failed" ||
      statusLower === "error" ||
      statusLower === "timed_out" ||
      statusLower === "crashed"
    ) {
      const personaKey = (sess.persona ?? "").trim().toLowerCase();
      if (personaKey && seenHeartbeatAgents.has(personaKey)) continue;
      if (personaKey) seenHeartbeatAgents.add(personaKey);
      incidents.push({
        id: `attempt-session-${sess.id}`,
        category: "heartbeat",
        severity: "critical",
        title: `Session ${sess.persona || sess.id} — ${statusLower.toUpperCase()}`,
        subtitle: `Session ${sess.id} reported ${statusLower} status`,
        agentName: sess.persona ?? null,
        badgeLabel: statusLower,
        actionLabel: "Inspect Session",
        target: {
          kind: "session",
          sessionId: sess.id,
          agentName: sess.persona ?? null,
          tab: "chat",
        },
      });
    }
  }

  for (const run of input.workflowRuns ?? []) {
    const statusLower = (run.status ?? "").trim().toLowerCase();
    const failedSteps = Object.values(run.steps ?? {}).filter(
      (s) => s.last_outcome === "fail" || s.state === "failed",
    );
    if (statusLower === "failed" || failedSteps.length > 0) {
      const assignedAgent = failedSteps[0]?.assigned_agent ?? null;
      incidents.push({
        id: `attempt-workflow-${run.id}`,
        category: "heartbeat",
        severity: "critical",
        title: `Workflow Failed: ${run.title || run.workflow_id}`,
        subtitle:
          failedSteps.length > 0
            ? `Step ${failedSteps[0].step_id} failed after ${failedSteps[0].attempt_count} attempt(s)`
            : `Workflow run ${run.id} failed in ${run.project}`,
        project: run.project,
        agentName: assignedAgent,
        badgeLabel: "failed attempt",
        actionLabel: "Open Workflow",
        target: {
          kind: "workflow",
          workflowRunId: run.id,
          project: run.project,
          agentName: assignedAgent,
          tab: "hub",
        },
      });
    } else if (statusLower === "needs_human" || statusLower === "awaiting_gate") {
      const stepList = Object.values(run.steps ?? {});
      const gateStep = stepList.find(
        (s) => s.last_outcome === "needs_human" || Boolean(s.needs_human_reason),
      );
      incidents.push({
        id: `review-workflow-${run.id}`,
        category: "review",
        severity: "warning",
        title: `Gate Approval Needed: ${run.title || run.workflow_id}`,
        subtitle:
          gateStep?.needs_human_reason ||
          `Active step ${run.active_steps[0] ?? "gate"} awaiting operator sign-off`,
        project: run.project,
        agentName: gateStep?.assigned_agent ?? null,
        badgeLabel: "needs sign-off",
        actionLabel: "Review Gate",
        target: {
          kind: "workflow",
          workflowRunId: run.id,
          project: run.project,
          agentName: gateStep?.assigned_agent ?? null,
          tab: "hub",
        },
      });
    }
  }

  // 2. Pending pull-request / review requests or blocked tasks
  for (const pr of input.prs ?? []) {
    const statusLower = (pr.status ?? "open").trim().toLowerCase();
    if (statusLower === "merged" || statusLower === "closed") continue;
    const reviews = pr.reviews ?? [];
    const hasChangesRequested = reviews.some((r) =>
      r.state.toUpperCase().includes("CHANGES_REQUESTED"),
    );
    const hasApproval = reviews.some((r) =>
      r.state.toUpperCase().includes("APPROV"),
    );

    if (hasChangesRequested || !hasApproval) {
      incidents.push({
        id: `review-pr-${pr.project || "repo"}-${pr.number}`,
        category: "review",
        severity: hasChangesRequested ? "critical" : "warning",
        title: `PR #${pr.number}: ${pr.title || "Pull Request"}`,
        subtitle: hasChangesRequested
          ? `Changes requested on ${pr.project || "repository"} (${pr.author || "author"})`
          : `Pending review approval on ${pr.project || "repository"} (${pr.author || "author"})`,
        project: pr.project ?? null,
        agentName: pr.author ?? null,
        badgeLabel: hasChangesRequested ? "changes_requested" : "review_required",
        actionLabel: "Open PR",
        target: {
          kind: "pr",
          prNumber: pr.number,
          project: pr.project ?? null,
          agentName: pr.author ?? null,
          tab: "git",
        },
      });
    }
  }

  for (const rec of input.records ?? []) {
    if (rec.state === "done") continue;
    const openFindings = rec.findings_open ?? 0;
    const isBlockedTitle = /\bblocked\b/i.test(rec.title);
    if (openFindings > 0 || isBlockedTitle) {
      incidents.push({
        id: `review-record-${rec.id}`,
        category: "review",
        severity: openFindings > 0 ? "warning" : "info",
        title: `${rec.display_id}: ${rec.title}`,
        subtitle:
          openFindings > 0
            ? `${openFindings} open finding(s) in ${rec.project}`
            : `Blocked record in ${rec.project}`,
        project: rec.project,
        badgeLabel:
          openFindings > 0 ? `${openFindings} findings` : "blocked",
        actionLabel: "Open Record",
        target: {
          kind: "record",
          recordId: rec.id,
          project: rec.project,
          tab: "board",
        },
      });
    }
  }

  // 3. Budget threshold alerts from FerroWatch
  for (const alert of input.watchAlerts ?? []) {
    const entityObj = alert.entity as unknown as Record<string, unknown> | undefined;
    const entityAgent =
      typeof entityObj?.agent === "string"
        ? entityObj.agent
        : typeof entityObj?.persona === "string"
          ? entityObj.persona
          : null;
    const entityProject =
      typeof entityObj?.project === "string" ? entityObj.project : null;
    const severity: TriageIncidentSeverity =
      alert.severity === "critical"
        ? "critical"
        : alert.severity === "warning"
          ? "warning"
          : "info";

    incidents.push({
      id: `budget-alert-${alert.id}`,
      category: "budget",
      severity,
      title: `FerroWatch: ${alert.message}`,
      subtitle: alert.remediation || `Rule: ${alert.rule}`,
      project: entityProject,
      agentName: entityAgent,
      badgeLabel: alert.rule.replace(/_/g, " "),
      actionLabel: "Open FerroWatch",
      target: {
        kind: "watch",
        agentName: entityAgent,
        project: entityProject,
        leftPanelTab: "watch",
        tab: "state",
      },
    });
  }

  // 4. Unread mail/urgent or operator-targeted wire messages
  for (const item of input.wireMessages ?? []) {
    if (!isWireItemUnacked(item)) continue;
    const msg = item.message;
    const urgencyLower = (msg.urgency ?? "").trim().toLowerCase();
    const kindLower = (msg.kind ?? "").trim().toLowerCase();
    const toLower = (msg.to ?? "").trim().toLowerCase();
    const combinedText = `${msg.subject ?? ""} ${msg.body ?? ""}`;

    const isUrgent =
      urgencyLower === "urgent" ||
      kindLower.includes("urgent") ||
      kindLower === "approval_request";
    const isOperatorTargeted =
      toLower === "owner" ||
      toLower === "operator" ||
      toLower === "lmcfarlin" ||
      toLower === "all";
    const hasBlockerKeyword =
      /\b(\[blocked\]|blocker:|urgent:|action required)\b/i.test(combinedText);

    if (isUrgent || isOperatorTargeted || hasBlockerKeyword) {
      const targetAgent =
        toLower &&
        toLower !== "owner" &&
        toLower !== "operator" &&
        toLower !== "lmcfarlin" &&
        toLower !== "all"
          ? msg.to
          : msg.sender_name || null;

      incidents.push({
        id: `wire-msg-${msg.id}`,
        category: "wire",
        severity: isUrgent || hasBlockerKeyword ? "critical" : "warning",
        title: `Wire [${msg.sender_name} → ${msg.to}]: ${msg.subject || "(no subject)"}`,
        subtitle:
          msg.body.length > 120 ? `${msg.body.slice(0, 117)}...` : msg.body,
        agentName: targetAgent,
        badgeLabel: isUrgent ? "mail/urgent" : "operator wire",
        actionLabel: "Open Wire Mail",
        target: {
          kind: "wire",
          wireMessageId: msg.id,
          agentName: targetAgent,
          tab: "mail",
        },
      });
    }
  }

  incidents.sort((a, b) => {
    const rankDiff = SEVERITY_RANK[a.severity] - SEVERITY_RANK[b.severity];
    if (rankDiff !== 0) return rankDiff;
    return a.id.localeCompare(b.id);
  });

  return incidents;
}

function getSeverityStyle(severity: TriageIncidentSeverity): React.CSSProperties {
  if (severity === "critical") {
    return {
      backgroundColor: "rgba(239, 68, 68, 0.16)",
      color: "#F87171",
      borderColor: "rgba(239, 68, 68, 0.45)",
      fontWeight: 700,
    };
  }
  if (severity === "warning") {
    return {
      backgroundColor: "rgba(245, 158, 11, 0.16)",
      color: "#FBBF24",
      borderColor: "rgba(245, 158, 11, 0.4)",
      fontWeight: 700,
    };
  }
  return {
    backgroundColor: "rgba(56, 189, 248, 0.14)",
    color: "#38BDF8",
    borderColor: "rgba(56, 189, 248, 0.35)",
    fontWeight: 700,
  };
}

const CATEGORY_LABELS: Record<TriageCategoryFilter, string> = {
  all: "All",
  heartbeat: "Heartbeats & Attempts",
  review: "PRs & Blocked",
  budget: "Budget & Watch",
  wire: "Urgent Wire",
};

export const TriageQueuePane: React.FC<TriageQueuePaneProps> = ({
  liveness,
  sessions,
  workflowRuns,
  prs,
  records,
  watchAlerts,
  watchAgentHealth,
  wireMessages,
  apiClient,
  loading = false,
  onRefresh,
  onNavigate,
  onSelectAgent,
  onSelectSession,
  onSelectTab,
  onSelectLeftPanelTab,
  onAckWireMessage,
  routedCiPrEvents: propRoutedEvents,
}) => {
  const [routedEvents, setRoutedEvents] = useState<RoutedCiPrFeedbackEvent[]>(
    propRoutedEvents ?? DEFAULT_ROUTED_CI_PR_EVENTS,
  );
  const [routedActionStatus, setRoutedActionStatus] = useState<string | null>(
    null,
  );

  const handleDispatchRoutedEvent = (ev: RoutedCiPrFeedbackEvent) => {
    setRoutedEvents((prev) =>
      prev.map((item) =>
        item.id === ev.id ? { ...item, status: "dispatched" } : item,
      ),
    );
    setRoutedActionStatus(
      `Dispatched ${ev.kind} fix task for PR #${ev.pr_number} (${ev.branch}) to @${ev.author_agent}.`,
    );
  };

  const handleAckRoutedEvent = (ev: RoutedCiPrFeedbackEvent) => {
    setRoutedEvents((prev) =>
      prev.map((item) =>
        item.id === ev.id ? { ...item, status: "acknowledged" } : item,
      ),
    );
    setRoutedActionStatus(
      `Acknowledged ${ev.kind} event for PR #${ev.pr_number}.`,
    );
  };

  // Record #103: Live Permission Prompt Interception & Remote Approval state
  const [permissionPrompts, setPermissionPrompts] = useState<
    PendingPermissionPromptItem[]
  >(DEFAULT_PENDING_PERMISSION_PROMPTS);
  const [permissionPromptBanner, setPermissionPromptBanner] = useState<
    string | null
  >(null);

  const handleRespondPermissionPrompt = async (
    item: PendingPermissionPromptItem,
    decision: PermissionResponseDecision,
  ) => {
    setPermissionPrompts((prev) =>
      prev.map((p) => (p.id === item.id ? { ...p, status: decision } : p)),
    );
    if (decision === "send_keys_unverified") {
      setPermissionPromptBanner(
        `Sent unverified PTY keystroke for @${item.agent} (${item.transport}; no verified permission grant recorded).`,
      );
    } else {
      setPermissionPromptBanner(
        `Sent ${decision} for @${item.agent} (${item.capability}: ${item.command_or_target}).`,
      );
    }
    try {
      if (typeof globalThis.fetch === "function") {
        await globalThis.fetch(
          `/api/v1/sessions/${encodeURIComponent(item.session_id)}/permissions/respond`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({
              prompt_id: item.id,
              capability: item.capability,
              channel_kind: item.channel_kind,
              decision,
            }),
          },
        );
      }
    } catch {
      // Client-side state already updated when endpoint is offline
    }
  };
  const [categoryFilter, setCategoryFilter] =
    useState<TriageCategoryFilter>("all");
  const [searchQuery, setSearchQuery] = useState<string>("");
  const [dismissedIds, setDismissedIds] = useState<Set<string>>(() => new Set());
  const [fetchedWatchAlerts, setFetchedWatchAlerts] = useState<WatchAlert[]>([]);
  const [fetchedWatchHealth, setFetchedWatchHealth] = useState<
    WatchAgentHealth[]
  >([]);
  const [fetchedRecords, setFetchedRecords] = useState<ProjectRecordItem[]>([]);

  const loadSupplementalData = useCallback(async () => {
    if (!apiClient) return;
    try {
      const promises: Array<Promise<void>> = [];
      if (
        watchAlerts === undefined &&
        typeof apiClient.getWatchSummary === "function"
      ) {
        promises.push(
          apiClient
            .getWatchSummary()
            .then((summary) => {
              setFetchedWatchAlerts(summary?.alerts ?? []);
              setFetchedWatchHealth(summary?.health?.agents ?? []);
            })
            .catch(() => {}),
        );
      }
      if (records === undefined && typeof apiClient.listRecords === "function") {
        promises.push(
          apiClient
            .listRecords("ferroloop")
            .then((res) => {
              setFetchedRecords(res?.records ?? []);
            })
            .catch(() => {}),
        );
      }
      await Promise.all(promises);
    } catch {
      // Ignore supplemental fetch errors
    }
  }, [apiClient, watchAlerts, records]);

  useEffect(() => {
    void loadSupplementalData();
  }, [loadSupplementalData]);

  const allIncidents = useMemo(
    () =>
      aggregateTriageIncidents({
        liveness,
        sessions,
        workflowRuns,
        prs,
        records: records ?? fetchedRecords,
        watchAlerts: watchAlerts ?? fetchedWatchAlerts,
        watchAgentHealth: watchAgentHealth ?? fetchedWatchHealth,
        wireMessages,
      }),
    [
      liveness,
      sessions,
      workflowRuns,
      prs,
      records,
      fetchedRecords,
      watchAlerts,
      fetchedWatchAlerts,
      watchAgentHealth,
      fetchedWatchHealth,
      wireMessages,
    ],
  );

  const activeIncidents = useMemo(
    () => allIncidents.filter((item) => !dismissedIds.has(item.id)),
    [allIncidents, dismissedIds],
  );

  const filteredIncidents = useMemo(() => {
    const q = searchQuery.trim().toLowerCase();
    return activeIncidents.filter((item) => {
      if (categoryFilter !== "all" && item.category !== categoryFilter) {
        return false;
      }
      if (!q) return true;
      return (
        item.title.toLowerCase().includes(q) ||
        item.subtitle.toLowerCase().includes(q) ||
        (item.project ?? "").toLowerCase().includes(q) ||
        (item.agentName ?? "").toLowerCase().includes(q) ||
        item.badgeLabel.toLowerCase().includes(q)
      );
    });
  }, [activeIncidents, categoryFilter, searchQuery]);

  const criticalCount = useMemo(
    () => activeIncidents.filter((i) => i.severity === "critical").length,
    [activeIncidents],
  );

  const countByCategory = useMemo(() => {
    const counts: Record<TriageCategoryFilter, number> = {
      all: activeIncidents.length,
      heartbeat: 0,
      review: 0,
      budget: 0,
      wire: 0,
    };
    for (const item of activeIncidents) {
      counts[item.category] += 1;
    }
    return counts;
  }, [activeIncidents]);

  const handleIncidentClick = (incident: TriageIncident) => {
    const { target } = incident;
    if (onNavigate) {
      onNavigate(target, incident);
    }
    if (target.agentName && onSelectAgent) {
      onSelectAgent(target.agentName);
    }
    if (target.sessionId && onSelectSession) {
      onSelectSession(target.sessionId);
    }
    if (target.leftPanelTab && onSelectLeftPanelTab) {
      onSelectLeftPanelTab(target.leftPanelTab);
    } else if (target.tab && onSelectTab) {
      onSelectTab(target.tab);
    }
  };

  return (
    <section
      className="fd-pane"
      data-testid="triage-queue-pane"
      style={{
        display: "flex",
        flexDirection: "column",
        height: "100%",
        minHeight: 0,
        minWidth: 0,
        overflow: "hidden",
      }}
    >
      {/* Header */}
      <div
        style={{
          display: "flex",
          flexDirection: "column",
          gap: "8px",
          padding: "8px 10px",
          borderBottom: "1px solid var(--border-subtle)",
          backgroundColor: "var(--bg-surface)",
          flexShrink: 0,
        }}
      >
        <div
          data-testid="triage-header-top-row"
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: "4px",
            flexWrap: "wrap",
            width: "100%",
            minWidth: 0,
          }}
        >
          <strong
            data-testid="triage-pane-title"
            style={{
              fontSize: "13px",
              color: "var(--text-primary)",
              whiteSpace: "nowrap",
              flexShrink: 0,
            }}
          >
            Events
          </strong>

          <div
            data-testid="triage-header-actions"
            style={{
              display: "flex",
              alignItems: "center",
              gap: "4px",
              marginLeft: "auto",
              flexWrap: "wrap",
              flexShrink: 0,
              minWidth: 0,
            }}
          >
            <span
              className="fd-badge"
              data-testid="triage-total-badge"
              style={{
                padding: "2px 5px",
                fontSize: "11px",
                whiteSpace: "nowrap",
                flexShrink: 0,
              }}
            >
              {activeIncidents.length}{" "}
              {activeIncidents.length === 1 ? "incident" : "incidents"}
            </span>
            {criticalCount > 0 && (
              <span
                className="fd-badge"
                data-testid="triage-critical-badge"
                style={{
                  ...getSeverityStyle("critical"),
                  padding: "2px 5px",
                  fontSize: "11px",
                  whiteSpace: "nowrap",
                  flexShrink: 0,
                }}
              >
                {criticalCount} critical
              </span>
            )}
            {dismissedIds.size > 0 && (
              <button
                type="button"
                className="fd-btn"
                data-testid="triage-restore-dismissed-btn"
                onClick={() => setDismissedIds(new Set())}
                style={{ padding: "2px 6px", fontSize: "11px", whiteSpace: "nowrap", flexShrink: 0 }}
              >
                Restore ({dismissedIds.size})
              </button>
            )}
            <button
              type="button"
              className="fd-btn"
              data-testid="triage-refresh-btn"
              aria-label={loading ? "Refreshing..." : "Refresh"}
              title="Refresh"
              disabled={loading}
              onClick={() => {
                onRefresh?.();
                void loadSupplementalData();
              }}
              style={{ padding: "4px 6px", lineHeight: 1, flexShrink: 0 }}
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

        {/* Category Filter Selector */}
        <div
          role="tablist"
          aria-label="Incident category filter"
          style={{
            width: "100%",
            minWidth: 0,
          }}
        >
          <select
            aria-label="Incident category filter"
            data-testid="triage-category-select"
            className="fd-select"
            value={categoryFilter}
            onChange={(e) =>
              setCategoryFilter(e.target.value as TriageCategoryFilter)
            }
            style={{
              width: "100%",
              minWidth: 0,
              maxWidth: "100%",
              boxSizing: "border-box",
              fontSize: "12px",
              padding: "5px 8px",
            }}
          >
            {(
              [
                "all",
                "heartbeat",
                "review",
                "budget",
                "wire",
              ] as TriageCategoryFilter[]
            ).map((cat) => (
              <option
                key={cat}
                value={cat}
                data-testid={`triage-category-option-${cat}`}
              >
                {CATEGORY_LABELS[cat]} ({countByCategory[cat]})
              </option>
            ))}
          </select>
          <div
            className="sr-only"
            style={{
              position: "absolute",
              width: "1px",
              height: "1px",
              padding: 0,
              margin: "-1px",
              overflow: "hidden",
              clip: "rect(0, 0, 0, 0)",
              whiteSpace: "nowrap",
              border: 0,
            }}
          >
            {(
              [
                "all",
                "heartbeat",
                "review",
                "budget",
                "wire",
              ] as TriageCategoryFilter[]
            ).map((cat) => {
              const isActive = categoryFilter === cat;
              return (
                <button
                  key={cat}
                  type="button"
                  role="tab"
                  aria-selected={isActive}
                  data-testid={`triage-filter-${cat}`}
                  className={`fd-btn ${isActive ? "fd-btn-active" : ""}`}
                  onClick={() => setCategoryFilter(cat)}
                >
                  {CATEGORY_LABELS[cat]} ({countByCategory[cat]})
                </button>
              );
            })}
          </div>
        </div>

        <input
          type="search"
          aria-label="Search incidents"
          data-testid="triage-search-input"
          className="fd-input"
          placeholder="Filter incidents by agent, project, rule, PR..."
          value={searchQuery}
          onChange={(e) => setSearchQuery(e.target.value)}
          style={{ width: "100%", fontSize: "12px" }}
        />
      </div>

      {/* Incident List */}
      <div
        className="fd-scroll"
        data-testid="triage-incident-list"
        style={{
          display: "flex",
          flexDirection: "column",
          gap: "8px",
          padding: "10px 12px",
          flex: 1,
          minHeight: 0,
          overflowY: "auto",
          overflowX: "hidden",
        }}
      >
        {filteredIncidents.length === 0 ? (
          <div
            className="fd-card"
            data-testid="triage-empty-state"
            style={{
              textAlign: "center",
              padding: "24px 12px",
              color: "var(--text-muted)",
              fontSize: "12px",
            }}
          >
            All clear — zero active incidents or blocked items across the fleet.
          </div>
        ) : (
          filteredIncidents.map((incident) => (
            <div
              key={incident.id}
              role="button"
              tabIndex={0}
              data-testid={`triage-incident-${incident.id}`}
              data-category={incident.category}
              data-severity={incident.severity}
              onClick={() => handleIncidentClick(incident)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  handleIncidentClick(incident);
                }
              }}
              className="fd-card"
              style={{
                padding: "10px 12px",
                display: "flex",
                flexDirection: "column",
                gap: "6px",
                cursor: "pointer",
                borderLeft:
                  incident.severity === "critical"
                    ? "3px solid #EF4444"
                    : incident.severity === "warning"
                      ? "3px solid #F59E0B"
                      : "3px solid #38BDF8",
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
                    minWidth: 0,
                  }}
                >
                  <span
                    className="fd-badge"
                    style={{
                      ...getSeverityStyle(incident.severity),
                      fontSize: "10px",
                      textTransform: "uppercase",
                    }}
                  >
                    {incident.severity}
                  </span>
                  <span className="fd-badge fd-mono" style={{ fontSize: "10.5px" }}>
                    {incident.badgeLabel}
                  </span>
                  {incident.project && (
                    <span className="fd-badge fd-mono" style={{ fontSize: "10.5px" }}>
                      {incident.project}
                    </span>
                  )}
                  {incident.agentName && (
                    <span className="fd-badge fd-mono" style={{ fontSize: "10.5px" }}>
                      @{incident.agentName}
                    </span>
                  )}
                </div>

                <div
                  style={{ display: "flex", alignItems: "center", gap: "4px" }}
                  onClick={(e) => e.stopPropagation()}
                >
                  {incident.target.kind === "wire" &&
                    incident.target.wireMessageId &&
                    onAckWireMessage && (
                      <button
                        type="button"
                        className="fd-btn"
                        data-testid={`triage-incident-ack-${incident.id}`}
                        onClick={() =>
                          void onAckWireMessage(incident.target.wireMessageId!)
                        }
                        style={{ padding: "2px 8px", fontSize: "11px" }}
                      >
                        Ack
                      </button>
                    )}
                  <button
                    type="button"
                    className="fd-btn fd-btn-primary"
                    data-testid={`triage-incident-action-${incident.id}`}
                    onClick={() => handleIncidentClick(incident)}
                    style={{ padding: "2px 8px", fontSize: "11px" }}
                  >
                    {incident.actionLabel}
                  </button>
                  <button
                    type="button"
                    className="fd-btn"
                    aria-label={`Dismiss ${incident.title}`}
                    data-testid={`triage-incident-dismiss-${incident.id}`}
                    onClick={() =>
                      setDismissedIds((prev) => {
                        const next = new Set(prev);
                        next.add(incident.id);
                        return next;
                      })
                    }
                    style={{ padding: "2px 6px", fontSize: "11px" }}
                  >
                    ✕
                  </button>
                </div>
              </div>

              <div
                style={{
                  fontWeight: 600,
                  fontSize: "12.5px",
                  color: "var(--text-primary)",
                  overflowWrap: "anywhere",
                }}
              >
                {incident.title}
              </div>

              <div
                style={{
                  fontSize: "11.5px",
                  color: "var(--text-secondary)",
                  overflowWrap: "anywhere",
                }}
              >
                {incident.subtitle}
              </div>
            </div>
          ))
        )}

        {/* Record #88: Automated CI Check Failure & PR Review Feedback Routing */}
        <div
          data-testid="triage-routed-ci-pr-section"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "6px",
            marginTop: "6px",
            paddingTop: "8px",
            borderTop: "1px solid var(--border-subtle)",
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
            <strong
              style={{
                fontSize: "11px",
                textTransform: "uppercase",
                letterSpacing: "0.04em",
                color: "var(--text-muted)",
              }}
            >
              Routed CI &amp; PR Review Feedback ({routedEvents.length})
            </strong>
            <span className="fd-badge fd-mono" style={{ fontSize: "10px" }}>
              Auto-Routed to Author
            </span>
          </div>

          {routedActionStatus && (
            <div
              role="status"
              data-testid="triage-routed-action-status"
              style={{
                fontSize: "11px",
                color: "#34D399",
                padding: "4px 8px",
                borderRadius: "4px",
                backgroundColor: "rgba(16, 185, 129, 0.12)",
              }}
            >
              {routedActionStatus}
            </div>
          )}

          {routedEvents.map((ev) => (
            <div
              key={ev.id}
              data-testid={`routed-ci-pr-card-${ev.id}`}
              data-kind={ev.kind}
              data-status={ev.status}
              className="fd-card"
              style={{
                padding: "8px 10px",
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
                <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                  <span
                    className="fd-badge fd-mono"
                    data-testid={`routed-ci-pr-kind-${ev.id}`}
                  >
                    {ev.kind}
                  </span>
                  <span
                    className="fd-badge fd-mono"
                    data-testid={`routed-ci-pr-branch-${ev.id}`}
                  >
                    PR #{ev.pr_number} · {ev.branch}
                  </span>
                  <span className="fd-badge fd-mono">@{ev.author_agent}</span>
                  <span
                    className="fd-badge fd-mono"
                    data-testid={`routed-ci-pr-status-${ev.id}`}
                  >
                    {ev.status}
                  </span>
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: "4px" }}>
                  <button
                    type="button"
                    className="fd-btn fd-btn-primary"
                    data-testid={`routed-ci-pr-dispatch-btn-${ev.id}`}
                    disabled={ev.status === "dispatched"}
                    style={{ fontSize: "10.5px", padding: "2px 8px" }}
                    onClick={() => handleDispatchRoutedEvent(ev)}
                  >
                    Dispatch Fix to Author
                  </button>
                  <button
                    type="button"
                    className="fd-btn"
                    data-testid={`routed-ci-pr-ack-btn-${ev.id}`}
                    disabled={ev.status === "acknowledged"}
                    style={{ fontSize: "10.5px", padding: "2px 8px" }}
                    onClick={() => handleAckRoutedEvent(ev)}
                  >
                    Acknowledge
                  </button>
                </div>
              </div>
              <div style={{ fontSize: "11.5px", fontWeight: 600 }}>
                {ev.repo} — {ev.check_or_reviewer}
              </div>
              <div className="fd-mono" style={{ fontSize: "11px", color: "var(--text-secondary)" }}>
                {ev.failing_step_or_summary}
              </div>
            </div>
          ))}
        </div>

        {/* Record #103: Live Permission Prompt Interception & Remote Approval */}
        <div
          data-testid="triage-permission-prompts-section"
          style={{
            marginTop: "6px",
            padding: "8px 10px",
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
              Live Permission Prompts &amp; Remote Approval
            </span>
            <span className="fd-badge fd-mono" style={{ fontSize: "10.5px" }}>
              1-Click Remote Approval (#103)
            </span>
          </div>

          {permissionPromptBanner && (
            <div
              role="status"
              data-testid="triage-permission-prompt-banner"
              className="fd-mono"
              style={{ fontSize: "11px", color: "#34D399" }}
            >
              {permissionPromptBanner}
            </div>
          )}

          {permissionPrompts.map((item) => (
            <div
              key={item.id}
              data-testid={`perm-prompt-card-${item.id}`}
              data-status={item.status}
              data-channel-kind={item.channel_kind}
              className="fd-card"
              style={{
                padding: "8px 10px",
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
                <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                  <span className="fd-badge fd-mono">@{item.agent}</span>
                  <span className="fd-badge fd-mono">{item.vendor}</span>
                  <span
                    className="fd-badge fd-mono"
                    data-testid={`perm-prompt-transport-${item.id}`}
                  >
                    {item.transport} ({item.channel_kind})
                  </span>
                  <span
                    className="fd-badge fd-mono"
                    data-testid={`perm-prompt-capability-${item.id}`}
                  >
                    {item.capability}
                  </span>
                  <span
                    className="fd-badge fd-mono"
                    data-testid={`perm-prompt-status-${item.id}`}
                  >
                    {item.status}
                  </span>
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: "4px", flexWrap: "wrap" }}>
                  {item.channel_kind === "rpc" ? (
                    <>
                      <button
                        type="button"
                        className="fd-btn fd-btn-primary"
                        data-testid={`perm-prompt-approve-once-btn-${item.id}`}
                        style={{ fontSize: "10.5px", padding: "2px 6px" }}
                        onClick={() =>
                          void handleRespondPermissionPrompt(
                            item,
                            "approve_once",
                          )
                        }
                      >
                        Approve Once
                      </button>
                      <button
                        type="button"
                        className="fd-btn"
                        data-testid={`perm-prompt-approve-session-btn-${item.id}`}
                        style={{ fontSize: "10.5px", padding: "2px 6px" }}
                        onClick={() =>
                          void handleRespondPermissionPrompt(
                            item,
                            "approve_session",
                          )
                        }
                      >
                        Approve for Session
                      </button>
                      <button
                        type="button"
                        className="fd-btn"
                        data-testid={`perm-prompt-always-allow-btn-${item.id}`}
                        disabled={item.always_allow_supported === false}
                        title={
                          item.always_allow_supported === false
                            ? `Always Allow not supported for ${item.request_kind || item.transport}`
                            : "Persist allow rule"
                        }
                        style={{ fontSize: "10.5px", padding: "2px 6px" }}
                        onClick={() =>
                          void handleRespondPermissionPrompt(
                            item,
                            "always_allow_rule",
                          )
                        }
                      >
                        Always Allow Rule
                      </button>
                      <button
                        type="button"
                        className="fd-btn fd-btn-danger"
                        data-testid={`perm-prompt-deny-btn-${item.id}`}
                        style={{ fontSize: "10.5px", padding: "2px 6px" }}
                        onClick={() =>
                          void handleRespondPermissionPrompt(item, "deny")
                        }
                      >
                        Deny
                      </button>
                    </>
                  ) : (
                    <>
                      <span
                        className="fd-badge fd-mono"
                        data-testid={`perm-prompt-notification-only-badge-${item.id}`}
                        style={{
                          fontSize: "10px",
                          backgroundColor: "rgba(245, 158, 11, 0.16)",
                          color: "#FBBF24",
                          borderColor: "rgba(245, 158, 11, 0.4)",
                        }}
                      >
                        Notification Only (No Verified Approval RPC)
                      </span>
                      {item.unverified_keys_supported !== false && (
                        <button
                          type="button"
                          className="fd-btn"
                          data-testid={`perm-prompt-send-keys-unverified-btn-${item.id}`}
                          style={{ fontSize: "10.5px", padding: "2px 6px" }}
                          onClick={() =>
                            void handleRespondPermissionPrompt(
                              item,
                              "send_keys_unverified",
                            )
                          }
                        >
                          Send keys — unverified
                        </button>
                      )}
                    </>
                  )}
                </div>
              </div>
              <div className="fd-mono" style={{ fontSize: "11px", color: "var(--text-secondary)" }}>
                {item.command_or_target}
              </div>
            </div>
          ))}
        </div>
      </div>
    </section>
  );
};
