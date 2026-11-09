import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import "@testing-library/jest-dom/vitest";
import { sameContextUsage } from "../App";
import {
  AgentSettingsModal,
  type AgentSettingsModalProps,
} from "../components/AgentSettingsModal";
import {
  ChatPane,
  estimateSessionContextUsage,
  formatTokenCount,
} from "../components/ChatPane";
import { FerroWatchPane } from "../components/FerroWatchPane";
import { RosterSidebar } from "../components/RosterSidebar";
import { HubApiClient } from "../api/client";
import { TEST_WORKSPACE_AGENTS as DEFAULT_WORKSPACE_AGENTS } from "./fixtures/testAgents";
import {
  type DiscoveredSessionSummary,
  type SessionContextUsage,
  type WatchSummaryResponse,
} from "../types/api";

const ADAPTERS = ["claude", "codex", "antigravity", "muse", "grok"] as const;

function modalProps(
  usage: SessionContextUsage | null,
  overrides: Partial<AgentSettingsModalProps> = {},
): AgentSettingsModalProps {
  return {
    isOpen: true,
    onClose: () => {},
    agentName: "ferris",
    agentDisplayName: "Ferris",
    sessionId: "sess-1",
    adapter: "claude",
    selectedModel: "claude-opus-5-5",
    selectedThinkingLevel: "high",
    onChangeModelAndThinking: () => {},
    contextEstimate: estimateSessionContextUsage([], usage),
    canCompact: false,
    compacting: false,
    onCompactContext: () => {},
    canClear: true,
    clearing: false,
    onOpenClearModal: () => {},
    canDecommission: true,
    decommissioning: false,
    onOpenDecommissionModal: () => {},
    ...overrides,
  };
}

describe("Agent Settings context meter", () => {
  it("shows used / max exactly as the server reported it", () => {
    render(
      <AgentSettingsModal
        {...modalProps({ used_tokens: 150_000, max_tokens: 256_000, source: "reported" })}
      />,
    );
    const meter = screen.getByTestId("context-level-meter");
    expect(meter).toHaveTextContent("Context: 150k / 256k (59%)");
    expect(meter).not.toHaveTextContent("assumed");
  });

  it("marks an assumed maximum and explains it in the tooltip", () => {
    render(
      <AgentSettingsModal
        {...modalProps({ used_tokens: 150_000, max_tokens: 1_000_000, source: "assumed" })}
      />,
    );
    const max = screen.getByTestId("context-max");
    expect(max).toHaveTextContent("about 1m, assumed");
    expect(max).toHaveAttribute("data-source", "assumed");
    expect(max).toHaveAttribute(
      "title",
      "No vendor file or setting names this session's window; this is a default",
    );
  });

  it.each([
    ["reported", "Window reported by the session"],
    ["catalog", "Window from the vendor's model catalog"],
  ] as const)("shows no assumed marker for a %s maximum", (source, tooltip) => {
    render(
      <AgentSettingsModal
        {...modalProps({ used_tokens: 10_000, max_tokens: 1_007_997, source })}
      />,
    );
    const max = screen.getByTestId("context-max");
    expect(max).not.toHaveTextContent(/assumed|about/);
    expect(max).toHaveAttribute("title", tooltip);
  });

  it("makes no claim about the source when an older hub omits it", () => {
    render(<AgentSettingsModal {...modalProps({ used_tokens: 10_000, max_tokens: 200_000 })} />);
    const max = screen.getByTestId("context-max");
    expect(max).toHaveTextContent("200k");
    expect(max).not.toHaveTextContent(/assumed|about/);
    expect(max).not.toHaveAttribute("title");
    expect(max).toHaveAttribute("data-source", "unknown");
  });

  it("lists the offered sizes read-only and highlights the effective one", () => {
    render(
      <AgentSettingsModal
        {...modalProps({
          used_tokens: 1_000,
          max_tokens: 500_000,
          source: "reported",
          offered_windows: [256_000, 500_000],
        })}
      />,
    );
    const offered = screen.getByTestId("context-offered-windows");
    expect(offered).toHaveTextContent("Offered: 256k, 500k");
    expect(within(offered).getByTestId("context-offered-500000")).toHaveAttribute(
      "data-effective",
      "true",
    );
    expect(within(offered).getByTestId("context-offered-500000")).toHaveAttribute(
      "aria-current",
      "true",
    );
    expect(within(offered).getByTestId("context-offered-256000")).not.toHaveAttribute(
      "aria-current",
    );
    expect(within(offered).getByTestId("context-offered-256000")).not.toHaveAttribute(
      "data-effective",
    );
    // Phase 1: nothing in the list is a control.
    expect(within(offered).queryAllByRole("button")).toHaveLength(0);
  });

  it("lists the sizes with no highlight and no extra text when the maximum is not one of them", () => {
    render(
      <AgentSettingsModal
        {...modalProps({
          used_tokens: 1_000,
          max_tokens: 300_000,
          source: "reported",
          offered_windows: [256_000, 500_000],
        })}
      />,
    );
    const offered = screen.getByTestId("context-offered-windows");
    expect(offered).toHaveTextContent(/^Offered: 256k, 500k$/);
    expect(offered.querySelector("[data-effective]")).toBeNull();
    expect(screen.getByTestId("context-level-meter")).toHaveTextContent("Context: 1k / 300k");
  });

  it("shows no Offered line when the hub lists no sizes", () => {
    render(<AgentSettingsModal {...modalProps({ used_tokens: 1, max_tokens: 200_000 })} />);
    expect(screen.queryByTestId("context-offered-windows")).toBeNull();
  });

  it("shows no maximum, no percent and no bar when the hub sent no usage", () => {
    render(<AgentSettingsModal {...modalProps(null)} />);
    const meter = screen.getByTestId("context-level-meter");
    expect(meter).toHaveTextContent("Context: no data");
    expect(meter).not.toHaveTextContent("%");
    expect(screen.queryByTestId("context-max")).toBeNull();
    expect(screen.queryByRole("progressbar")).toBeNull();
  });
});

