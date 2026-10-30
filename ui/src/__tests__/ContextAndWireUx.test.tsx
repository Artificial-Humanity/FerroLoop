import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import {
  App,
  computeUnreadByAgent,
  countUnseenAssistantTurns,
  isUserDirectedAssistantTurn,
  LAST_SEEN_ASSISTANT_TURN_STORAGE_KEY,
  preserveOptimisticTurn,
  THEME_STORAGE_KEY,
  transcriptPollIntervalMs,
} from "../App";
import type { HubWsClient } from "../api/ws";
import {
  ChatPane,
  computeLastTurnKey,
  formatTokenCount,
  estimateSessionContextUsage,
  hasNewTurnActivity,
} from "../components/ChatPane";
import { ThinkingBlock } from "../components/ThinkingBlock";
import { summarizeWireContent } from "../components/TurnCard";
import type { TranscriptTurn } from "../types/api";

function turn(partial: Partial<TranscriptTurn> & { index: number }): TranscriptTurn {
  return {
    role: "assistant",
    content: "",
    thinking: null,
    tool_calls: [],
    ...partial,
  };
}

describe("collapsible Wire cards", () => {
  it("collapses wire turns by default with a Subject summary and expands on toggle", () => {
    const wireTurns: TranscriptTurn[] = [
      turn({
        index: 3,
        role: "wire:jarvis",
        content: [
          "Subject: PR #61 approved — ready to merge",
          "",
          "CI is green on the branch.",
          "Merge when ready.",
        ].join("\n"),
      }),
    ];

    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="claude"
        sessionId="sess-ferris-01"
        turns={wireTurns}
        onSendPrompt={vi.fn()}
      />,
    );

    const card = screen.getByTestId("turn-card-3");
    const toggle = within(card).getByTestId("wire-output-toggle");
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(toggle).toHaveTextContent("Wire · jarvis — PR #61 approved — ready to merge");
    expect(toggle).toHaveTextContent("(4 lines)");
    expect(within(card).queryByText(/CI is green on the branch/)).toBeNull();

    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(within(card).getByText(/CI is green on the branch/)).toBeInTheDocument();

    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(within(card).queryByText(/CI is green on the branch/)).toBeNull();
  });

  it("falls back to the first non-empty line when no Subject is present", () => {
    const wireTurns: TranscriptTurn[] = [
      turn({
        index: 0,
        role: "wire:felicia",
        content: "\nBackend PR is up for the state endpoints.\nDetails follow here.",
      }),
    ];

    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="claude"
        sessionId="sess-ferris-01"
        turns={wireTurns}
        onSendPrompt={vi.fn()}
      />,
    );

    const toggle = screen.getByTestId("wire-output-toggle");
    expect(toggle).toHaveTextContent("Backend PR is up for the state endpoints.");
    expect(screen.queryByText(/Details follow here/)).toBeNull();
  });

  it("truncates a >80-char Subject with an ellipsis", () => {
    const subject = "x".repeat(100);
    const summary = summarizeWireContent(`Subject: ${subject}\nBody line.`, "jarvis");
    expect(summary).toBe(`Wire · jarvis — ${"x".repeat(80)}… (2 lines)`);
    expect(summary).not.toContain(subject);
  });
});

describe("background task output guard", () => {
  it("renders mislabeled background task completions as collapsed Tool Output, not Operator", () => {
    const turns: TranscriptTurn[] = [
      turn({
        index: 5,
        role: "user",
        content: [
          'Task id "bash-42" finished with result:',
          "Command exited with code 0.",
          "",
          "Command output:",
          "cargo test passed 42 tests",
        ].join("\n"),
      }),
    ];

    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="claude"
        sessionId="sess-ferris-01"
        turns={turns}
        onSendPrompt={vi.fn()}
      />,
    );

    expect(screen.getByText("Tool Output")).toBeInTheDocument();
    expect(screen.queryByText("Operator")).toBeNull();
    const toggle = screen.getByTestId("turn-tool-output-toggle-5");
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(toggle).toHaveTextContent(/Tool \/ Terminal Output \(5 lines\)/);
    expect(toggle).toHaveTextContent(/Command exited with code 0/);
    expect(screen.queryByText(/cargo test passed 42 tests/)).toBeNull();

    fireEvent.click(toggle);
    expect(screen.getByText(/cargo test passed 42 tests/)).toBeInTheDocument();
  });
});

