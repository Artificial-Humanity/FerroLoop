import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import {
  aggregateTriageIncidents,
  TriageQueuePane,
  type TriageNavigationTarget,
} from "../components/TriageQueuePane";
import type {
  AgentLivenessItem,
  DiscoveredSessionSummary,
  ProjectRecordItem,
  PullRequestItem,
  WatchAlert,
  WireInboxItem,
  WorkflowRun,
} from "../types/api";

const SAMPLE_LIVENESS: AgentLivenessItem[] = [
  {
    agent: "feira",
    project: "FerroLoop",
    role: "system-developer",
    has_route: true,
    adapter: "claude",
    verdict: {
      state: "stale",
      reason: "Heartbeat stale for 48s",
    },
  },
  {
    agent: "ferris",
    project: "FerroLoop",
    role: "lead",
    has_route: true,
    adapter: "claude",
    verdict: {
      state: "alive",
      reason: "Healthy",
    },
  },
];

const SAMPLE_SESSIONS: DiscoveredSessionSummary[] = [
  {
    id: "sess-kacey-timeout",
    source: "route",
    adapter: "codex",
    persona: "kacey",
    status: "timed_out",
    cwd: "/home/lmcfarlin/Projects/Artificial-Humanity/Kaggle-Gemma4",
  },
];

const SAMPLE_WORKFLOW_RUNS: WorkflowRun[] = [
  {
    id: "run-failed-1",
    workflow_id: "wf-ci",
    project: "FerroLoop",
    title: "Nightly Verification Pipeline",
    status: "failed",
    active_steps: ["verify"],
    steps: {
      verify: {
        step_id: "verify",
        visit_count: 1,
        attempt_count: 2,
        state: "failed",
        assigned_agent: "feira",
        last_outcome: "fail",
        tokens_used: 18000,
        cost_usd_micros: 90000,
      },
    },
    total_steps_executed: 1,
    total_tokens: 18000,
    total_cost_usd_micros: 90000,
    created_at: "2026-11-03T03:00:00Z",
    updated_at: "2026-11-03T03:05:00Z",
    by: "ferris",
  },
  {
    id: "run-gate-2",
    workflow_id: "wf-release",
    project: "FerroLoop",
    title: "Release Sign-Off Gate",
    status: "needs_human",
    active_steps: ["sign_off"],
    steps: {
      sign_off: {
        step_id: "sign_off",
        visit_count: 1,
        attempt_count: 1,
        state: "needs_human",
        assigned_agent: "ferris",
        last_outcome: "needs_human",
        needs_human_reason: "Awaiting operator release approval",
        tokens_used: 4000,
        cost_usd_micros: 20000,
      },
    },
    total_steps_executed: 1,
    total_tokens: 4000,
    total_cost_usd_micros: 20000,
    created_at: "2026-11-03T03:10:00Z",
    updated_at: "2026-11-03T03:12:00Z",
    by: "ferris",
  },
];

const SAMPLE_PRS: PullRequestItem[] = [
  {
    number: 225,
    title: "feat(ui): add inline diff line commenting",
    author: "felix",
    project: "FerroLoop",
    branch: "felix/record-89",
    head_sha: "46ca07d",
    status: "open",
    reviews: [],
  },
];

const SAMPLE_RECORDS: ProjectRecordItem[] = [
  {
    id: "urn:uuid:record-90",
    handle: 90,
    display_id: "#90",
    project: "ferroloop",
    title: "Operator Today & Incidents Triage Queue",
    state: "doing",
    findings_open: 2,
    findings_total: 3,
  },
];

const SAMPLE_WATCH_ALERTS: WatchAlert[] = [
  {
    id: "alert-budget-1",
    rule: "budget_threshold",
    severity: "critical",
    entity: { kind: "project", project: "FerroLoop", agent: "feira" } as unknown as WatchAlert["entity"],
    message: "Daily project spend exceeded 90% of budget cap",
    observed: 450_000_000,
    threshold: 400_000_000,
    at_ms: 1793678000000,
    remediation: "Remodel subordinate sessions to economy tier or raise budget cap.",
  },
];