describe("no hard-coded vendor maximum", () => {
  it.each(ADAPTERS)(
    "%s with no context_usage shows used tokens at most, with no maximum and no percent",
    (adapter) => {
      render(
        <ChatPane
          agentName="ferris"
          agentDisplayName="Ferris"
          adapter={adapter}
          sessionId="sess-1"
          turns={[
            {
              index: 0,
              role: "user",
              content: "Hello",
              thinking: null,
              tool_calls: [],
              created_at: null,
            },
          ]}
          agentSettingsOpen={true}
          onSendPrompt={vi.fn()}
        />,
      );
      const meter = screen.getByTestId("context-level-meter");
      expect(meter).toHaveTextContent("window not reported");
      expect(meter).not.toHaveTextContent(/%|\/|\d+(\.\d)?m\b|500k|200k|256k/);
      expect(screen.queryByTestId("context-max")).toBeNull();
      expect(screen.queryByRole("progressbar")).toBeNull();
      // An unknown window does not disable Compact (see "Compact with an unknown window").
      expect(screen.getByTestId("compact-context-btn")).not.toBeDisabled();
    },
  );

  it.each(ADAPTERS)("estimateSessionContextUsage invents no maximum for %s", () => {
    expect(estimateSessionContextUsage([])).toMatchObject({ maxTokens: null, percent: null });
  });
});

describe("App context usage dedupe", () => {
  const base: SessionContextUsage = {
    used_tokens: 10,
    max_tokens: 1_000_000,
    source: "assumed",
    offered_windows: [256_000, 1_000_000],
  };

  it("treats identical reports as the same", () => {
    expect(sameContextUsage(base, { ...base, offered_windows: [256_000, 1_000_000] })).toBe(true);
    expect(sameContextUsage(null, null)).toBe(true);
    expect(sameContextUsage(undefined, undefined)).toBe(true);
    expect(
      sameContextUsage(
        { used_tokens: 1, max_tokens: 2 },
        { used_tokens: 1, max_tokens: 2, offered_windows: [] },
      ),
    ).toBe(true);
  });

  it("does not drop a change of source at the same max_tokens", () => {
    expect(sameContextUsage(base, { ...base, source: "reported" })).toBe(false);
    expect(sameContextUsage(base, { ...base, source: undefined })).toBe(false);
  });

  it("does not drop a change of offered sizes at the same max_tokens", () => {
    expect(sameContextUsage(base, { ...base, offered_windows: [1_000_000] })).toBe(false);
    expect(sameContextUsage(base, { ...base, offered_windows: undefined })).toBe(false);
  });

  it("still tells used or max changes and a missing report apart", () => {
    expect(sameContextUsage(base, { ...base, used_tokens: 11 })).toBe(false);
    expect(sameContextUsage(base, { ...base, max_tokens: 500_000 })).toBe(false);
    expect(sameContextUsage(base, null)).toBe(false);
    expect(sameContextUsage(null, base)).toBe(false);
  });
});