describe("estimateSessionContextUsage", () => {
  it("returns zero usage for an empty transcript", () => {
    expect(estimateSessionContextUsage([], "claude")).toEqual({
      usedTokens: 0,
      maxTokens: 1_000_000,
      percent: 0,
    });
  });

  it("estimates overhead plus content, thinking, and tool calls below 50%", () => {
    const usage = estimateSessionContextUsage(
      [turn({ index: 0, content: "x".repeat(400) })],
      null,
    );
    // 12_000 overhead + ceil((400 + 0 + 2) / 4) for content + "[]" tool calls.
    expect(usage.usedTokens).toBe(12_101);
    expect(usage.maxTokens).toBe(200_000);
    expect(usage.percent).toBe(6);
  });

  it("crosses the 50% threshold on a large transcript and caps at 100%", () => {
    const usage = estimateSessionContextUsage(
      [turn({ index: 0, content: "x".repeat(360_000) })],
      null,
    );
    expect(usage.percent).toBe(51);
    expect(usage.percent).toBeGreaterThanOrEqual(50);

    const capped = estimateSessionContextUsage(
      [turn({ index: 0, content: "x".repeat(2_000_000) })],
      null,
    );
    expect(capped.percent).toBe(100);
  });

  it("resets the estimate at the last compaction boundary", () => {
    const usage = estimateSessionContextUsage(
      [
        turn({ index: 0, content: "x".repeat(360_000) }),
        turn({ index: 1, content: "[Compacted context] Earlier work summary." }),
        turn({ index: 2, content: "Next step." }),
      ],
      null,
    );
    // Only the boundary turn and the short follow-up count toward usage.
    expect(usage.usedTokens).toBeLessThan(13_000);
    expect(usage.percent).toBeLessThan(50);
  });

  it("uses 1M windows for antigravity-family, muse, and claude adapters and 500k for grok and codex", () => {
    for (const adapter of [
      "antigravity",
      "agy",
      "argon",
      "gemini",
      "muse",
      "muse-wonder",
      "muse-wonder-full",
      "claude",
      "claude-sonnet-4-5",
      "claude-haiku-5-5",
    ]) {
      expect(estimateSessionContextUsage([], adapter).maxTokens).toBe(1_000_000);
    }
    for (const adapter of ["codex", "grok"]) {
      expect(estimateSessionContextUsage([], adapter).maxTokens).toBe(500_000);
    }
    expect(estimateSessionContextUsage([], null).maxTokens).toBe(200_000);
    expect(formatTokenCount(1_000_000)).toBe("1m");
    expect(formatTokenCount(1_500_000)).toBe("1.5m");
    expect(formatTokenCount(500_000)).toBe("500k");
    expect(formatTokenCount(200_000)).toBe("200k");
  });

  it("prefers server usage and uses 500k context defaults for grok and codex", () => {
    expect(
      estimateSessionContextUsage([turn({ index: 0, content: "hi" })], "grok"),
    ).toMatchObject({ maxTokens: 500_000 });
    expect(
      estimateSessionContextUsage([turn({ index: 0, content: "hi" })], "codex"),
    ).toMatchObject({ maxTokens: 500_000 });

    expect(
      estimateSessionContextUsage([], "claude", {
        used_tokens: 150_000,
        max_tokens: 200_000,
      }),
    ).toEqual({ usedTokens: 150_000, maxTokens: 200_000, percent: 75 });
  });
});

