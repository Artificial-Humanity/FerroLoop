import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import { ChatPane } from "../components/ChatPane";
import {
  FerroWatchPane,
  computeTelemetryVirtualEquivMicros,
  computeVirtualApiEquivMicros,
  resolveCanonicalTokenRate,
} from "../components/FerroWatchPane";
import {
  RosterSidebar,
  hasTrailingIncompleteWork,
} from "../components/RosterSidebar";
import type { WatchSummaryResponse } from "../types/api";

function makeSampleWatchSummary(): WatchSummaryResponse {
  const nowMs = Date.now();
  return {
    telemetry: {
      project: null,
      source: "ledger",
      totals: {
        attempts: 120,
        succeeded: 112,
        failed: 6,
        crashed: 2,
        timed_out: 0,
        tokens_in: 450000,
        tokens_out: 85000,
        cost_usd_micros: 12_500_000,
        duration_ms: 540000,
      },
      gate_runs: {
        pass: 44,
        fail: 4,
        error: 0,
        cost_usd_micros: 1_200_000,
        duration_ms: 95000,
      },
      by_project: [
        {
          project: "urn:fl:project:ferroloop",
          slug: "FerroLoop",
          root: "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop",
          attempts: 95,
          succeeded: 90,
          failed: 4,
          crashed: 1,
          timed_out: 0,
          tokens_in: 350000,
          tokens_out: 70000,
          cost_usd_micros: 9_800_000,
          duration_ms: 420000,
        },
        {
          project: "urn:fl:project:sonora",
          slug: "Sonora",
          root: "/home/lmcfarlin/Projects/Artificial-Humanity/Sonora",
          attempts: 25,
          succeeded: 22,
          failed: 2,
          crashed: 1,
          timed_out: 0,
          tokens_in: 100000,
          tokens_out: 15000,
          cost_usd_micros: 2_700_000,
          duration_ms: 120000,
        },
      ],
      by_adapter: [
        {
          key: "claude",
          attempts: 80,
          succeeded: 76,
          failed: 3,
          crashed: 1,
          timed_out: 0,
          tokens_in: 300000,
          tokens_out: 60000,
          cost_usd_micros: 8_500_000,
          duration_ms: 360000,
        },
        {
          key: "codex",
          attempts: 40,
          succeeded: 36,
          failed: 3,
          crashed: 1,
          timed_out: 0,
          tokens_in: 150000,
          tokens_out: 25000,
          cost_usd_micros: 4_000_000,
          duration_ms: 180000,
        },
      ],
      by_agent: [
        {
          key: "ferris",
          attempts: 60,
          succeeded: 58,
          failed: 2,
          crashed: 0,
          timed_out: 0,
          tokens_in: 220000,
          tokens_out: 45000,
          cost_usd_micros: 6_200_000,
          duration_ms: 280000,
        },
      ],
      regret: {
        projects_audited: 2,
        challenges: [],
      },
      failure_streaks: [
        {
          record: "urn:fl:record:71",
          slug: "ferrowatch-ui",
          project: "urn:fl:project:ferroloop",
          attempt_streak: 3,
          gate_streak: 1,
          gate_name: "cargo-test",
        },
      ],
    },
    health: {
      now_ms: nowMs,
      stale_after_ms: 30000,
      local_host: "ai-lab-0",
      totals: {
        agents: 2,
        active: 2,
        live: 1,
        stale: 1,
        dead: 0,
        exited: 0,
        missing: 0,
      },
      by_node: [],
      by_project: [],
      hosted_sessions: 1,
      hosted_sessions_undecodable: 0,
      heartbeats_undecodable: 0,
      agents: [
        {
          agent: "ferris",
          project: "FerroLoop",
          adapter: "claude",
          active: true,
          liveness: "live",
          verdict: { state: "alive" },
          heartbeat_at_ms: nowMs - 5000,
          heartbeat_age_ms: 5000,
          node: "local",
          hosted_session: {
            session_id: "claude:ferris",
            persona: "ferris",
            project: "FerroLoop",
            role: "lead",
            vendor: "claude",
            model: "claude-opus-4-7",
            node_id: "local",
            status: "running",
            restart_count: 0,
            updated_at_ms: nowMs - 5000,
          },
        },
        {
          agent: "feira",
          project: "FerroLoop",
          adapter: "codex",
          active: true,
          liveness: "stale",
          verdict: { state: "stale" },
          heartbeat_at_ms: nowMs - 120000,
          heartbeat_age_ms: 120000,
          node: "local",
          hosted_session: null,
        },
      ],
    },
    quotas: [
      {
        vendor: "claude",
        window: "primary",
        used_percent: 92,
        resets_at_ms: nowMs + 3_600_000,
        exhausted_reason: null,
      },
      {
        vendor: "claude",
        window: "secondary",
        used_percent: 64,
        resets_at_ms: nowMs + 86_400_000,
        exhausted_reason: null,
      },
      {
        vendor: "codex",
        window: "primary",
        used_percent: 28,
        resets_at_ms: nowMs + 7_200_000,
        exhausted_reason: null,
      },
      {
        vendor: "codex",
        window: "secondary",
        used_percent: 24,
        resets_at_ms: nowMs + 172_800_000,
        exhausted_reason: null,
      },
    ],
    alerts: [
      {
        id: "quota:claude:primary",
        rule: "quota",
        severity: "critical",
        entity: {
          kind: "vendor",
          id: "claude",
          label: "Claude (5h window)",
        },
        observed: 92,
        threshold: 90,
        at_ms: nowMs,
        resets_at_ms: nowMs + 3_600_000,
        message: "claude primary 5h quota window at 92% (critical threshold 90%)",
        remediation: "Re-model active Claude sessions to Codex or Antigravity.",
      },
      {
        id: "heartbeat_stale:agent:feira",
        rule: "heartbeat_stale",
        severity: "warning",
        entity: {
          kind: "agent",
          id: "feira",
          label: "Feira",
        },
        observed: 120000,
        threshold: 30000,
        at_ms: nowMs,
        message: "Agent feira heartbeat is stale (2m ago).",
        remediation: "Inspect session codex:feira or restart hosted supervision.",
      },
    ],
    thresholds: {
      warn_quota_pct: 80,
      critical_quota_pct: 95,
      warn_cost_usd_micros: 100_000_000,
      critical_cost_usd_micros: 500_000_000,
      stale_after_ms: 30000,
      crash_loop_warn_streak: 3,
      crash_loop_critical_streak: 5,
    },
  };
}