const SAMPLE_WIRE: WireInboxItem[] = [
  {
    message: {
      id: "urn:uuid:wire-urgent-1",
      thread_id: "urn:uuid:wire-urgent-1",
      sender_name: "jarvis",
      to: "owner",
      kind: "mail",
      urgency: "urgent",
      durable: true,
      subject: "GPU temperature threshold on ai-lab-0",
      body: "Action required: GPU 0 reached 88C during Sonora training.",
      created_at: "2026-11-03T03:50:00Z",
    },
    receipt: {
      message_id: "urn:uuid:wire-urgent-1",
      recipient_id: "owner",
      recipient_name: "owner",
      status: "delivered",
      attempts: 1,
      updated_at: "2026-11-03T03:50:01Z",
    },
  },
  {
    message: {
      id: "urn:uuid:wire-acked-2",
      thread_id: "urn:uuid:wire-acked-2",
      sender_name: "ferris",
      to: "felix",
      kind: "mail",
      urgency: "urgent",
      durable: true,
      subject: "Already handled urgent mail",
      body: "This was already acknowledged.",
      created_at: "2026-11-03T02:00:00Z",
    },
    receipt: {
      message_id: "urn:uuid:wire-acked-2",
      recipient_id: "felix",
      recipient_name: "felix",
      status: "acked",
      attempts: 1,
      updated_at: "2026-11-03T02:01:00Z",
    },
  },
];