describe("Compact Context button", () => {
  it("shows the meter and keeps compact disabled below 50%", () => {
    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="claude"
        sessionId="sess-ferris-01"
        turns={[turn({ index: 0, role: "user", content: "Hello" })]}
        onSendPrompt={vi.fn()}
      />,
    );

    expect(screen.getByTestId("context-level-meter")).toHaveTextContent(/Context:/);
    const btn = screen.getByTestId("compact-context-btn") as HTMLButtonElement;
    expect(btn).toBeDisabled();
    expect(btn.title).toMatch(/50%/);
  });

  it("enables at >= 50%, checkpoints best-effort, and dispatches /compact immediately", async () => {
    const checkpointCalls: string[] = [];
    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === "/api/v1/state/checkpoint" && init?.method === "POST") {
        checkpointCalls.push(url);
        expect(JSON.parse(String(init.body))).toEqual({
          summary: "Pre-compaction automatic checkpoint from FerroDeck",
        });
        return new Response(
          JSON.stringify({
            entry: { id: "e1" },
            branch: "refs/heads/fl/state",
            head_sha: "abc1234",
            dirty_files: 0,
          }),
          { status: 200 },
        );
      }
      return new Response(JSON.stringify({ error: "not found" }), { status: 404 });
    });
    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const onSendPrompt = vi.fn(async () => undefined);

    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter={null}
        sessionId="sess-ferris-01"
        turns={[turn({ index: 0, role: "user", content: "x".repeat(360_000) })]}
        onSendPrompt={onSendPrompt}
        apiClient={apiClient}
      />,
    );

    expect(screen.getByTestId("context-level-meter")).toHaveTextContent(/\(51%\)/);
    const btn = screen.getByTestId("compact-context-btn") as HTMLButtonElement;
    expect(btn).not.toBeDisabled();

    fireEvent.click(btn);

    await waitFor(() => {
      expect(onSendPrompt).toHaveBeenCalledWith(
        "sess-ferris-01",
        expect.objectContaining({ prompt: "/compact", send_now: true }),
      );
    });
    expect(checkpointCalls).toEqual(["/api/v1/state/checkpoint"]);
    expect(await screen.findByTestId("compact-feedback")).toHaveTextContent("Sent /compact");
  });

  it("still dispatches /compact when the checkpoint call fails", async () => {
    const fetchImpl = vi.fn(async () =>
      new Response(JSON.stringify({ error: "checkpoint unavailable" }), { status: 500 }),
    );
    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const onSendPrompt = vi.fn(async () => undefined);

    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter={null}
        sessionId="sess-ferris-01"
        turns={[turn({ index: 0, role: "user", content: "x".repeat(360_000) })]}
        onSendPrompt={onSendPrompt}
        apiClient={apiClient}
      />,
    );

    fireEvent.click(screen.getByTestId("compact-context-btn"));

    await waitFor(() => {
      expect(onSendPrompt).toHaveBeenCalledWith(
        "sess-ferris-01",
        expect.objectContaining({ prompt: "/compact", send_now: true }),
      );
    });
  });
});