describe("Roster context ring", () => {
  const sessions: DiscoveredSessionSummary[] = [
    { id: "s1", source: "route", adapter: "claude", persona: "ferris", status: "active" },
    { id: "s2", source: "route", adapter: "codex", persona: "jarvis", status: "active" },
  ];

  function renderRoster(usage: Record<string, SessionContextUsage | null>) {
    render(
      <RosterSidebar
        agents={DEFAULT_WORKSPACE_AGENTS}
        routes={sessions}
        liveness={[]}
        unreadByAgent={{}}
        contextUsageByAgent={usage}
        selectedAgentName="ferris"
        onSelectAgent={vi.fn()}
      />,
    );
  }

  it("marks an assumed maximum in the ring and its title", () => {
    renderRoster({
      ferris: { used_tokens: 420_000, max_tokens: 1_000_000, source: "assumed" },
      jarvis: { used_tokens: 100_000, max_tokens: 256_000, source: "reported" },
    });
    const ferris = screen.getByTestId("context-ring-ferris");
    expect(ferris).toHaveAttribute("data-source", "assumed");
    expect(ferris).toHaveAttribute("data-percent", "42");
    expect(ferris).toHaveTextContent("~42%");
    expect(ferris).toHaveAttribute("title", "Context used: 42% (420k / about 1m, assumed)");
    const jarvis = screen.getByTestId("context-ring-jarvis");
    expect(jarvis).toHaveTextContent(/^39%$/);
    expect(jarvis).toHaveAttribute("title", "Context used: 39% (100k / 256k tokens)");
  });

  it("shows no percent and no maximum when the hub sent no usage", () => {
    renderRoster({});
    for (const key of ["ferris", "jarvis"]) {
      const ring = screen.getByTestId(`context-ring-${key}`);
      expect(ring).not.toHaveAttribute("data-percent");
      expect(ring).toHaveTextContent("n/a");
      expect(ring).not.toHaveTextContent("%");
      expect(ring).toHaveAttribute("title", "Context: no data from the hub");
    }
  });
});

describe("FerroWatch context cell", () => {
  function summary(): WatchSummaryResponse {
    const nowMs = Date.now();
    return {
      telemetry: null,
      health: {
        thresholds: { stale_after_ms: 60_000, dead_after_ms: 300_000 },
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
            heartbeat_at_ms: nowMs,
            heartbeat_age_ms: 0,
            node: "local",
            hosted_session: {
              session_id: "claude:ferris",
              persona: "ferris",
              project: "FerroLoop",
              role: "lead",
              vendor: "claude",
              model: "claude-opus-5-5",
              node_id: "local",
              status: "running",
              restart_count: 0,
              updated_at_ms: nowMs,
            },
          },
          {
            agent: "jarvis",
            project: "FerroLoop",
            adapter: "codex",
            active: true,
            liveness: "live",
            verdict: { state: "alive" },
            heartbeat_at_ms: nowMs,
            heartbeat_age_ms: 0,
            node: "local",
            hosted_session: null,
          },
          {
            agent: "wendy",
            project: "FerroLoop",
            adapter: "grok",
            active: true,
            liveness: "live",
            verdict: { state: "alive" },
            heartbeat_at_ms: nowMs,
            heartbeat_age_ms: 0,
            node: "local",
            hosted_session: null,
          },
        ],
      },
      quotas: [],
    } as unknown as WatchSummaryResponse;
  }

  function renderWatch(sessions: DiscoveredSessionSummary[]) {
    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const body = url.includes("/api/v1/watch/summary") ? summary() : {};
      return new Response(JSON.stringify(body), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    });
    const apiClient = new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    render(
      <FerroWatchPane
        authority={{ tier: "owner", login: "lmcfarlin" }}
        apiClient={apiClient}
        sessions={sessions}
      />,
    );
  }

  it("shows used / max with the source marker per agent and says so when nothing was reported", async () => {
    renderWatch([
      {
        id: "claude:ferris",
        source: "route",
        adapter: "claude",
        persona: "ferris",
        status: "active",
        context_usage: { used_tokens: 120_000, max_tokens: 1_000_000, source: "assumed" },
      },
      {
        id: "codex:jarvis",
        source: "route",
        adapter: "codex",
        persona: "jarvis",
        status: "active",
        context_usage: { used_tokens: 5_000, max_tokens: 272_000, source: "catalog" },
      },
    ]);
    const ferris = await screen.findByTestId("watch-liveness-context-ferris");
    expect(ferris).toHaveTextContent("Context: 120k / about 1m, assumed");
    expect(screen.getByTestId("watch-liveness-context-max-ferris")).toHaveAttribute(
      "title",
      "No vendor file or setting names this session's window; this is a default",
    );
    const jarvis = screen.getByTestId("watch-liveness-context-jarvis");
    expect(jarvis).toHaveTextContent(/^Context: 5k \/ 272k$/);
    expect(screen.getByTestId("watch-liveness-context-wendy")).toHaveTextContent(
      "Context: not reported",
    );
  });
});