describe("FerroWatchPane — Antigravity hosted model suffix", () => {
  async function renderModels(vendor: string, hostedModel: string, sessionModel: string) {
    const summary = makeSampleWatchSummary();
    summary.telemetry.by_adapter = [
      {
        key: vendor,
        attempts: 3,
        succeeded: 3,
        failed: 0,
        crashed: 0,
        timed_out: 0,
        tokens_in: 1000,
        tokens_out: 100,
        cost_usd_micros: 0,
        duration_ms: 1000,
      },
    ];
    summary.health!.agents = [
      {
        ...summary.health!.agents[0],
        adapter: vendor,
        hosted_session: {
          ...summary.health!.agents[0].hosted_session!,
          vendor,
          model: hostedModel,
        },
      },
    ];
    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const body = String(input).includes("/api/v1/watch/summary") ? summary : {};
      return new Response(JSON.stringify(body), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    });
    render(
      <FerroWatchPane
        authority={{ tier: "owner", login: "lmcfarlin" }}
        apiClient={new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch })}
        sessions={[{ id: "s1", source: "route", adapter: vendor, status: "active", model: sessionModel }]}
      />,
    );
    return screen.findByTestId(`watch-rollup-adapter-${vendor}`);
  }

  it("shows one chip when the hosted record holds the effort-suffixed id of the session model", async () => {
    const row = await renderModels("antigravity", "claude-sonnet-5-5-high", "claude-sonnet-5-5");
    await waitFor(() =>
      expect(row.querySelector("span")?.textContent).toBe("antigravity (claude-sonnet-5-5)"),
    );
  });

  it("keeps the hosted model verbatim for any other vendor", async () => {
    const row = await renderModels("claude", "claude-opus-5-5-high", "claude-opus-5-5");
    await waitFor(() =>
      expect(row).toHaveTextContent("(claude-opus-5-5, claude-opus-5-5-high)"),
    );
  });
});