describe("ThinkingBlock live activity", () => {
  it('shows the collapsed "Thinking..." label and expands to thinking-content', () => {
    render(<ThinkingBlock thinking="First inspect the hub health endpoint." />);

    const toggle = screen.getByRole("button", { name: /Thinking\.\.\./i });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByTestId("thinking-content")).toBeNull();
    expect(screen.queryByTestId("thinking-activity-dot")).toBeNull();

    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByTestId("thinking-content")).toHaveTextContent(
      "First inspect the hub health endpoint.",
    );
  });

  it("renders the activity animation dot only when isActive", () => {
    const { rerender } = render(
      <ThinkingBlock thinking="Working through the plan." isActive />,
    );
    expect(screen.getByTestId("thinking-block")).toHaveAttribute("data-active", "true");
    expect(screen.getByTestId("thinking-activity-dot")).toBeInTheDocument();

    rerender(<ThinkingBlock thinking="Working through the plan." />);
    expect(screen.getByTestId("thinking-block")).toHaveAttribute("data-active", "false");
    expect(screen.queryByTestId("thinking-activity-dot")).toBeNull();
  });

  it("shows a live thinking indicator while busy when the last turn has no thinking", () => {
    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="claude"
        sessionId="sess-ferris-01"
        turns={[turn({ index: 0, role: "user", content: "Ship it" })]}
        isBusy
        onSendPrompt={vi.fn()}
      />,
    );

    const live = screen.getByTestId("live-thinking-indicator");
    expect(within(live).getByText(/Thinking\.\.\./)).toBeInTheDocument();
    expect(within(live).getByTestId("thinking-activity-dot")).toBeInTheDocument();
  });

  it("marks the last turn active instead of a live indicator when it already thinks", () => {
    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="claude"
        sessionId="sess-ferris-01"
        turns={[
          turn({ index: 0, role: "user", content: "Ship it" }),
          turn({ index: 1, content: "Working", thinking: "Planning the change." }),
        ]}
        isBusy
        onSendPrompt={vi.fn()}
      />,
    );

    expect(screen.queryByTestId("live-thinking-indicator")).toBeNull();
    expect(screen.getByTestId("thinking-activity-dot")).toBeInTheDocument();
  });

  it("shows no live indicator when idle", () => {
    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="claude"
        sessionId="sess-ferris-01"
        turns={[turn({ index: 0, role: "user", content: "Ship it" })]}
        onSendPrompt={vi.fn()}
      />,
    );

    expect(screen.queryByTestId("live-thinking-indicator")).toBeNull();
    expect(screen.queryByTestId("thinking-activity-dot")).toBeNull();
  });
});

describe("windowed transcript auto-scroll keys", () => {
  const windowed = (lastIndex: number, lastContent = "done") =>
    Array.from({ length: 250 }, (_, i) =>
      turn({
        index: lastIndex - 249 + i,
        content: i === 249 ? lastContent : `turn ${i}`,
      }),
    );

  it("detects activity when the 250-turn window slides at constant length", () => {
    const before = windowed(400);
    const after = windowed(401);
    expect(after.length).toBe(before.length);
    expect(
      hasNewTurnActivity(before.length, computeLastTurnKey(before), after),
    ).toBe(true);
  });

  it("detects streaming growth of the last turn and ignores an unchanged tail", () => {
    const before = windowed(400, "partial");
    const grown = windowed(400, "partial plus more streamed text");
    expect(
      hasNewTurnActivity(before.length, computeLastTurnKey(before), grown),
    ).toBe(true);
    expect(
      hasNewTurnActivity(before.length, computeLastTurnKey(before), before),
    ).toBe(false);
    expect(hasNewTurnActivity(0, "", [])).toBe(false);
  });
});