describe("formatTokenCount", () => {
  it.each([
    [0, "0"],
    [999, "999"],
    [1_000, "1k"],
    [1_234, "1.23k"],
    [12_345, "12.3k"],
    [1_500, "1.5k"],
    [200_000, "200k"],
    [256_000, "256k"],
    [272_000, "272k"],
    [500_000, "500k"],
    [262_144, "262k"],
    [828_400, "828k"],
    [999_999, "1m"],
    [1_000_000, "1m"],
    [1_007_997, "1.01m"],
    [1_048_576, "1.05m"],
    [1_500_000, "1.5m"],
    [2_000_000, "2m"],
  ])("formats %i as %s", (value, text) => {
    expect(formatTokenCount(value)).toBe(text);
  });
});

describe("Context meter accessibility", () => {
  it.each([
    ["assumed", /50%.*against an assumed window/],
    ["reported", /50%.*reported/],
    ["catalog", /50%.*catalog/],
  ] as const)("gives the %s progressbar an aria-valuetext", (source, pattern) => {
    render(
      <AgentSettingsModal {...modalProps({ used_tokens: 500, max_tokens: 1_000, source })} />,
    );
    expect(screen.getByRole("progressbar")).toHaveAttribute(
      "aria-valuetext",
      expect.stringMatching(pattern),
    );
  });

  it("names no source in aria-valuetext when an older hub omits it", () => {
    render(<AgentSettingsModal {...modalProps({ used_tokens: 500, max_tokens: 1_000 })} />);
    const text = screen.getByRole("progressbar").getAttribute("aria-valuetext") ?? "";
    expect(text).toMatch(/50%/);
    expect(text).not.toMatch(/assumed|reported|catalog/);
  });
});

