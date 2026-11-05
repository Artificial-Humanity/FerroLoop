import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import "@testing-library/jest-dom/vitest";
import { sameContextUsage } from "../App";
import {
  AgentSettingsModal,
  type AgentSettingsModalProps,
} from "../components/AgentSettingsModal";
import { ChatPane, estimateSessionContextUsage } from "../components/ChatPane";
import { FerroWatchPane } from "../components/FerroWatchPane";
import { RosterSidebar } from "../components/RosterSidebar";
import { HubApiClient } from "../api/client";
import {
  DEFAULT_WORKSPACE_AGENTS,
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
      expect(screen.getByTestId("compact-context-btn")).toBeDisabled();
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