describe("optimistic prompt preservation", () => {
  const diskTurns = [turn({ index: 7, role: "assistant", content: "Earlier reply" })];

  it("keeps a fresh optimistic user turn that disk does not have yet", () => {
    const merged = preserveOptimisticTurn(
      diskTurns,
      { content: "Ship it", sentAt: 1_000 },
      3_000,
    );
    expect(merged).toHaveLength(2);
    expect(merged[1]).toMatchObject({ role: "user", content: "Ship it" });
  });

  it("drops the optimistic turn once it reaches the disk tail or ages out past 60s", () => {
    const onDisk = [
      ...diskTurns,
      turn({ index: 8, role: "user", content: "Ship it" }),
    ];
    expect(
      preserveOptimisticTurn(onDisk, { content: "Ship it", sentAt: 1_000 }, 3_000),
    ).toBe(onDisk);
    expect(
      preserveOptimisticTurn(diskTurns, { content: "Ship it", sentAt: 1_000 }, 30_000),
    ).toHaveLength(2);
    expect(
      preserveOptimisticTurn(diskTurns, { content: "Ship it", sentAt: 1_000 }, 62_000),
    ).toBe(diskTurns);
    expect(preserveOptimisticTurn(diskTurns, null, 3_000)).toBe(diskTurns);
  });

  it("does not append the optimistic turn after switching agents", () => {
    expect(
      preserveOptimisticTurn(
        diskTurns,
        { content: "Ship it", sentAt: 1_000, agentKey: "ferris" },
        3_000,
        "jarvis",
      ),
    ).toBe(diskTurns);
    const merged = preserveOptimisticTurn(
      diskTurns,
      { content: "Ship it", sentAt: 1_000, agentKey: "ferris" },
      3_000,
      "ferris",
    );
    expect(merged).toHaveLength(2);
    expect(merged[1]).toMatchObject({ role: "user", content: "Ship it" });
  });

  it("recognizes an on-disk user turn that wraps the prompt via substring match", () => {
    const wrappedOnDisk = [
      ...diskTurns,
      turn({ index: 8, role: "user", content: "<USER_REQUEST>\nShip it\n</USER_REQUEST>" }),
    ];
    expect(
      preserveOptimisticTurn(
        wrappedOnDisk,
        { content: "Ship it", sentAt: 1_000 },
        3_000,
      ),
    ).toBe(wrappedOnDisk);
  });

  it("polls faster while the session is busy", () => {
    expect(transcriptPollIntervalMs(true)).toBe(2_000);
    expect(transcriptPollIntervalMs(false)).toBe(3_500);
  });
});