describe("Compact with an unknown window", () => {
  const turns = [
    {
      index: 0,
      role: "user" as const,
      content: "Hello",
      thinking: null,
      tool_calls: [],
      created_at: null,
    },
  ];

  function renderPane(
    usage: SessionContextUsage | null,
    onSendPrompt: () => Promise<void> = async () => undefined,
  ) {
    return render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="claude"
        sessionId="sess-1"
        turns={turns}
        contextUsage={usage}
        agentSettingsOpen={true}
        onSendPrompt={onSendPrompt}
      />,
    );
  }

  it("is enabled with no context_usage and says the window is unknown", () => {
    renderPane(null);
    const btn = screen.getByTestId("compact-context-btn") as HTMLButtonElement;
    expect(btn).not.toBeDisabled();
    expect(btn.title).toBe("Context window unknown \u2014 compact anyway");
  });

  it("is enabled when the server window is 0 (no window named)", () => {
    renderPane({ used_tokens: 10, max_tokens: 0 });
    expect(screen.getByTestId("compact-context-btn")).not.toBeDisabled();
  });

  it("enables at exactly 500k of an assumed 1,000,000 window and not at 499,999", () => {
    const { unmount } = renderPane({
      used_tokens: 499_999,
      max_tokens: 1_000_000,
      source: "assumed",
    });
    const low = screen.getByTestId("compact-context-btn") as HTMLButtonElement;
    expect(low).toBeDisabled();
    expect(low.title).toBe("Available when context reaches 50%");
    unmount();
    renderPane({ used_tokens: 500_000, max_tokens: 1_000_000, source: "assumed" });
    expect(screen.getByTestId("compact-context-btn")).not.toBeDisabled();
  });

  it("stays disabled below 50% of a reported window", () => {
    renderPane({ used_tokens: 40_000, max_tokens: 200_000, source: "reported" });
    const btn = screen.getByTestId("compact-context-btn") as HTMLButtonElement;
    expect(btn).toBeDisabled();
    expect(btn.title).toBe("Available when context reaches 50%");
  });

  it("is disabled while a compaction is in flight, even with no window", async () => {
    const onSendPrompt = vi.fn(() => new Promise<void>(() => undefined));
    renderPane(null, onSendPrompt);
    fireEvent.click(screen.getByTestId("compact-context-btn"));
    await waitFor(() => expect(onSendPrompt).toHaveBeenCalled());
    const btn = screen.getByTestId("compact-context-btn");
    expect(btn).toBeDisabled();
    expect(btn).toHaveTextContent("Compacting...");
  });
});

describe("Roster ring with a transcript estimate", () => {
  it("reads '~Nk tok' where a percent normally sits", () => {
    const text = "x".repeat(4 * 12_000);
    render(
      <RosterSidebar
        agents={DEFAULT_WORKSPACE_AGENTS}
        routes={[{ id: "s1", source: "route", adapter: "claude", persona: "ferris", status: "active" }]}
        liveness={[]}
        unreadByAgent={{}}
        contextUsageByAgent={{}}
        transcriptsByAgent={{
          ferris: [
            { index: 0, role: "user", content: text, thinking: null, tool_calls: [], created_at: null },
          ],
        }}
        selectedAgentName="ferris"
        onSelectAgent={vi.fn()}
      />,
    );
    const ring = screen.getByTestId("context-ring-ferris");
    expect(ring).toHaveTextContent(/^~\d+(\.\d+)?k tok$/);
    expect(screen.getByText(/tok$/)).toHaveStyle({ whiteSpace: "nowrap" });
    expect(ring).not.toHaveAttribute("data-percent");
  });
});

describe("FerroWatch persona fallback across vendors", () => {
  it("does not show another vendor's usage for a shared persona", async () => {
    const nowMs = Date.now();
    const item = (agent: string, adapter: string) => ({
      agent,
      project: "FerroLoop",
      adapter,
      active: true,
      liveness: "live",
      verdict: { state: "alive" },
      heartbeat_at_ms: nowMs,
      heartbeat_age_ms: 0,
      node: "local",
      hosted_session: null,
    });
    const summary = {
      telemetry: null,
      health: {
        thresholds: { stale_after_ms: 60_000, dead_after_ms: 300_000 },
        hosted_sessions: 0,
        hosted_sessions_undecodable: 0,
        heartbeats_undecodable: 0,
        agents: [item("ferris", "codex")],
      },
      quotas: [],
    } as unknown as WatchSummaryResponse;
    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const body = String(input).includes("/api/v1/watch/summary") ? summary : {};
      return new Response(JSON.stringify(body), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    });
    const apiClient = new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    render(
      <FerroWatchPane
        authority={{ tier: "owner", login: "lmcfarlin" }}
        apiClient={apiClient}
        sessions={[
          {
            id: "claude:ferris",
            source: "route",
            adapter: "claude",
            persona: "ferris",
            status: "active",
            context_usage: { used_tokens: 120_000, max_tokens: 1_000_000, source: "assumed" },
          },
          {
            id: "codex:ferris",
            source: "route",
            adapter: "Codex",
            persona: "ferris",
            status: "active",
            context_usage: { used_tokens: 5_000, max_tokens: 272_000, source: "catalog" },
          },
        ]}
      />,
    );
    const cell = await screen.findByTestId("watch-liveness-context-ferris");
    expect(cell).toHaveTextContent(/^Context: 5k \/ 272k$/);
  });
});

