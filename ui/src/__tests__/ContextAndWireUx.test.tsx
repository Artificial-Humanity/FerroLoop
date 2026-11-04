import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
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
    const { rerender } = render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="claude"
        sessionId="sess-ferris-01"
        turns={[turn({ index: 0, role: "user", content: "Hello" })]}
        onSendPrompt={vi.fn()}
      />,
    );

    expect(screen.queryByTestId("context-level-meter")).toBeNull();
    rerender(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="claude"
        sessionId="sess-ferris-01"
        turns={[turn({ index: 0, role: "user", content: "Hello" })]}
        agentSettingsOpen={true}
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
        agentSettingsOpen={true}
        onSendPrompt={onSendPrompt}
        apiClient={apiClient}
      />,
    );

    expect(screen.getByTestId("context-level-meter")).toHaveTextContent(/51%/);
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
        agentSettingsOpen={true}
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

  it("does not carry an optimistic prompt between sessions of the same agent", () => {
    const prompt = { content: "private to A", sentAt: 1_000, agentKey: "ferris", sessionId: "session-a" };
    expect(preserveOptimisticTurn(diskTurns, prompt, 3_000, "ferris", "session-b")).toBe(diskTurns);
    expect(preserveOptimisticTurn(diskTurns, prompt, 3_000, "ferris", "session-a")).toHaveLength(2);
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
  it("switches document theme between dark and light via Settings Accessibility tab and persists it", async () => {
    window.localStorage.removeItem(THEME_STORAGE_KEY);
    window.localStorage.removeItem("ferrodeck.theme");
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

    const settingsBtn = await screen.findByTestId("settings-modal-btn");
    expect(document.documentElement.dataset.theme).toBe("dark");
    // Applying the system-derived default must not persist it.
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBeNull();

    fireEvent.click(settingsBtn);
    fireEvent.click(screen.getByTestId("settings-tab-accessibility"));

    fireEvent.click(screen.getByTestId("theme-mode-light"));
    expect(document.documentElement.dataset.theme).toBe("light");
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("light");

    fireEvent.click(screen.getByTestId("theme-mode-dark"));
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

  it("resolves handleSendPrompt and clears Sending... state immediately without waiting for slow post-send session/transcript refresh", async () => {
    let holdRefresh = false;
    let resolvePendingSessionsRefresh = null as ((res: Response) => void) | null;
    let resolvePendingBgTranscript = null as ((res: Response) => void) | null;

    const fetchImpl = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
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
      if (url.includes("/prompt") && init?.method === "POST") {
        return json({
          status: "delivered",
          queued: false,
          is_busy: true,
          effective_body: "Fast operator prompt",
        });
      }
      if (url.includes("/api/v1/sessions") && !url.includes("/transcript")) {
        if (holdRefresh) {
          return new Promise<Response>((resolve) => {
            resolvePendingSessionsRefresh = resolve;
          });
        }
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
      if (url.includes("/sess-jarvis/transcript")) {
        if (holdRefresh) {
          return new Promise<Response>((resolve) => {
            resolvePendingBgTranscript = resolve;
          });
        }
        return json({
          session_id: "sess-jarvis",
          turns: [],
          is_busy: false,
          queue: [],
        });
      }
      if (url.includes("/transcript")) {
        return json({
          session_id: "sess-ferris",
          turns: [],
          is_busy: false,
          queue: [],
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

    const textarea = (await screen.findByPlaceholderText(
      /Send a message to Ferris/i,
    )) as HTMLTextAreaElement;
    const sendBtn = screen.getByTestId("send-prompt-btn");

    // Hold post-send /api/v1/sessions and background transcript refresh calls
    holdRefresh = true;

    fireEvent.change(textarea, { target: { value: "Fast operator prompt" } });
    fireEvent.click(sendBtn);

    // Even while the post-send refresh is still pending, Sending... clears immediately,
    // the textarea resets to empty, and the optimistic turn renders right away
    await waitFor(() => {
      expect(textarea.value).toBe("");
      expect(sendBtn).toHaveTextContent("Send Message");
    });
    expect(screen.getAllByText("Fast operator prompt").length).toBeGreaterThan(0);
    expect(resolvePendingSessionsRefresh).not.toBeNull();

    // Clean up pending promises
    resolvePendingSessionsRefresh?.(
      new Response(JSON.stringify({ sessions: [] }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    resolvePendingBgTranscript?.(
      new Response(JSON.stringify({ session_id: "sess-jarvis", turns: [] }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );

    unmount();
  });

  it("refreshes the implicitly active session rather than its persona after a different session event", async () => {
    let sessionListener: ((frame: unknown) => void) | null = null;
    const transcriptTargets: string[] = [];
    const fetchImpl = vi.fn((input: RequestInfo | URL) => {
      const url = String(input);
      const json = (body: unknown) => Promise.resolve(new Response(JSON.stringify(body), { status: 200, headers: { "Content-Type": "application/json" } }));
      if (url.includes("/api/v1/auth/session")) return json({ authenticated: true, authority: { tier: "owner", login: "lmcfarlin" } });
      if (url.includes("/api/v1/sessions") && !url.includes("/transcript")) return json({ sessions: [
        { id: "session-b", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" },
        { id: "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" },
      ] });
      if (url.includes("/transcript")) {
        transcriptTargets.push(url);
        const isB = url.includes("session-b");
        return json({ session_id: isB ? "session-b" : "session-a", turns: [turn({ index: 0, content: isB ? "Correct session B" : "Wrong session A" })], queue: [] });
      }
      return json({});
    });
    const wsStub = { setToken: vi.fn(), connect: vi.fn(), disconnect: vi.fn(), onStatusChange: () => () => {}, subscribe: (topic: string, cb: (frame: unknown) => void) => { if (topic === "session") sessionListener = cb; return () => {}; } } as unknown as HubWsClient;
    const apiClient = new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    const { unmount } = render(<App apiClient={apiClient} wsClient={wsStub} />);
    await waitFor(() => expect(transcriptTargets.some((url) => url.includes("session-b"))).toBe(true));
    await screen.findByText("Correct session B");
    const before = transcriptTargets.length;
    act(() => sessionListener?.({ type: "event", topic: "session", event: "prompt_sent", data: { session_id: "session-a", persona: "ferris", effective_body: "A message" } }));
    await waitFor(() => expect(transcriptTargets.length).toBeGreaterThan(before));
    await waitFor(() => expect(screen.getByText("Correct session B")).toBeInTheDocument());
    expect(screen.queryByText("Wrong session A")).toBeNull();
    expect(transcriptTargets.slice(before).some((url) => url.includes("/ferris/transcript"))).toBe(false);
    unmount();
  });

  it("does not apply an implicit session's late send response to its successor", async () => {
    let sessionListener: ((frame: unknown) => void) | null = null;
    let firstId = "session-a";
    let finishSend!: (response: Response) => void;
    const sendResponse = new Promise<Response>((resolve) => { finishSend = resolve; });
    const fetchImpl = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const json = (body: unknown) => Promise.resolve(new Response(JSON.stringify(body), { status: 200, headers: { "Content-Type": "application/json" } }));
      if (url.includes("/api/v1/auth/session")) return json({ authenticated: true, authority: { tier: "owner", login: "lmcfarlin" } });
      if (url.includes("/prompt") && init?.method === "POST") return sendResponse;
      if (url.includes("/api/v1/sessions") && !url.includes("/transcript")) return json({ sessions: [
        { id: firstId, source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" },
        { id: firstId === "session-a" ? "session-b" : "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" },
      ] });
      if (url.includes("/transcript")) return json({ session_id: firstId, turns: [turn({ index: 0, content: firstId === "session-a" ? "Session A ready" : "Session B ready" })], queue: [], is_busy: false });
      return json({});
    });
    const wsStub = { setToken: vi.fn(), connect: vi.fn(), disconnect: vi.fn(), onStatusChange: () => () => {}, subscribe: (topic: string, cb: (frame: unknown) => void) => { if (topic === "session") sessionListener = cb; return () => {}; } } as unknown as HubWsClient;
    const apiClient = new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    const { unmount } = render(<App apiClient={apiClient} wsClient={wsStub} />);
    await screen.findByText("Session A ready");
    fireEvent.change(screen.getByPlaceholderText(/Send a message/i), { target: { value: "A only" } });
    fireEvent.click(screen.getByRole("button", { name: /^Send Message$/i }));
    await waitFor(() => expect(fetchImpl.mock.calls.some(([url, init]) => String(url).includes("/prompt") && init?.method === "POST")).toBe(true));
    firstId = "session-b";
    act(() => sessionListener?.({ type: "event", topic: "session", event: "updated", data: {} }));
    await waitFor(() => expect((screen.getByTestId("session-selector") as HTMLSelectElement).value).toBe("session-b"));
    await act(async () => { finishSend(new Response(JSON.stringify({ status: "sent", delivered: true, effective_body: "A only", queue: [{ id: "a-queue", prompt: "A queue" }], is_busy: true }), { status: 200, headers: { "Content-Type": "application/json" } })); await sendResponse; });
    expect(screen.queryByText("A only")).toBeNull();
    expect(screen.queryByText("A queue")).toBeNull();
    unmount();
  });

  it("does not apply a late initial transcript after the implicit session changes", async () => {
    let sessionListener: ((frame: unknown) => void) | null = null;
    let current = "session-a";
    let releaseInitial!: (response: Response) => void;
    const initialResponse = new Promise<Response>((resolve) => { releaseInitial = resolve; });
    let held = false;
    const response = (body: unknown) => new Response(JSON.stringify(body), { status: 200, headers: { "Content-Type": "application/json" } });
    const fetchImpl = vi.fn((input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/api/v1/auth/session")) return Promise.resolve(response({ authenticated: true, authority: { tier: "owner", login: "lmcfarlin" } }));
      if (url.includes("/api/v1/sessions") && !url.includes("/transcript")) return Promise.resolve(response({ sessions: [
        { id: current, source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" },
        { id: current === "session-a" ? "session-b" : "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" },
      ] }));
      if (url.includes("/session-a/transcript") && !held) { held = true; return initialResponse; }
      if (url.includes("/transcript")) {
        const isB = url.includes("session-b");
        return Promise.resolve(response({ session_id: isB ? "session-b" : "session-a", turns: [turn({ index: 0, content: isB ? "Session B initial" : "Session A interim" })], queue: [], is_busy: false }));
      }
      return Promise.resolve(response({}));
    });
    const wsStub = { setToken: vi.fn(), connect: vi.fn(), disconnect: vi.fn(), onStatusChange: () => () => {}, subscribe: (topic: string, cb: (frame: unknown) => void) => { if (topic === "session") sessionListener = cb; return () => {}; } } as unknown as HubWsClient;
    const apiClient = new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    const { unmount } = render(<App apiClient={apiClient} wsClient={wsStub} />);
    await waitFor(() => expect(held).toBe(true));
    current = "session-b";
    act(() => sessionListener?.({ type: "event", topic: "session", event: "updated", data: {} }));
    await screen.findByText("Session B initial");
    await act(async () => {
      releaseInitial(response({ session_id: "session-a", turns: [turn({ index: 0, content: "Stale initial A" })], queue: [{ id: "old", prompt: "Stale A queue" }], is_busy: true }));
      await initialResponse;
    });
    expect(screen.getByText("Session B initial")).toBeInTheDocument();
    expect(screen.queryByText("Stale initial A")).toBeNull();
    expect(screen.queryByText("Stale A queue")).toBeNull();
    unmount();
  });

  it("applies the initial transcript when its implicit session remains active", async () => {
    let releaseInitial!: (response: Response) => void;
    const initialResponse = new Promise<Response>((resolve) => { releaseInitial = resolve; });
    let held = false;
    const response = (body: unknown) => new Response(JSON.stringify(body), { status: 200, headers: { "Content-Type": "application/json" } });
    const fetchImpl = vi.fn((input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/api/v1/auth/session")) return Promise.resolve(response({ authenticated: true, authority: { tier: "owner", login: "lmcfarlin" } }));
      if (url.includes("/api/v1/sessions") && !url.includes("/transcript")) return Promise.resolve(response({ sessions: [
        { id: "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" },
      ] }));
      if (url.includes("/session-a/transcript") && !held) { held = true; return initialResponse; }
      if (url.includes("/transcript")) return Promise.resolve(response({ session_id: "session-a", turns: [], queue: [], is_busy: false }));
      return Promise.resolve(response({}));
    });
    const wsStub = { setToken: vi.fn(), connect: vi.fn(), disconnect: vi.fn(), onStatusChange: () => () => {}, subscribe: () => () => {} } as unknown as HubWsClient;
    const apiClient = new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    const { unmount } = render(<App apiClient={apiClient} wsClient={wsStub} />);
    await waitFor(() => expect(held).toBe(true));
    await act(async () => {
      releaseInitial(response({ session_id: "session-a", turns: [turn({ index: 0, content: "Legitimate initial A" })], queue: [], is_busy: false }));
      await initialResponse;
    });
    expect(screen.getByText("Legitimate initial A")).toBeInTheDocument();
    unmount();
  });

  it("preserves multiple outbound prompts across polls and keeps prompt_sent WebSocket / queued prompts visible while in FerroWire transit", async () => {
    const now = Date.now();
    const baseTurns: TranscriptTurn[] = [
      turn({ index: 0, role: "assistant", content: "Initial greeting" }),
    ];
    const multiOptimistic = [
      { content: "First outbound prompt", sentAt: now - 2000, agentKey: "ferris" },
      { content: "Second outbound prompt", sentAt: now - 1000, agentKey: "ferris" },
    ];

    const preservedBoth = preserveOptimisticTurn(
      baseTurns,
      multiOptimistic,
      now,
      "ferris",
    );
    expect(preservedBoth.map((t) => t.content)).toEqual([
      "Initial greeting",
      "First outbound prompt",
      "Second outbound prompt",
    ]);

    // When the first prompt flushes to disk, only the second remains appended optimistically
    const afterFirstFlush = preserveOptimisticTurn(
      [
        ...baseTurns,
        turn({ index: 1, role: "user", content: "First outbound prompt" }),
      ],
      multiOptimistic,
      now,
      "ferris",
    );
    expect(afterFirstFlush.map((t) => t.content)).toEqual([
      "Initial greeting",
      "First outbound prompt",
      "Second outbound prompt",
    ]);

    // Verify prompt_sent WebSocket event and handleSendQueuedNow keep outbound prompts visible across transcript refreshes
    let sessionListener: ((frame: unknown) => void) | null = null;
    const fetchImpl = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
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
      if (url.includes("/queue/q-1/send") && init?.method === "POST") {
        return json({
          status: "delivered",
          prompt: "Force-sent queued prompt",
          effective_body: "Force-sent queued prompt",
          queue: [],
          is_busy: true,
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
          ],
        });
      }
      if (url.includes("/transcript")) {
        // Disk JSONL has NOT yet flushed the outbound prompts
        return json({
          session_id: "sess-ferris",
          turns: [
            turn({
              index: 0,
              role: "assistant",
              content: "Waiting for operator instructions.",
            }),
          ],
          is_busy: false,
          queue: [
            {
              id: "q-1",
              session_id: "sess-ferris",
              prompt: "Force-sent queued prompt",
              created_at: new Date().toISOString(),
            },
          ],
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
      subscribe: (topic: string, cb: (frame: unknown) => void) => {
        if (topic === "session") {
          sessionListener = cb;
        }
        return () => {};
      },
    } as unknown as HubWsClient;

    const { unmount } = render(<App apiClient={apiClient} wsClient={wsStub} />);

    expect(
      await screen.findByText(/Waiting for operator instructions\./),
    ).toBeInTheDocument();

    // Another session of the same persona must not affect the active session.
    act(() => {
      sessionListener?.({ type: "event", topic: "session", event: "prompt_sent", data: {
        session_id: "sess-ferris-other", persona: "ferris", effective_body: "Other session secret", is_busy: true,
        queue: [{ id: "other-session-queue", session_id: "sess-ferris-other", prompt: "Wrong queue", created_at: new Date().toISOString() }],
      }});
    });
    expect(screen.queryByText("Other session secret")).toBeNull();
    expect(screen.queryByText("Wrong queue")).toBeNull();

    // A failed push is broadcast too; it must not become a sent transcript turn.
    act(() => {
      sessionListener?.({
        type: "event",
        topic: "session",
        event: "prompt_sent",
        data: {
          session_id: "sess-ferris",
          persona: "ferris",
          effective_body: "Failed delivery must not appear as sent",
          delivered: false,
        },
      });
    });
    expect(screen.queryByText("Failed delivery must not appear as sent")).toBeNull();

    // Emit a prompt_sent WebSocket event (e.g. auto-drained queued prompt)
    act(() => {
      sessionListener?.({
        type: "event",
        topic: "session",
        event: "prompt_sent",
        data: {
          session_id: "sess-ferris",
          persona: "ferris",
          effective_body: "Auto-drained queue prompt via WS",
        },
      });
    });

    expect(
      (await screen.findAllByText("Auto-drained queue prompt via WS")).length,
    ).toBeGreaterThan(0);

    // Force-send a queued prompt via Send Now button — both prompts stay visible even after refreshSessionsAndTranscript runs against the un-flushed disk transcript
    const sendNowBtn = await screen.findByTestId("queued-send-now-q-1");
    fireEvent.click(sendNowBtn);

    await waitFor(() => {
      expect(
        screen.getAllByText("Force-sent queued prompt").length,
      ).toBeGreaterThan(1);
      expect(
        screen.getAllByText("Auto-drained queue prompt via WS").length,
      ).toBeGreaterThan(0);
    });

    unmount();
  });

  it("sets flexShrink: 0 on TurnCard and EncapsulatedWorkBlock root articles so chat entries never get squished or cut off", () => {
    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        agentRole="lead"
        agentProject="FerroLoop"
        adapter="claude"
        model="claude-opus-4-6"
        sessionId="sess-ferris"
        turns={[
          turn({
            index: 0,
            role: "user",
            content: "Run a quick check.",
          }),
          turn({
            index: 1,
            role: "assistant",
            content: "",
            thinking: "Checking workspace status...",
            tool_calls: [{ name: "run_command", arguments: '{"cmd":"git status"}' }],
          }),
          turn({
            index: 2,
            role: "assistant",
            content: "Workspace is clean.",
          }),
        ]}
        isBusy={false}
        queue={[]}
        loading={false}
        wsConnected={true}
        onRefresh={() => {}}
        onSendPrompt={async () => ({ status: "ok" })}
      />,
    );

    const userCard = screen.getByTestId("turn-card-0");
    const workBlock = screen.getByTestId("encapsulated-work-block");
    const assistantCard = screen.getByTestId("turn-card-2");

    expect(userCard.style.flexShrink).toBe("0");
    expect(workBlock.style.flexShrink).toBe("0");
    expect(assistantCard.style.flexShrink).toBe("0");
  });
});