describe("Record #90 — Operator Today & Incidents Triage Queue", () => {
  it("aggregates heartbeat/attempt failures, pending PRs & blocked records, FerroWatch budget alerts, and unread urgent wire messages", () => {
    const incidents = aggregateTriageIncidents({
      liveness: SAMPLE_LIVENESS,
      sessions: SAMPLE_SESSIONS,
      workflowRuns: SAMPLE_WORKFLOW_RUNS,
      prs: SAMPLE_PRS,
      records: SAMPLE_RECORDS,
      watchAlerts: SAMPLE_WATCH_ALERTS,
      wireMessages: SAMPLE_WIRE,
    });

    // 3 heartbeat/attempt (feira stale, kacey timed_out, run-failed-1) +
    // 3 review/blocked (run-gate-2, PR #225, Record #90) +
    // 1 budget alert (alert-budget-1) +
    // 1 unread urgent wire (wire-urgent-1; wire-acked-2 is ignored) = 8 incidents
    expect(incidents).toHaveLength(8);
    expect(incidents.some((i) => i.id === "wire-msg-urn:uuid:wire-acked-2")).toBe(
      false,
    );

    // Sorted with critical incidents first
    expect(incidents[0].severity).toBe("critical");
  });

  it("filters incidents by category and search query, supports dismiss/restore and wire ack, and navigates directly to the target session, record, PR, watch view, or wire message on click", () => {
    const navigated: TriageNavigationTarget[] = [];
    const onSelectAgent = vi.fn();
    const onSelectSession = vi.fn();
    const onSelectTab = vi.fn();
    const onSelectLeftPanelTab = vi.fn();
    const onAckWireMessage = vi.fn();

    render(
      <TriageQueuePane
        liveness={SAMPLE_LIVENESS}
        sessions={SAMPLE_SESSIONS}
        workflowRuns={SAMPLE_WORKFLOW_RUNS}
        prs={SAMPLE_PRS}
        records={SAMPLE_RECORDS}
        watchAlerts={SAMPLE_WATCH_ALERTS}
        wireMessages={SAMPLE_WIRE}
        onNavigate={(target) => navigated.push(target)}
        onSelectAgent={onSelectAgent}
        onSelectSession={onSelectSession}
        onSelectTab={onSelectTab}
        onSelectLeftPanelTab={onSelectLeftPanelTab}
        onAckWireMessage={onAckWireMessage}
      />,
    );

    expect(screen.getByTestId("triage-pane-title")).toHaveTextContent(
      "Today & Incidents",
    );
    expect(screen.getByTestId("triage-total-badge")).toHaveTextContent(
      "8 incidents",
    );
    expect(screen.getByTestId("triage-critical-badge")).toHaveTextContent(
      "4 critical",
    );

    // Click timed-out session incident -> navigates to session & chat tab
    fireEvent.click(
      screen.getByTestId("triage-incident-attempt-session-sess-kacey-timeout"),
    );
    expect(onSelectAgent).toHaveBeenCalledWith("kacey");
    expect(onSelectSession).toHaveBeenCalledWith("sess-kacey-timeout");
    expect(onSelectTab).toHaveBeenCalledWith("chat");

    // Click pending PR incident -> navigates to git tab
    fireEvent.click(
      screen.getByTestId("triage-incident-review-pr-FerroLoop-225"),
    );
    expect(onSelectAgent).toHaveBeenCalledWith("felix");
    expect(onSelectTab).toHaveBeenCalledWith("git");

    // Click record with open findings -> navigates to board tab
    fireEvent.click(
      screen.getByTestId("triage-incident-review-record-urn:uuid:record-90"),
    );
    expect(onSelectTab).toHaveBeenCalledWith("board");

    // Click FerroWatch budget alert -> navigates to watch left panel tab
    fireEvent.click(
      screen.getByTestId("triage-incident-budget-alert-alert-budget-1"),
    );
    expect(onSelectLeftPanelTab).toHaveBeenCalledWith("watch");

    // Click unread urgent wire incident -> navigates to mail tab
    fireEvent.click(
      screen.getByTestId("triage-incident-wire-msg-urn:uuid:wire-urgent-1"),
    );
    expect(onSelectAgent).toHaveBeenCalledWith("jarvis");
    expect(onSelectTab).toHaveBeenCalledWith("mail");

    // Click Ack button on urgent wire incident
    fireEvent.click(
      screen.getByTestId("triage-incident-ack-wire-msg-urn:uuid:wire-urgent-1"),
    );
    expect(onAckWireMessage).toHaveBeenCalledWith("urn:uuid:wire-urgent-1");

    // Filter by Budget & Watch category
    fireEvent.click(screen.getByTestId("triage-filter-budget"));
    expect(
      screen.getByTestId("triage-incident-budget-alert-alert-budget-1"),
    ).toBeInTheDocument();
    expect(
      screen.queryByTestId("triage-incident-review-pr-FerroLoop-225"),
    ).toBeNull();

    // Dismiss the budget alert and restore it
    fireEvent.click(
      screen.getByTestId("triage-incident-dismiss-budget-alert-alert-budget-1"),
    );
    expect(screen.getByTestId("triage-empty-state")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("triage-restore-dismissed-btn"));
    expect(
      screen.getByTestId("triage-incident-budget-alert-alert-budget-1"),
    ).toBeInTheDocument();
  });

  it("opens Today & Incidents TriageQueuePane from RosterSidebar via roster-triage-toggle-btn and left-panel-tab-triage", () => {
    const apiClient = new HubApiClient({
      fetchImpl: vi.fn(
        async () =>
          new Response(JSON.stringify({}), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          }),
      ) as unknown as typeof fetch,
    });
    const wsClient = {
      connect: vi.fn(),
      disconnect: vi.fn(),
      setToken: vi.fn(),
      subscribe: vi.fn(() => () => {}),
      onStatusChange: vi.fn(() => () => {}),
    } as unknown as HubWsClient;

    render(<App apiClient={apiClient} wsClient={wsClient} />);

    const sidebar = screen.getByTestId("roster-sidebar");
    fireEvent.click(within(sidebar).getByTestId("roster-triage-toggle-btn"));
    const triagePane = within(sidebar).getByTestId("triage-queue-pane");
    expect(triagePane).toBeInTheDocument();
    expect(triagePane.style.overflow).toBe("hidden");
    const incidentList = within(triagePane).getByTestId("triage-incident-list");
    expect(incidentList).toHaveClass("fd-scroll");
    expect(incidentList.style.overflowY).toBe("auto");
    expect(within(sidebar).getByTestId("triage-pane-title")).toHaveTextContent(
      "Today & Incidents",
    );
    const headerTopRow = within(triagePane).getByTestId("triage-header-top-row");
    expect(headerTopRow.style.flexWrap).toBe("wrap");
    const headerActions = within(triagePane).getByTestId("triage-header-actions");
    expect(headerActions.style.marginLeft).toBe("auto");
    expect(headerActions.style.flexWrap).toBe("wrap");
    const refreshBtn = within(headerActions).getByTestId("triage-refresh-btn");
    expect(refreshBtn).toHaveAttribute("aria-label", "Refresh");
    expect(refreshBtn.querySelector("svg")).not.toBeNull();
  });
});