describe("FerroWatch persona match uses the displayed vendor", () => {
  type Sess = { id: string; adapter: string | null; used: number };
  async function renderCell(
    item: { adapter: string | null; hosted: { vendor: string; session_id: string } | null },
    sessions: Sess[],
  ) {
    const nowMs = Date.now();
    const summary = {
      telemetry: null,
      health: {
        thresholds: { stale_after_ms: 60_000, dead_after_ms: 300_000 },
        hosted_sessions: 0,
        hosted_sessions_undecodable: 0,
        heartbeats_undecodable: 0,
        agents: [
          {
            agent: "ferris",
            project: "FerroLoop",
            adapter: item.adapter,
            active: true,
            liveness: "live",
            verdict: { state: "alive" },
            heartbeat_at_ms: nowMs,
            heartbeat_age_ms: 0,
            node: "local",
            hosted_session: item.hosted
              ? {
                  session_id: item.hosted.session_id,
                  persona: "ferris",
                  project: "FerroLoop",
                  role: "lead",
                  vendor: item.hosted.vendor,
                  model: "m",
                  node_id: "local",
                  status: "running",
                  restart_count: 0,
                  updated_at_ms: nowMs,
                }
              : null,
          },
        ],
      },
      quotas: [],
    } as unknown as WatchSummaryResponse;
    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const body = String(input).includes("/api/v1/watch/summary") ? summary : {};
      return new Response(JSON.stringify(body), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    });
    const apiClient = new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    render(
      <FerroWatchPane
        authority={{ tier: "owner", login: "lmcfarlin" }}
        apiClient={apiClient}
        sessions={sessions.map((s) => ({
          id: s.id,
          source: "route",
          adapter: s.adapter,
          persona: "ferris",
          status: "active",
          context_usage: { used_tokens: s.used, max_tokens: 1_000_000, source: "catalog" },
        })) as unknown as DiscoveredSessionSummary[]}
      />,
    );
    return screen.findByTestId("watch-liveness-context-ferris");
  }

  it("keeps the hosted claude session's usage when a codex heartbeat shares the persona", async () => {
    const cell = await renderCell(
      { adapter: "codex", hosted: { vendor: "claude", session_id: "gone:ferris" } },
      [
        { id: "codex:ferris", adapter: "codex", used: 5_000 },
        { id: "claude:ferris", adapter: "claude", used: 120_000 },
      ],
    );
    expect(cell).toHaveTextContent(/^Context: 120k \/ 1m$/);
  });

  it("picks the session of the displayed vendor when two share a persona", async () => {
    const cell = await renderCell({ adapter: "claude", hosted: null }, [
      { id: "codex:ferris", adapter: "codex", used: 5_000 },
      { id: "claude:ferris", adapter: "claude", used: 120_000 },
    ]);
    expect(cell).toHaveTextContent(/^Context: 120k \/ 1m$/);
  });

  it("compares against the hosted vendor when the row adapter is null", async () => {
    const cell = await renderCell(
      { adapter: null, hosted: { vendor: "claude", session_id: "gone:ferris" } },
      [
        { id: "codex:ferris", adapter: "codex", used: 5_000 },
        { id: "claude:ferris", adapter: "claude", used: 120_000 },
      ],
    );
    expect(cell).toHaveTextContent(/^Context: 120k \/ 1m$/);
  });

  it("matches the persona when neither the row nor the session names a vendor", async () => {
    const cell = await renderCell({ adapter: null, hosted: null }, [
      { id: "ferris", adapter: null, used: 7_000 },
    ]);
    expect(cell).toHaveTextContent(/^Context: 7k \/ 1m$/);
  });

  it("treats the antigravity aliases as one vendor", async () => {
    const cell = await renderCell({ adapter: "agy", hosted: null }, [
      { id: "claude:ferris", adapter: "claude", used: 120_000 },
      { id: "antigravity:ferris", adapter: "antigravity", used: 9_000 },
    ]);
    expect(cell).toHaveTextContent(/^Context: 9k \/ 1m$/);
  });
});