describe("roster unread badges", () => {
  const jarvisTurns: TranscriptTurn[] = [
    turn({ index: 1, content: "First status for the operator." }),
    turn({ index: 2, role: "wire:ferris", content: "Subject: ping\nAgent mail." }),
    turn({ index: 3, role: "tool", content: "Created At: t\ntool bytes" }),
    turn({ index: 4, content: "", thinking: "Quiet reasoning only." }),
    turn({ index: 5, content: "Created At: t\nCommand output: done" }),
  ];

  it("counts only user-directed assistant prose as unread", () => {
    expect(isUserDirectedAssistantTurn(jarvisTurns[0])).toBe(true);
    expect(isUserDirectedAssistantTurn(jarvisTurns[1])).toBe(false);
    expect(isUserDirectedAssistantTurn(jarvisTurns[2])).toBe(false);
    expect(isUserDirectedAssistantTurn(jarvisTurns[3])).toBe(false);
    expect(isUserDirectedAssistantTurn(jarvisTurns[4])).toBe(false);
    expect(countUnseenAssistantTurns(jarvisTurns, 0)).toBe(1);
    expect(countUnseenAssistantTurns(jarvisTurns, 1)).toBe(0);
    expect(countUnseenAssistantTurns(jarvisTurns, undefined)).toBe(0);
    expect(computeUnreadByAgent({ jarvis: jarvisTurns }, { jarvis: 0 })).toEqual({
      jarvis: 1,
    });
  });

  it("badges a background agent for new assistant prose and clears on select", async () => {
    window.localStorage.removeItem(LAST_SEEN_ASSISTANT_TURN_STORAGE_KEY);
    let jarvisStamp = "t1";
    let liveJarvisTurns = [...jarvisTurns];

    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const json = (body: unknown) =>
        new Response(JSON.stringify(body), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      if (url.includes("/api/v1/auth/session")) {
        return json({
          authenticated: true,
          authority: { tier: "owner", login: "lmcfarlin" },
        });
      }
      if (url.includes("/api/v1/wire/messages")) return json({ messages: [] });
      if (url.includes("/sess-jarvis/transcript")) {
        return json({ session_id: "sess-jarvis", turns: liveJarvisTurns, is_busy: false, queue: [] });
      }
      if (url.includes("/transcript")) {
        return json({ session_id: "sess-ferris", turns: [], is_busy: false, queue: [] });
      }
      if (url.includes("/api/v1/agents/liveness")) return json({ liveness: [] });
      if (url.includes("/api/v1/agents")) return json({ agents: [], areas: [], routes: [] });
      if (url.includes("/api/v1/sessions")) {
        return json({
          sessions: [
            { id: "sess-jarvis", source: "route", adapter: "claude", persona: "jarvis", status: "active", updated_at: jarvisStamp },
            { id: "sess-ferris", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" },
          ],
        });
      }
      if (url.includes("/api/v1/worktrees")) return json({ worktrees: [] });
      if (url.includes("/api/v1/prs")) return json({ prs: [] });
      if (url.includes("/api/v1/workflows")) return json({ workflows: [] });
      if (url.includes("/api/v1/workflow-runs")) return json({ runs: [] });
      if (url.includes("/api/v1/architecture")) {
        return json({ model: null, likec4_dsl: null, conformance: null });
      }
      if (url.includes("/api/v1/stats")) {
        return json({
          attempts: 0,
          by_status: {},
          wall_clock_ms: 0,
          tokens_in: 0,
          tokens_out: 0,
          cost_usd_micros: 0,
          zero_cost_attempts: 0,
          by_adapter: {},
          regret_audit: [],
        });
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

    // First observation establishes the baseline: history never badges.
    await waitFor(() => {
      expect(
        fetchImpl.mock.calls.some(([input]) =>
          String(input).includes("/sess-jarvis/transcript"),
        ),
      ).toBe(true);
    });
    expect(screen.queryByTestId("unread-badge-jarvis")).toBeNull();

    // A new user-directed assistant turn lands on disk for jarvis.
    liveJarvisTurns = [
      ...liveJarvisTurns,
      turn({ index: 6, content: "New results are ready for the operator." }),
    ];
    jarvisStamp = "t2";
    fireEvent.click(screen.getByRole("button", { name: /^Refresh$/i }));

    expect(await screen.findByTestId("unread-badge-jarvis")).toHaveTextContent("1");

    fireEvent.click(screen.getByTestId("agent-row-jarvis"));

    await waitFor(() => {
      expect(screen.queryByTestId("unread-badge-jarvis")).toBeNull();
    });
    expect(
      window.localStorage.getItem(LAST_SEEN_ASSISTANT_TURN_STORAGE_KEY),
    ).toContain('\"jarvis\":6');

    unmount();
  });
});

describe("App theme toggle", () => {
  it("switches document theme between dark and light and persists it", async () => {
    window.localStorage.removeItem(THEME_STORAGE_KEY);
    document.documentElement.removeAttribute("data-theme");

    const fetchImpl = vi.fn(async () =>
      new Response(JSON.stringify({}), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
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

    const toggle = await screen.findByTestId("theme-toggle-btn");
    expect(document.documentElement.dataset.theme).toBe("dark");
    // Applying the system-derived default must not persist it.
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBeNull();
    expect(toggle).toHaveTextContent(/Light/);

    fireEvent.click(toggle);
    expect(document.documentElement.dataset.theme).toBe("light");
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("light");
    expect(toggle).toHaveTextContent(/Dark/);

    fireEvent.click(toggle);
    expect(document.documentElement.dataset.theme).toBe("dark");
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("dark");

    unmount();
  });
});

describe("Agent switch transcript isolation", () => {
  it("immediately clears/swaps previous agent turns on switch and ignores stale in-flight poll responses", async () => {
    let resolvePendingFerrisPoll = null as ((res: Response) => void) | null;
    let resolvePendingJarvisFetch = null as ((res: Response) => void) | null;
    let holdFerrisPoll = false;
    let holdJarvisFetch = false;

    const fetchImpl = vi.fn((input: RequestInfo | URL) => {
      const url = String(input);
      const json = (body: unknown) =>
        Promise.resolve(
          new Response(JSON.stringify(body), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          }),
        );

      if (url.includes("/api/v1/auth/session")) {
        return json({
          authenticated: true,
          authority: { tier: "owner", login: "lmcfarlin" },
        });
      }
      if (url.includes("/api/v1/sessions") && !url.includes("/transcript")) {
        return json({
          sessions: [
            {
              id: "sess-ferris",
              source: "route",
              adapter: "claude",
              persona: "ferris",
              status: "active",
              updated_at: "t1",
            },
            {
              id: "sess-jarvis",
              source: "route",
              adapter: "claude",
              persona: "jarvis",
              status: "active",
              updated_at: "t1",
            },
          ],
        });
      }
      if (url.includes("/sess-ferris/transcript") || url.includes("/ferris/transcript")) {
        if (!holdFerrisPoll) {
          return json({
            session_id: "sess-ferris",
            turns: [
              turn({
                index: 1,
                role: "assistant",
                content: "Ferris initial message that must disappear on switch.",
              }),
            ],
            is_busy: false,
            queue: [],
          });
        }
        return new Promise<Response>((resolve) => {
          resolvePendingFerrisPoll = resolve;
        });
      }
      if (url.includes("/sess-jarvis/transcript") || url.includes("/jarvis/transcript")) {
        if (!holdJarvisFetch) {
          return json({
            session_id: "sess-jarvis",
            turns: [],
            is_busy: false,
            queue: [],
          });
        }
        return new Promise<Response>((resolve) => {
          resolvePendingJarvisFetch = resolve;
        });
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

    // Wait for Ferris's initial message and Refresh button to render
    expect(
      await screen.findByText(/Ferris initial message that must disappear on switch\./),
    ).toBeInTheDocument();
    const refreshBtn = await screen.findByRole("button", { name: /^Refresh$/i });

    // Hold subsequent Ferris poll and Jarvis fetch
    holdFerrisPoll = true;
    holdJarvisFetch = true;

    // Trigger a Refresh while on Ferris (starts an in-flight Ferris poll)
    fireEvent.click(refreshBtn);
    await waitFor(() => {
      expect(resolvePendingFerrisPoll).not.toBeNull();
    });

    // Switch to Jarvis while Jarvis's transcript fetch and Ferris's poll are still pending
    fireEvent.click(screen.getByTestId("agent-row-jarvis"));

    // Immediately upon switching to Jarvis, Ferris's message is no longer visible
    expect(
      screen.queryByText(/Ferris initial message that must disappear on switch\./),
    ).toBeNull();

    // Now resolve the stale in-flight Ferris poll — it must NOT overwrite Jarvis's view
    if (resolvePendingFerrisPoll) {
      resolvePendingFerrisPoll(
        new Response(
          JSON.stringify({
            session_id: "sess-ferris",
            turns: [
              turn({
                index: 2,
                role: "assistant",
                content: "Stale Ferris poll turn that must not leak into Jarvis.",
              }),
            ],
            is_busy: false,
            queue: [],
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        ),
      );
    }

    // Resolve Jarvis's transcript fetch
    await waitFor(() => {
      expect(resolvePendingJarvisFetch).not.toBeNull();
    });
    resolvePendingJarvisFetch!(
      new Response(
        JSON.stringify({
          session_id: "sess-jarvis",
          turns: [
            turn({
              index: 1,
              role: "assistant",
              content: "Jarvis lab manager transcript turn.",
            }),
          ],
          is_busy: false,
          queue: [],
        }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      ),
    );

    expect(
      await screen.findByText(/Jarvis lab manager transcript turn\./),
    ).toBeInTheDocument();
    expect(
      screen.queryByText(/Stale Ferris poll turn that must not leak into Jarvis\./),
    ).toBeNull();

    unmount();
  });
});