describe("FerroWatchPane — Telemetry, Quota Matrix, Re-Model/Balance, Liveness & Rollups", () => {
  it("renders alerts, vendor quota matrix, liveness table, and rollups, and executes remodel + balance actions", async () => {
    const summaryData = makeSampleWatchSummary();
    const requestLog: Array<{ url: string; method: string; body?: string }> = [];

    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = (init?.method ?? "GET").toUpperCase();
      const body = typeof init?.body === "string" ? init.body : undefined;
      requestLog.push({ url, method, body });

      const json = (payload: unknown) =>
        new Response(JSON.stringify(payload), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });

      if (url.includes("/api/v1/watch/summary")) {
        return json(summaryData);
      }
      if (url.includes("/api/v1/watch/balance/plan")) {
        return json({
          policy: {
            enabled: false,
            primary_trigger_pct: 90,
            secondary_trigger_pct: 85,
            target_max_primary_pct: 70,
            target_max_secondary_pct: 75,
          },
          switches: [
            {
              persona: "ferris",
              from_vendor: "claude",
              to_vendor: "codex",
              reason: "claude 5h quota window at 92% exceeds 90%",
            },
          ],
          blocked: null,
        });
      }
      if (url.includes("/api/v1/watch/balance/apply")) {
        return json({
          plan: {
            policy: {
              enabled: false,
              primary_trigger_pct: 90,
              secondary_trigger_pct: 85,
              target_max_primary_pct: 70,
              target_max_secondary_pct: 75,
            },
            switches: [
              {
                persona: "ferris",
                from_vendor: "claude",
                to_vendor: "codex",
                reason: "claude 5h quota window at 92% exceeds 90%",
              },
            ],
            blocked: null,
          },
          results: [
            {
              persona: "ferris",
              session_id: "claude:ferris",
              from_vendor: "claude",
              to_vendor: "codex",
              status: "ok",
              new_session_id: "codex:ferris",
              outcome: "remodeled",
              error: null,
            },
          ],
          audit_slug: "watch-balance-01",
          warnings: [],
        });
      }
      if (url.includes("/api/v1/watch/balance")) {
        return json({
          enabled: false,
          policy: {
            enabled: false,
            primary_trigger_pct: 90,
            secondary_trigger_pct: 85,
            target_max_primary_pct: 70,
            target_max_secondary_pct: 75,
          },
        });
      }
      if (url.includes("/api/v1/sessions/ferris/remodel")) {
        return json({
          persona: "ferris",
          from_vendor: "claude",
          vendor: "codex",
          model: "gpt-5.4",
          session_id: "codex:ferris",
          mode: "execute",
          hosted: true,
        });
      }
      if (url.includes("/api/v1/watch/rollups")) {
        return json({
          telemetry: {
            ...summaryData.telemetry,
            by_project: summaryData.telemetry.by_project.filter(
              (p) => p.slug === "FerroLoop",
            ),
          },
        });
      }
      return json({});
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const onAfterRemodel = vi.fn();

    render(
      <FerroWatchPane
        authority={{ tier: "owner", login: "lmcfarlin" }}
        apiClient={apiClient}
        onAfterRemodel={onAfterRemodel}
      />,
    );

    // 1. Live Alerts list
    const alertsSection = await screen.findByTestId("watch-alerts-section");
    expect(screen.getByTestId("ferrowatch-pane").style.overflow).toBe("hidden");
    expect(screen.getByTestId("watch-console-body")).toHaveClass("fd-scroll");
    expect(screen.getByTestId("watch-console-body").style.overflowY).toBe("auto");
    expect(
      within(alertsSection).getByTestId("watch-alert-item-quota:claude:primary"),
    ).toHaveAttribute("data-severity", "critical");
    expect(
      within(alertsSection).getByTestId("watch-alert-item-heartbeat_stale:agent:feira"),
    ).toHaveAttribute("data-severity", "warning");

    // 2. Subscription Quota & Usage Matrix across all 5 canonical vendors
    const quotasSection = screen.getByTestId("watch-quota-matrix-section");
    const claudeCard = within(quotasSection).getByTestId("watch-vendor-card-claude");
    expect(within(claudeCard).getByTestId("watch-vendor-tier-claude")).toHaveTextContent(
      "Max 20x",
    );
    expect(within(claudeCard).getByTestId("watch-quota-primary-pct-claude")).toHaveTextContent(
      "92%",
    );
    expect(within(claudeCard).getByTestId("watch-quota-secondary-pct-claude")).toHaveTextContent(
      "64%",
    );

    const codexCard = within(quotasSection).getByTestId("watch-vendor-card-codex");
    expect(within(codexCard).getByTestId("watch-vendor-tier-codex")).toHaveTextContent(
      "Pro",
    );
    expect(within(codexCard).getByTestId("watch-quota-primary-pct-codex")).toHaveTextContent(
      "28%",
    );

    expect(within(quotasSection).getByTestId("watch-vendor-card-grok")).toBeInTheDocument();
    expect(within(quotasSection).getByTestId("watch-vendor-card-antigravity")).toBeInTheDocument();
    expect(within(quotasSection).getByTestId("watch-vendor-card-muse")).toBeInTheDocument();

    // 3. Dry-Run Balance Plan & Apply Balance
    fireEvent.click(within(quotasSection).getByTestId("watch-balance-plan-btn"));
    const planResults = await screen.findByTestId("watch-balance-plan-results");
    expect(planResults).toHaveTextContent("ferris: claude → codex");

    fireEvent.click(within(quotasSection).getByTestId("watch-balance-apply-btn"));
    const applyResults = await screen.findByTestId("watch-balance-apply-results");
    expect(applyResults).toHaveTextContent("ferris: claude → codex [OK]");

    // 4. Quick Re-Model from Claude card
    fireEvent.click(within(claudeCard).getByTestId("watch-vendor-remodel-btn-claude"));
    const balanceControls = within(quotasSection).getByTestId("watch-quota-balance-controls");
    expect(within(balanceControls).getByTestId("watch-remodel-agent-select")).toHaveValue(
      "ferris",
    );
    fireEvent.change(within(balanceControls).getByTestId("watch-remodel-vendor-select"), {
      target: { value: "codex" },
    });
    fireEvent.change(within(balanceControls).getByTestId("watch-remodel-model-input"), {
      target: { value: "gpt-5.4" },
    });
    fireEvent.click(within(balanceControls).getByTestId("watch-remodel-submit-btn"));

    await waitFor(() => {
      expect(
        requestLog.some(
          (r) =>
            r.method === "POST" &&
            r.url.includes("/api/v1/sessions/ferris/remodel") &&
            r.body?.includes('"vendor":"codex"'),
        ),
      ).toBe(true);
    });
    expect(onAfterRemodel).toHaveBeenCalledTimes(1);

    // 5. Agent & Peer Liveness Table
    const livenessSection = screen.getByTestId("watch-liveness-section");
    const ferrisRow = within(livenessSection).getByTestId("watch-liveness-row-ferris");
    expect(within(ferrisRow).getByTestId("watch-liveness-badge-ferris")).toHaveAttribute(
      "data-liveness",
      "live",
    );
    expect(within(ferrisRow).getByTestId("watch-liveness-hosted-ferris")).toHaveTextContent(
      "claude:ferris",
    );

    // 6. Spend & Token Rollups + Project Filter + Threshold Controls
    const rollupsSection = screen.getByTestId("watch-rollups-section");
    expect(
      within(rollupsSection).getByTestId("watch-rollup-project-FerroLoop"),
    ).toHaveTextContent("$9.80");

    const projectSelect = within(rollupsSection).getByTestId("watch-rollups-project-select");
    fireEvent.change(projectSelect, { target: { value: "FerroLoop" } });
    await waitFor(() => {
      expect(
        requestLog.some((r) => r.url.includes("/api/v1/watch/rollups?project=FerroLoop")),
      ).toBe(true);
    });

    const thresholdsForm = within(rollupsSection).getByTestId("watch-thresholds-form");
    fireEvent.change(within(thresholdsForm).getByTestId("threshold-warn-quota-input"), {
      target: { value: "70" },
    });
    fireEvent.click(within(thresholdsForm).getByTestId("watch-apply-thresholds-btn"));

    await waitFor(() => {
      expect(requestLog.some((r) => r.url.includes("warn_quota_pct=70"))).toBe(true);
    });
  });

  it("displays virtual API-equivalent dollar value alongside $0 billed subscription CLI runs (Record #221)", async () => {
    // 1M in + 100k out on Claude Opus ($15/$75 per 1M) = $15 + $7.50 = $22.50 (22,500,000 micros)
    expect(resolveCanonicalTokenRate("claude", "claude-opus-4-7").inputUsdPer1M).toBe(15);
    expect(
      computeVirtualApiEquivMicros({
        tokensIn: 1_000_000,
        tokensOut: 100_000,
        costUsdMicros: 0,
        vendor: "claude",
        model: "claude-opus-4-7",
      }),
    ).toBe(22_500_000);

    // 1M in + 200k out on Codex Pro ($10/$30 per 1M) = $10 + $6 = $16.00 (16,000,000 micros)
    expect(
      computeVirtualApiEquivMicros({
        tokensIn: 1_000_000,
        tokensOut: 200_000,
        costUsdMicros: 0,
        vendor: "codex",
        model: "gpt-5.4",
      }),
    ).toBe(16_000_000);

    const zeroBilledSummary = makeSampleWatchSummary();
    zeroBilledSummary.telemetry.totals = {
      ...zeroBilledSummary.telemetry.totals,
      tokens_in: 2_000_000,
      tokens_out: 300_000,
      cost_usd_micros: 0,
    };
    zeroBilledSummary.telemetry.by_adapter = [
      {
        key: "claude",
        attempts: 80,
        succeeded: 80,
        failed: 0,
        crashed: 0,
        timed_out: 0,
        tokens_in: 1_000_000,
        tokens_out: 100_000,
        cost_usd_micros: 0,
        duration_ms: 120000,
      },
      {
        key: "codex",
        attempts: 40,
        succeeded: 40,
        failed: 0,
        crashed: 0,
        timed_out: 0,
        tokens_in: 1_000_000,
        tokens_out: 200_000,
        cost_usd_micros: 0,
        duration_ms: 90000,
      },
    ];
    zeroBilledSummary.telemetry.by_agent = [
      {
        key: "ferris",
        attempts: 80,
        succeeded: 80,
        failed: 0,
        crashed: 0,
        timed_out: 0,
        tokens_in: 1_000_000,
        tokens_out: 100_000,
        cost_usd_micros: 0,
        duration_ms: 120000,
      },
    ];

    expect(computeTelemetryVirtualEquivMicros(zeroBilledSummary.telemetry)).toBe(
      38_500_000,
    );

    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const json = (payload: unknown) =>
        new Response(JSON.stringify(payload), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      if (url.includes("/api/v1/watch/summary")) {
        return json(zeroBilledSummary);
      }
      return json({});
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });

    render(
      <FerroWatchPane
        authority={{ tier: "owner", login: "lmcfarlin" }}
        apiClient={apiClient}
      />,
    );

    const kpiEquiv = await screen.findByTestId("watch-kpi-virtual-equiv");
    await waitFor(() => {
      expect(kpiEquiv).toHaveTextContent("API Eq: $38.50");
    });
    expect(screen.getByTestId("watch-kpi-spend")).toHaveTextContent("$0.00");

    // Per-vendor card virtual equivalence
    expect(screen.getByTestId("watch-vendor-virtual-value-claude")).toHaveTextContent(
      "Billed: $0.00 · API-Eq: $22.50",
    );
    expect(screen.getByTestId("watch-vendor-virtual-value-codex")).toHaveTextContent(
      "Billed: $0.00 · API-Eq: $16.00",
    );

    // Rollups virtual equivalence
    expect(screen.getByTestId("watch-rollups-virtual-equiv")).toHaveTextContent(
      "Virtual API-Eq: $38.50",
    );
    expect(screen.getByTestId("watch-rollup-adapter-virtual-claude")).toHaveTextContent(
      "$22.50 API-eq",
    );
    expect(screen.getByTestId("watch-rollup-adapter-virtual-codex")).toHaveTextContent(
      "$16.00 API-eq",
    );
    expect(screen.getByTestId("watch-rollup-agent-virtual-ferris")).toHaveTextContent(
      "$22.50 API-eq",
    );
  });
});

describe("RosterSidebar Activity Tag & ChatPane Busy Badge Relocation", () => {
  it("computes hasTrailingIncompleteWork accurately from transcript turns", () => {
    expect(hasTrailingIncompleteWork([])).toBe(false);
    expect(
      hasTrailingIncompleteWork([
        { index: 0, role: "user", content: "Hello", thinking: null, tool_calls: [] },
      ]),
    ).toBe(true);
    expect(
      hasTrailingIncompleteWork([
        { index: 0, role: "user", content: "Hello", thinking: null, tool_calls: [] },
        { index: 1, role: "assistant", content: "Hi!", thinking: null, tool_calls: [] },
      ]),
    ).toBe(false);
    expect(
      hasTrailingIncompleteWork([
        {
          index: 0,
          role: "assistant",
          content: "Running tool...",
          thinking: null,
          tool_calls: [{ name: "bash", arguments: { cmd: "cargo test" } }],
        },
      ]),
    ).toBe(true);
  });

  it("renders icon-only activity-tag-<key> in RosterSidebar for working, idle, and offline states and hides visual busy pill in ChatPane", () => {
    render(
      <RosterSidebar
        agents={[
          {
            id: "urn:fl:agent:ferris",
            name_key: "ferris",
            display_name: "Ferris",
            project: "FerroLoop",
            role: "lead",
            status: "active",
          },
          {
            id: "urn:fl:agent:felix",
            name_key: "felix",
            display_name: "Felix",
            project: "FerroLoop",
            role: "ui-developer",
            status: "active",
          },
          {
            id: "urn:fl:agent:feira",
            name_key: "feira",
            display_name: "Feira",
            project: "FerroLoop",
            role: "assistant",
            status: "active",
          },
        ]}
        routes={[
          {
            id: "claude:ferris",
            source: "route",
            adapter: "claude",
            persona: "ferris",
            status: "active",
          },
          {
            id: "antigravity:felix",
            source: "route",
            adapter: "antigravity",
            persona: "felix",
            status: "active",
          },
        ]}
        liveness={[
          {
            agent: "ferris",
            project: "FerroLoop",
            role: "lead",
            adapter: "claude",
            verdict: { state: "alive" },
            has_route: true,
          },
          {
            agent: "felix",
            project: "FerroLoop",
            role: "ui-developer",
            adapter: "antigravity",
            verdict: { state: "alive" },
            has_route: true,
          },
          {
            agent: "feira",
            project: "FerroLoop",
            role: "assistant",
            adapter: null,
            verdict: { state: "dead" },
            has_route: false,
          },
        ]}
        unreadByAgent={{}}
        busyByAgent={{ ferris: true, felix: false }}
        selectedAgentName="ferris"
        onSelectAgent={vi.fn()}
      />,
    );

    const ferrisTag = screen.getByTestId("activity-tag-ferris");
    expect(ferrisTag).toHaveAttribute("data-activity", "working");
    expect(ferrisTag).toHaveAttribute("title", "Activity: Working (Busy)");

    const felixTag = screen.getByTestId("activity-tag-felix");
    expect(felixTag).toHaveAttribute("data-activity", "idle");
    expect(felixTag).toHaveAttribute("title", "Activity: Idle (Online)");

    const feiraTag = screen.getByTestId("activity-tag-feira");
    expect(feiraTag).toHaveAttribute("data-activity", "offline");
    expect(feiraTag).toHaveAttribute("title", "Activity: Offline");

    // Verify ChatPane keeps session-busy-badge only as an sr-only 1x1 span
    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        agentRole="lead"
        agentProject="FerroLoop"
        adapter="claude"
        model="claude-opus-4-7"
        sessionId="claude:ferris"
        turns={[]}
        isBusy={true}
        queue={[]}
        loading={false}
        wsConnected={true}
        onRefresh={vi.fn()}
        onSendPrompt={vi.fn()}
        onSendQueuedNow={vi.fn()}
        onCancelQueued={vi.fn()}
        onUpdateQueued={vi.fn()}
      />,
    );

    const busyBadge = screen.getByTestId("session-busy-badge");
    expect(busyBadge.style.position).toBe("absolute");
    expect(busyBadge.style.width).toBe("1px");
    expect(busyBadge.style.height).toBe("1px");
  });

  it("opens FerroWatchPane in the Left Panel console when clicking left-panel-tab-watch in App", async () => {
    const summaryData = makeSampleWatchSummary();
    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const json = (payload: unknown) =>
        new Response(JSON.stringify(payload), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      if (url.includes("/api/v1/auth/session")) {
        return json({
          authenticated: true,
          authority: { tier: "owner", login: "lmcfarlin" },
        });
      }
      if (url.includes("/api/v1/watch/summary")) {
        return json(summaryData);
      }
      return json({});
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const wsStub = {
      setToken: vi.fn(),
      connect: vi.fn(),
      disconnect: vi.fn(),
      onStatusChange: () => () => {},
      subscribe: () => () => {},
    } as unknown as HubWsClient;

    const { unmount } = render(<App apiClient={apiClient} wsClient={wsStub} />);

    const sidebar = await screen.findByTestId("roster-sidebar");
    const watchTabBtn = within(sidebar).getByTestId("left-panel-tab-watch");
    fireEvent.click(watchTabBtn);

    expect(within(sidebar).getByTestId("left-panel-active-title")).toHaveTextContent(
      "FerroWatch",
    );
    const watchPane = await screen.findByTestId("ferrowatch-pane");
    expect(watchPane).toBeInTheDocument();
    expect(watchPane.style.overflow).toBe("hidden");

    const topRow = within(watchPane).getByTestId("ferrowatch-header-top-row");
    expect(within(topRow).getByTestId("ferrowatch-pane-title")).toHaveTextContent(
      "FerroWatch",
    );
    const headerActions = within(topRow).getByTestId("ferrowatch-header-actions");
    expect(headerActions.style.marginLeft).toBe("auto");
    const refreshBtn = within(headerActions).getByTestId("watch-refresh-btn");
    expect(refreshBtn).toHaveAttribute("aria-label", "Refresh");
    expect(refreshBtn.querySelector("svg")).not.toBeNull();

    const consoleBody = within(watchPane).getByTestId("watch-console-body");
    expect(consoleBody).toHaveClass("fd-scroll");
    expect(consoleBody.style.overflowY).toBe("auto");

    unmount();
  });
});
