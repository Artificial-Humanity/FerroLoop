import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import {
  activeSessionStillCurrent,
  App,
  computeUnreadByAgent,
  countUnseenAssistantTurns,
  isUserDirectedAssistantTurn,
  LAST_SEEN_ASSISTANT_TURN_STORAGE_KEY,
  OPTIMISTIC_PROMPT_BUSY_MAX_MS,
  OPTIMISTIC_PROMPT_TTL_MS,
  pruneOptimisticPrompts,
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
  it("returns no usage and no maximum for an empty transcript with no server usage", () => {
    expect(estimateSessionContextUsage([])).toEqual({
      usedTokens: null,
      maxTokens: null,
      percent: null,
      source: null,
      offeredWindows: [],
      estimated: false,
    });
  });

  it("estimates overhead plus content, thinking, and tool calls as used tokens only", () => {
    const usage = estimateSessionContextUsage([turn({ index: 0, content: "x".repeat(400) })]);
    // 12_000 overhead + ceil((400 + 0 + 2) / 4) for content + "[]" tool calls.
    expect(usage.usedTokens).toBe(12_101);
    expect(usage.estimated).toBe(true);
    // The estimate never invents a window: no maximum and no percent.
    expect(usage.maxTokens).toBeNull();
    expect(usage.percent).toBeNull();
    expect(usage.source).toBeNull();
  });

  it("resets the estimate at the last compaction boundary", () => {
    const usage = estimateSessionContextUsage([
      turn({ index: 0, content: "x".repeat(360_000) }),
      turn({ index: 1, content: "[Compacted context] Earlier work summary." }),
      turn({ index: 2, content: "Next step." }),
    ]);
    // Only the boundary turn and the short follow-up count toward usage.
    expect(usage.usedTokens).toBeLessThan(13_000);
  });

  it("takes the maximum, percent, source and offered sizes only from the server usage", () => {
    expect(
      estimateSessionContextUsage([turn({ index: 0, content: "hi" })], {
        used_tokens: 150_000,
        max_tokens: 200_000,
      }),
    ).toEqual({
      usedTokens: 150_000,
      maxTokens: 200_000,
      percent: 75,
      source: null,
      offeredWindows: [],
      estimated: false,
    });
    expect(
      estimateSessionContextUsage([], {
        used_tokens: 5,
        max_tokens: 256_000,
        source: "catalog",
        offered_windows: [256_000, 500_000],
      }),
    ).toMatchObject({ source: "catalog", offeredWindows: [256_000, 500_000] });
    // A percent above 100 is capped.
    expect(
      estimateSessionContextUsage([], { used_tokens: 300_000, max_tokens: 200_000 }).percent,
    ).toBe(100);
    // A server maximum of zero names no window.
    expect(
      estimateSessionContextUsage([], { used_tokens: 7, max_tokens: 0, source: "assumed" }),
    ).toMatchObject({ usedTokens: 7, maxTokens: null, percent: null, source: null });
  });

  it("formats token counts", () => {
    expect(formatTokenCount(1_000_000)).toBe("1m");
    expect(formatTokenCount(1_500_000)).toBe("1.5m");
    expect(formatTokenCount(500_000)).toBe("500k");
    expect(formatTokenCount(200_000)).toBe("200k");
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
        contextUsage={{ used_tokens: 40_000, max_tokens: 200_000, source: "reported" }}
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
        turns={[turn({ index: 0, role: "user", content: "Hello" })]}
        contextUsage={{ used_tokens: 102_000, max_tokens: 200_000, source: "reported" }}
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
        turns={[turn({ index: 0, role: "user", content: "Hello" })]}
        contextUsage={{ used_tokens: 102_000, max_tokens: 200_000, source: "reported" }}
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

  it("drops the optimistic turn once it reaches the disk tail or ages out past 5 minutes", () => {
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
    ).toHaveLength(2);
    expect(
      preserveOptimisticTurn(diskTurns, { content: "Ship it", sentAt: 1_000 }, 302_000),
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

  it("carries a prompt recorded before the session id resolved to the resolved id", () => {
    const resolved = "72e7cfb2-13cc-4e34-8b49-ee789472fdc6";
    for (const sessionId of [undefined, "claude-felix-2027881"]) {
      const prompt = { content: "Hello", sentAt: 1_000, agentKey: "felix", sessionId };
      expect(preserveOptimisticTurn(diskTurns, prompt, 3_000, "felix", resolved)).toHaveLength(2);
      // Another agent, or a canonical id of another session, still does not inherit it.
      expect(preserveOptimisticTurn(diskTurns, prompt, 3_000, "ferris", resolved)).toBe(diskTurns);
      const kept = pruneOptimisticPrompts([prompt], diskTurns, 3_000, "felix", resolved);
      expect(kept).toHaveLength(1);
      expect(kept[0].sessionId).toBe(resolved);
    }
    const canonical = { content: "Hello", sentAt: 1_000, agentKey: "felix", sessionId: resolved };
    expect(
      preserveOptimisticTurn(diskTurns, canonical, 3_000, "felix", "11111111-2222-4333-8444-555555555555"),
    ).toBe(diskTurns);
  });

  it("does not restamp another agent's unresolved prompt to the active agent's session id", () => {
    const resolved = "72e7cfb2-13cc-4e34-8b49-ee789472fdc6";
    const other = { content: "for ferris", sentAt: 1_000, agentKey: "ferris", sessionId: undefined };
    const kept = pruneOptimisticPrompts([other], diskTurns, 3_000, "felix", resolved);
    expect(kept).toHaveLength(1);
    expect(kept[0].sessionId).toBeUndefined();
    const own = { content: "for felix", sentAt: 1_000, agentKey: "felix", sessionId: undefined };
    expect(pruneOptimisticPrompts([own], diskTurns, 3_000, "felix", resolved)[0].sessionId).toBe(resolved);
  });

  it("keeps recording a send whose active session id resolved mid-flight, not one that switched", () => {
    const resolved = "72e7cfb2-13cc-4e34-8b49-ee789472fdc6";
    expect(activeSessionStillCurrent(null, resolved, "felix")).toBe(true);
    expect(activeSessionStillCurrent("claude-felix-2027881", resolved, resolved)).toBe(true);
    expect(activeSessionStillCurrent(resolved, resolved, resolved)).toBe(true);
    expect(activeSessionStillCurrent(resolved, "11111111-2222-4333-8444-555555555555", resolved)).toBe(false);
    expect(activeSessionStillCurrent(resolved, null, resolved)).toBe(false);
    expect(activeSessionStillCurrent(resolved, resolved, "other-target")).toBe(false);
  });

  it("keeps an expired optimistic prompt while the session is busy, up to a hard cap", () => {
    const prompt = { content: "Still working", sentAt: 1_000, agentKey: "felix" };
    const late = 1_000 + OPTIMISTIC_PROMPT_TTL_MS + 1;
    expect(preserveOptimisticTurn(diskTurns, prompt, late, "felix")).toBe(diskTurns);
    expect(preserveOptimisticTurn(diskTurns, prompt, late, "felix", undefined, true)).toHaveLength(2);
    expect(pruneOptimisticPrompts([prompt], diskTurns, late, "felix", undefined, true)).toHaveLength(1);
    expect(pruneOptimisticPrompts([prompt], diskTurns, late, "felix", undefined, false)).toHaveLength(0);
    const beyondCap = 1_000 + OPTIMISTIC_PROMPT_BUSY_MAX_MS + 1;
    expect(preserveOptimisticTurn(diskTurns, prompt, beyondCap, "felix", undefined, true)).toBe(diskTurns);
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

  it("rejects an older initial transcript after a newer observed empty read", async () => {
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
    expect(screen.queryByText("Legitimate initial A")).toBeNull();
    unmount();
  });

  it("applies a refused queued response through the App callback before surfacing its error", async () => {
    const item = { id: "refused-app-q", session_id: "refused-app-session", prompt: "held App prompt", by: "owner", created_at: "now" };
    let refused = false;
    const heldItem = { ...item, delivery_state: "refused", delivery_error: "App receiver refused" };
    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200 });
      if (url.includes("/auth/session")) return json({ authenticated: true, authority: { tier: "owner", login: "lmcfarlin" } });
      if (url.includes("/queue/refused-app-q/send") && init?.method === "POST") {
        refused = true;
        return json({ delivered: false, delivery_stage: "refused", delivery_error: "App receiver refused", effective_body: item.prompt,
          queue: [heldItem], is_busy: false });
      }
      if (url.includes("/sessions") && !url.includes("/transcript")) return json({ sessions: [{
        id: item.session_id, source: "route", adapter: "codex", persona: "ferris", status: "active", updated_at: "t1",
      }] });
      if (url.includes("/transcript")) return json({ session_id: item.session_id, turns: [], queue: [refused ? heldItem : item], is_busy: !refused });
      return json({});
    });
    const apiClient = new HubApiClient({ fetchImpl: fetchImpl as typeof fetch });
    const wsClient = { setToken: vi.fn(), connect: vi.fn(), disconnect: vi.fn(), onStatusChange: () => () => {}, subscribe: () => () => {} } as unknown as HubWsClient;
    const view = render(<App apiClient={apiClient} wsClient={wsClient} />);
    fireEvent.click(await screen.findByTestId("queued-send-now-refused-app-q"));
    expect(await screen.findByText(/Send now refused/)).toBeInTheDocument();
    expect(screen.getByTestId("queued-send-now-refused-app-q")).toBeDisabled();
    expect(screen.getByTestId("queued-edit-refused-app-q")).toBeDisabled();
    expect(screen.getByTestId("queued-cancel-refused-app-q")).toBeEnabled();
    expect(screen.getByRole("alert")).toHaveTextContent("App receiver refused");
    expect(screen.getByText("Idle")).toBeInTheDocument();
    expect(screen.queryByTestId("turn-card-0")).toBeNull();
    view.unmount();
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

describe("App transcript observation ownership", () => {
  const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
  const observed = (content: string) => response({ session_id: "session-a", turns: content ? [turn({ index: 0, content })] : [], queue: [], is_busy: false });
  function harness(initialRead?: (target: string) => Promise<Response>) {
    let sessions = [{ id: "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" }];
    let read: (target: string) => Promise<Response> = initialRead ?? (async () => observed("Last good history"));
    let listener: ((frame: unknown) => void) | undefined;
    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/auth/session")) return response({ authenticated: true, authority: { tier: "owner", login: "owner" } });
      if (url.includes("/transcript")) return read(url);
      if (url.endsWith("/sessions")) return response({ sessions });
      return response({});
    });
    const api = new HubApiClient({ fetchImpl: fetchImpl as typeof fetch });
    const ws = { setToken: vi.fn(), connect: vi.fn(), disconnect: vi.fn(), onStatusChange: () => () => {}, subscribe: (topic: string, cb: (frame: unknown) => void) => { if (topic === "session") listener = cb; return () => {}; } } as unknown as HubWsClient;
    const view = render(<App apiClient={api} wsClient={ws} />);
    return { ...view, api, ws, fetchImpl, setRead: (next: typeof read) => { read = next; }, setSessions: (next: typeof sessions) => { sessions = next; }, refresh: (data?: Record<string, unknown>) => act(() => listener?.({ type: "event", topic: "session", event: data ? "prompt_sent" : "updated", data: data ?? {} })) };
  }

  it.each(["503", "network", "malformed"])("keeps same-session history and metadata on %s then accepts recovery and observed empty", async (failure) => {
    const h = harness();
    await screen.findByText("Last good history");
    h.setRead(async () => {
      if (failure === "network") throw new TypeError("offline");
      return failure === "503" ? response({ code: "transcript_unavailable" }, 503) : response({ session_id: "session-a" });
    });
    h.refresh();
    await screen.findByText("History refresh unavailable; showing last loaded history");
    expect(screen.getByText("Last good history")).toBeInTheDocument();
    h.setRead(async () => observed("Recovered history")); h.refresh();
    await screen.findByText("Recovered history");
    expect(screen.queryByText("History refresh unavailable; showing last loaded history")).toBeNull();
    h.setRead(async () => observed("")); h.refresh();
    await waitFor(() => expect(screen.queryByText("Recovered history")).toBeNull());
    h.unmount();
  });

  it.each([["implicit", "503"], ["implicit", "network"], ["explicit", "503"], ["explicit-non-first", "503"]])("retains %s same-session history and metadata through actual triage navigation and %s", async (preference, failure) => {
    const richHistory = () => response({ session_id: "session-a", turns: [turn({ index: 0, content: "Same-target triage history" })], is_busy: true,
      queue: [{ id: "same-target-held", session_id: "session-a", prompt: "Same-target held prompt", by: "owner", created_at: "now", delivery_state: "unconfirmed" }],
      context_usage: { used_tokens: 50000, max_tokens: 200000 }, quota_usage: { exhausted_reason: "Same-target quota" } });
    const h = harness(async (target) => target.includes("session-b")
      ? response({ session_id: "session-b", turns: [turn({ index: 0, content: "Alternate concrete baseline" })], queue: [], is_busy: false })
      : richHistory());
    const sameTarget = { id: "session-a", source: "route", adapter: "claude", persona: "ferris", status: "failed", updated_at: "t1" };
    const alternate = { id: "session-b", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" };
    h.setSessions(preference === "explicit-non-first" ? [alternate, sameTarget] : [sameTarget, alternate]);
    if (preference === "explicit-non-first") {
      await screen.findByText("Alternate concrete baseline");
      fireEvent.change(screen.getByTestId("session-selector"), { target: { value: "session-a" } });
      await screen.findByText("Same-target triage history");
    } else {
      await screen.findByText("Same-target triage history");
    }
    if (preference === "explicit") {
      // Establish an explicit preference using genuine different-value changes.
      fireEvent.change(screen.getByTestId("session-selector"), { target: { value: "session-b" } });
      await screen.findByText("Alternate concrete baseline");
      fireEvent.change(screen.getByTestId("session-selector"), { target: { value: "session-a" } });
      await screen.findByText("Same-target triage history");
    }
    await screen.findByText("Same-target held prompt");
    const assertBaseline = () => {
      expect(screen.getByText("Same-target triage history")).toBeInTheDocument();
      expect(screen.getByText("Same-target held prompt")).toBeInTheDocument();
      expect(screen.getByTestId("queued-send-now-same-target-held")).toBeDisabled();
      expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Busy");
      expect(screen.getByTestId("quota-exhausted-toolbar-pill")).toBeInTheDocument();
      fireEvent.click(screen.getByTestId("quota-exhausted-toolbar-pill"));
      expect(screen.getByTestId("context-level-text")).toHaveTextContent("50k");
      expect(screen.getByTestId("context-max")).toHaveTextContent("200k");
      fireEvent.click(screen.getByTestId("close-agent-settings-btn"));
    };
    assertBaseline();
    const send = vi.spyOn(h.api, "sendSessionPrompt");
    const sendQueued = vi.spyOn(h.api, "sendQueuedPromptNow");
    const reset = vi.spyOn(h.api, "clearSession");
    fireEvent.click(screen.getByTestId("left-panel-tab-triage"));
    const incident = await screen.findByTestId("triage-incident-attempt-session-session-a");
    let release!: (result: Response) => void;
    let reject!: (error: Error) => void;
    const pending = new Promise<Response>((resolve, rejectRead) => { release = resolve; reject = rejectRead; });
    let started = false;
    h.setRead(async () => { started = true; return pending; });
    // The real card invokes onSelectAgent followed by onSelectSession for the
    // displayed session, including when its session preference was implicit.
    fireEvent.click(incident);
    await waitFor(() => expect(started).toBe(true));
    assertBaseline();
    await act(async () => {
      if (failure === "network") reject(new TypeError("offline"));
      else release(response({ code: "transcript_unavailable" }, 503));
      await pending.catch(() => {});
    });
    await screen.findByText("History refresh unavailable; showing last loaded history");
    assertBaseline();
    h.setRead(async () => observed("Stable same-target recovery")); h.refresh();
    await screen.findByText("Stable same-target recovery");
    expect(screen.queryByText("History refresh unavailable; showing last loaded history")).toBeNull();
    h.setRead(async () => observed("")); h.refresh();
    await waitFor(() => expect(screen.queryByText("Stable same-target recovery")).toBeNull());
    expect(screen.queryByText("Same-target triage history")).toBeNull();
    expect(screen.queryByText("Same-target held prompt")).toBeNull();
    expect(send).not.toHaveBeenCalled();
    expect(sendQueued).not.toHaveBeenCalled();
    expect(reset).not.toHaveBeenCalled();
    h.unmount();
  });

  it("masks a different-session triage target and rejects the old selected response", async () => {
    const h = harness(async () => response({ session_id: "session-a", turns: [turn({ index: 0, content: "Before different-target navigation" })], is_busy: true,
      queue: [{ id: "previous-target", session_id: "session-a", prompt: "Previous target queue", by: "owner", created_at: "now" }],
      context_usage: { used_tokens: 50000, max_tokens: 200000 }, quota_usage: { exhausted_reason: "Previous target quota" } }));
    h.setSessions([{ id: "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" },
      { id: "session-b", source: "route", adapter: "claude", persona: "ferris", status: "failed", updated_at: "t1" }]);
    await screen.findByText("Before different-target navigation");
    await screen.findByText("Previous target queue");
    let release!: (result: Response) => void;
    const pending = new Promise<Response>((resolve) => { release = resolve; });
    let started = false;
    h.setRead(async () => { started = true; return pending; }); h.refresh();
    await waitFor(() => expect(started).toBe(true));
    fireEvent.click(screen.getByTestId("left-panel-tab-triage"));
    h.setRead(async () => response({ code: "transcript_unavailable" }, 503));
    fireEvent.click(await screen.findByTestId("triage-incident-attempt-session-session-b"));
    expect(screen.queryByText("Before different-target navigation")).toBeNull();
    expect(screen.queryByText("Previous target queue")).toBeNull();
    expect(screen.queryByTestId("quota-exhausted-toolbar-pill")).toBeNull();
    expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Idle");
    await act(async () => { release(observed("Late previous-session response")); await pending; });
    await screen.findByText("History unavailable");
    expect((screen.getByTestId("session-selector") as HTMLSelectElement).value).toBe("session-b");
    expect(screen.queryByText("Late previous-session response")).toBeNull();
    expect(screen.queryByText("Before different-target navigation")).toBeNull();
    expect(screen.queryByText("Previous target queue")).toBeNull();
    h.unmount();
  });

  it("does not install persona badge history into a different concrete session", async () => {
    const h = harness(); await screen.findByText("Last good history");
    h.setSessions([{ id: "session-b", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t2" }, { id: "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" }]);
    h.setRead(async () => response({ code: "transcript_unavailable" }, 503)); h.refresh();
    await waitFor(() => expect((screen.getByTestId("session-selector") as HTMLSelectElement).value).toBe("session-b"));
    expect(screen.queryByText("Last good history")).toBeNull();
    await screen.findByText("History unavailable");
    h.unmount();
  });

  it.each([401, 403])("clears history on denied transcript %s", async (status) => {
    const h = harness(); await screen.findByText("Last good history");
    h.setRead(async () => response({ error: "Denied" }, status)); h.refresh();
    await waitFor(() => expect(screen.queryByText("Last good history")).toBeNull());
    expect(screen.queryByText("History refresh unavailable; showing last loaded history")).toBeNull();
    h.unmount();
  });
  it.each(["success", "failure"])("rejects old initial success and its queue/context after newer %s", async (latest) => {
    let release!: (result: Response) => void;
    const pending = new Promise<Response>((resolve) => { release = resolve; });
    let concreteReads = 0;
    const h = harness(async (target) => {
      if (!target.includes("session-a")) return observed("Alias history");
      if (++concreteReads === 1) return pending;
      return latest === "success" ? observed("Newest history") : response({ code: "transcript_unavailable" }, 503);
    });
    await screen.findByText(latest === "success" ? "Newest history" : "History unavailable");
    await act(async () => { release(response({ session_id: "session-a", turns: [turn({ index: 7, content: "Old response" })], is_busy: true, queue: [{ id: "old", prompt: "Old queued prompt" }], context_usage: { used_tokens: 199999, max_tokens: 200000 } })); await pending; });
    expect(screen.queryByText("Old response")).toBeNull();
    expect(screen.queryByText("Old queued prompt")).toBeNull();
    expect(screen.getByText(latest === "success" ? "Newest history" : "History unavailable")).toBeInTheDocument();
    h.unmount();
  });

  it("rejects A to B to A old reads", async () => {
    const h = harness(); await screen.findByText("Last good history");
    let release!: (result: Response) => void;
    const pending = new Promise<Response>((resolve) => { release = resolve; });
    h.setRead(async () => pending); h.refresh();
    await waitFor(() => expect(h.fetchImpl.mock.calls.filter(([input]) => String(input).includes("/transcript")).length).toBeGreaterThan(3));
    fireEvent.click(screen.getByTestId("agent-row-jarvis"));
    expect(screen.queryByText("Last good history")).toBeNull();
    h.setRead(async () => observed("Current A"));
    fireEvent.click(screen.getByTestId("agent-row-ferris"));
    await screen.findByText("Current A");
    await act(async () => { release(observed("Old A")); await pending; });
    expect(screen.queryByText("Old A")).toBeNull();
    expect(screen.getByText("Current A")).toBeInTheDocument();
    h.unmount();
  });

  it.each(["clear", "reforge"] as const)("delegates %s once and rejects a pre-reset pending transcript", async (mode) => {
    const h = harness(); await screen.findByText("Last good history");
    let release!: (result: Response) => void;
    const pending = new Promise<Response>((resolve) => { release = resolve; });
    let started = false;
    h.setRead(async () => { started = true; return pending; }); h.refresh();
    await waitFor(() => expect(started).toBe(true));
    const clear = vi.spyOn(h.api, "clearSession").mockImplementation(async () => {
      h.setRead(async () => observed("")); return { ok: true } as never;
    });
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
    fireEvent.click(screen.getByTestId(`clear-modal-${mode}-btn`));
    await waitFor(() => expect(clear).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(screen.queryByText("Last good history")).toBeNull());
    await act(async () => { release(observed("Pre-reset history")); await pending; });
    expect(screen.queryByText("Pre-reset history")).toBeNull();
    expect(clear).toHaveBeenCalledWith("session-a", mode);
    h.unmount();
  });

  it.each(["clear", "reforge", "teardown"] as const)("failed %s retains last good history and permits a later refresh", async (mode) => {
    const h = harness(); await screen.findByText("Last good history");
    if (mode === "teardown") vi.spyOn(h.api, "decommissionSession").mockRejectedValue(new Error("Reset refused"));
    else vi.spyOn(h.api, "clearSession").mockRejectedValue(new Error("Reset refused"));
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    if (mode === "teardown") {
      fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
      fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
      fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));
    } else {
      fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
      fireEvent.click(screen.getByTestId(`clear-modal-${mode}-btn`));
    }
    await screen.findByText("Reset refused");
    expect(screen.getByText("Last good history")).toBeInTheDocument();
    h.setRead(async () => observed("After failed reset")); h.refresh();
    await screen.findByText("After failed reset");
    h.unmount();
  });

  it("successful teardown rejects its pending transcript and cache", async () => {
    const h = harness(); await screen.findByText("Last good history");
    let release!: (result: Response) => void;
    const pending = new Promise<Response>((resolve) => { release = resolve; });
    let started = false;
    h.setRead(async () => { started = true; return pending; }); h.refresh();
    await waitFor(() => expect(started).toBe(true));
    vi.spyOn(h.api, "decommissionSession").mockResolvedValue({ ok: true } as never);
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));
    await waitFor(() => expect(screen.queryByText("Last good history")).toBeNull());
    await act(async () => { release(observed("Old teardown history")); await pending; });
    expect(screen.queryByText("Old teardown history")).toBeNull();
    h.unmount();
  });

  it("does not retain an unresolved alias's previous successful observation", async () => {
    const h = harness(); await screen.findByText("Last good history");
    h.setSessions([]);
    h.setRead(async () => response({ code: "transcript_unavailable" }, 503));
    fireEvent.click(screen.getByTestId("agent-row-jarvis"));
    await screen.findByText("History unavailable");
    expect(screen.queryByText("Last good history")).toBeNull();
    expect(screen.queryByTestId("chat-empty-state")).toBeNull();
    h.unmount();
  });

  it("rejects old principal reads after logout and relogin at the same authority tier", async () => {
    const h = harness(); await screen.findByText("Last good history");
    let release!: (result: Response) => void;
    const pending = new Promise<Response>((resolve) => { release = resolve; });
    let started = false;
    h.setRead(async () => { started = true; return pending; }); h.refresh();
    await waitFor(() => expect(started).toBe(true));
    vi.spyOn(h.api, "logout").mockResolvedValue({} as never);
    vi.spyOn(h.api, "pair").mockResolvedValue({ authenticated: true, authority: { tier: "owner", login: "other-owner" } });
    fireEvent.click(screen.getByRole("button", { name: /Sign Out/i }));
    await screen.findByPlaceholderText("Paste bearer token");
    expect(screen.queryByText("Last good history")).toBeNull();
    h.setRead(async () => observed("New principal history"));
    fireEvent.change(screen.getByPlaceholderText("Paste bearer token"), { target: { value: "other-token" } });
    fireEvent.click(screen.getByRole("button", { name: "Save Token" }));
    await screen.findByText("New principal history");
    await act(async () => { release(response({ session_id: "session-a", turns: [turn({ index: 1, content: "Old principal secret" })], queue: [{ id: "secret", prompt: "Old principal queue" }], is_busy: true })); await pending; });
    expect(screen.queryByText("Old principal secret")).toBeNull();
    expect(screen.queryByText("Old principal queue")).toBeNull();
    expect(screen.getByText("New principal history")).toBeInTheDocument();
    h.unmount();
  });

  it("rejects old auth initial bulk work after reauthentication", async () => {
    const h = harness();
    let release!: (result: never) => void;
    const pending = new Promise<never>((resolve) => { release = resolve; });
    vi.spyOn(h.api, "getSettings").mockImplementationOnce(() => pending);
    await screen.findByText("Last good history");
    vi.spyOn(h.api, "logout").mockResolvedValue({} as never);
    vi.spyOn(h.api, "pair").mockResolvedValue({ authenticated: true, authority: { tier: "owner", login: "other-owner" } });
    fireEvent.click(screen.getByRole("button", { name: /Sign Out/i }));
    await screen.findByPlaceholderText("Paste bearer token");
    h.setSessions([{ id: "new-session", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "new" }]);
    h.setRead(async () => observed("New initial history"));
    fireEvent.change(screen.getByPlaceholderText("Paste bearer token"), { target: { value: "other-token" } });
    fireEvent.click(screen.getByRole("button", { name: "Save Token" }));
    await screen.findByText("New initial history");
    const readsBefore = h.fetchImpl.mock.calls.filter(([input]) => String(input).includes("/transcript")).length;
    await act(async () => { release({} as never); await pending; });
    const laterTargets = h.fetchImpl.mock.calls.filter(([input]) => String(input).includes("/transcript")).slice(readsBefore);
    expect(laterTargets.every(([input]) => String(input).includes("/new-session/transcript"))).toBe(true);
    expect(screen.getByText("New initial history")).toBeInTheDocument();
    h.unmount();
  });

  it("does not start a queued refresh after unmount", async () => {
    const h = harness(); await screen.findByText("Last good history");
    let release!: (result: Response) => void;
    const pending = new Promise<Response>((resolve) => { release = resolve; });
    let started = false;
    h.setRead(async () => { started = true; return pending; }); h.refresh();
    await waitFor(() => expect(started).toBe(true));
    h.refresh(); h.unmount();
    const before = h.fetchImpl.mock.calls.length;
    await act(async () => { release(observed("Unmounted history")); await pending; });
    expect(h.fetchImpl).toHaveBeenCalledTimes(before);
  });

  it("retains confirmed optimistic output during initial unavailable reads without calling it observed history", async () => {
    const h = harness(async () => response({ code: "transcript_unavailable" }, 503));
    await screen.findByText("History unavailable");
    await waitFor(() => expect(h.fetchImpl.mock.calls.some(([input]) => String(input).includes("/session-a/transcript"))).toBe(true));
    h.refresh({ session_id: "session-a", persona: "ferris", effective_body: "Confirmed outbound message", delivered: true });
    await waitFor(() => expect(within(screen.getByTestId("turn-card-0")).getByText("Confirmed outbound message")).toBeInTheDocument());
    await screen.findByText("History unavailable");
    expect(within(screen.getByTestId("turn-card-0")).getByText("Confirmed outbound message")).toBeInTheDocument();
    expect(screen.queryByText("History refresh unavailable; showing last loaded history")).toBeNull();
    h.unmount();
  });

  it("rejects a background predecessor after the same persona's concrete session changes", async () => {
    window.localStorage.setItem(LAST_SEEN_ASSISTANT_TURN_STORAGE_KEY, JSON.stringify({ jarvis: 0 }));
    let release!: (result: Response) => void;
    const pending = new Promise<Response>((resolve) => { release = resolve; });
    let oldStarted = false;
    const h = harness(async (target) => {
      if (target.includes("jarvis-old")) { oldStarted = true; return pending; }
      if (target.includes("jarvis-new")) return response({ session_id: "jarvis-new", turns: [turn({ index: 1, content: "Current background" })] });
      return observed("Last good history");
    });
    const main = { id: "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" };
    h.setSessions([main, { ...main, id: "jarvis-old", persona: "jarvis" }]);
    await screen.findByText("Last good history");
    await waitFor(() => expect(oldStarted).toBe(true));
    h.setSessions([main, { ...main, id: "jarvis-new", persona: "jarvis" }]); h.refresh();
    await waitFor(() => expect(screen.getByTestId("unread-badge-jarvis")).toHaveTextContent("1"));
    await act(async () => { release(response({ session_id: "jarvis-old", turns: [turn({ index: 7, content: "Old background one" }), turn({ index: 8, content: "Old background two" })], is_busy: true, context_usage: { used_tokens: 199999, max_tokens: 200000 } })); await pending; });
    expect(screen.getByTestId("unread-badge-jarvis")).toHaveTextContent("1");
    h.setRead(async () => response({ code: "transcript_unavailable" }, 503));
    fireEvent.click(screen.getByTestId("agent-row-jarvis"));
    await screen.findByText("History unavailable");
    expect(screen.queryByText("Current background")).toBeNull();
    expect(screen.queryByText("Old background one")).toBeNull();
    expect(screen.queryByText("Last good history")).toBeNull();
    h.unmount();
  });

  it("preserves independently observed busy and queue metadata during a transient read failure", async () => {
    const h = harness(async () => response({ session_id: "session-a", turns: [turn({ index: 0, content: "Busy history" })], is_busy: true,
      queue: [{ id: "held", session_id: "session-a", prompt: "Held queue metadata", by: "owner", created_at: "now" }],
      context_usage: { used_tokens: 100000, max_tokens: 200000, source: "reported" } }));
    await screen.findByText("Busy history");
    await screen.findByText("Held queue metadata");
    h.setRead(async () => response({ code: "transcript_unavailable" }, 503)); h.refresh();
    await screen.findByText("History refresh unavailable; showing last loaded history");
    expect(screen.getByText("Held queue metadata")).toBeInTheDocument();
    expect(screen.getByText("Busy history")).toBeInTheDocument();
    expect(screen.getByTestId("prompt-queue-bar")).toBeInTheDocument();
    h.unmount();
  });

  it("does not unmask a previous session's queue after the successor's initial failure", async () => {
    const h = harness(async () => response({ session_id: "session-a", turns: [turn({ index: 0, content: "Session A metadata owner" })], is_busy: true,
      queue: [{ id: "previous-queue", session_id: "session-a", prompt: "Previous session queue", by: "owner", created_at: "now" }],
      context_usage: { used_tokens: 190000, max_tokens: 200000 } }));
    await screen.findByText("Session A metadata owner");
    await screen.findByText("Previous session queue");
    h.setSessions([{ id: "session-b", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t2" }, { id: "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" }]);
    h.setRead(async () => response({ code: "transcript_unavailable" }, 503)); h.refresh();
    await screen.findByText("History unavailable");
    expect(screen.queryByText("Previous session queue")).toBeNull();
    expect(screen.queryByTestId("live-thinking-indicator")).toBeNull();
    h.unmount();
  });

  it("defaults predecessor metadata when a discovered successor omits queue and busy", async () => {
    const h = harness(async () => response({ session_id: "session-a", turns: [turn({ index: 0, content: "Rich A history" })], is_busy: true,
      queue: [{ id: "old-rich", session_id: "session-a", prompt: "Rich A queue", by: "owner", created_at: "now" }],
      context_usage: { used_tokens: 190000, max_tokens: 200000 }, quota_usage: { exhausted_reason: "Rich A quota" } }));
    await screen.findByText("Rich A queue");
    expect(screen.getByTestId("quota-exhausted-toolbar-pill")).toBeInTheDocument();
    h.setSessions([{ id: "session-b", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t2" }]);
    h.setRead(async () => response({ session_id: "session-b", resolved_session_id: "session-b", turns: [turn({ index: 0, content: "Partial B history" })] })); h.refresh();
    await screen.findByText("Partial B history");
    expect(screen.getByTestId("context-ring-ferris")).not.toHaveAttribute("data-percent", "95");
    expect(screen.queryByText("Rich A queue")).toBeNull();
    expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Idle");
    expect(screen.queryByTestId("quota-exhausted-toolbar-pill")).toBeNull();
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    expect(screen.getByTestId("context-level-text")).not.toHaveTextContent("190k");
    expect(screen.queryByTestId("context-max")).toBeNull();
    h.unmount();
  });

  it("preserves same-owner independently observed queue and busy across a partial transcript", async () => {
    const h = harness(); await screen.findByText("Last good history");
    h.setRead(async () => response({ code: "transcript_unavailable" }, 503));
    h.refresh({ session_id: "session-a", persona: "ferris", delivered: false, is_busy: true,
      queue: [{ id: "independent", session_id: "session-a", prompt: "Independent current queue", by: "owner", created_at: "now" }] });
    await screen.findByText("Independent current queue");
    await screen.findByText("History refresh unavailable; showing last loaded history");
    h.setRead(async () => response({ session_id: "session-a", turns: [turn({ index: 0, content: "Partial current history" })] })); h.refresh();
    await screen.findByText("Partial current history");
    expect(screen.getByText("Independent current queue")).toBeInTheDocument();
    expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Busy");
    h.unmount();
  });

  it.each(["clear", "reforge", "teardown"] as const)("successful %s invalidates its unselected target while preserving another agent", async (mode) => {
    const h = harness();
    h.setSessions([
      { id: "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" },
      { id: "session-b", source: "route", adapter: "claude", persona: "jarvis", status: "active", updated_at: "t2" },
    ]);
    await screen.findByText("Last good history");
    h.refresh({ session_id: "session-a", persona: "ferris", effective_body: "A pre-reset optimism", delivered: true });
    await waitFor(() => expect(within(screen.getByTestId("turn-card-1")).getByText("A pre-reset optimism")).toBeInTheDocument());
    let releaseRead!: (value: Response) => void;
    const late = new Promise<Response>((resolve) => { releaseRead = resolve; });
    let oldReadStarted = false;
    h.setRead(async () => { oldReadStarted = true; return late; }); h.refresh();
    await waitFor(() => expect(oldReadStarted).toBe(true));
    let finish!: (value: never) => void;
    const mutation = new Promise<never>((resolve) => { finish = resolve; });
    const reset = vi.spyOn(h.api, mode === "teardown" ? "decommissionSession" : "clearSession").mockReturnValue(mutation);
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    if (mode === "teardown") {
      fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
      fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
      fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));
    } else {
      fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
      fireEvent.click(screen.getByTestId(`clear-modal-${mode}-btn`));
    }
    await waitFor(() => expect(reset).toHaveBeenCalledOnce());
    h.setRead(async (target) => target.includes("session-a") ? late : response({ session_id: "session-b", turns: [turn({ index: 0, content: "B fresh history" })],
      queue: [{ id: "b-fresh", session_id: "session-b", prompt: "B fresh queue", by: "owner", created_at: "now" }], is_busy: true,
      context_usage: { used_tokens: 50000, max_tokens: 200000 }, quota_usage: { exhausted_reason: "B fresh quota" } }));
    fireEvent.click(screen.getByTestId("agent-row-jarvis"));
    await screen.findByText("B fresh history"); h.refresh();
    await screen.findByText("B fresh queue");
    h.refresh({ session_id: "session-b", persona: "jarvis", effective_body: "B fresh optimism", delivered: true });
    await waitFor(() => expect(within(screen.getByTestId("turn-card-1")).getByText("B fresh optimism")).toBeInTheDocument());
    await act(async () => { finish({ ok: true } as never); await mutation; });
    await act(async () => { releaseRead(observed("Late A before-reset data")); await late; });
    expect(screen.getByText("B fresh history")).toBeInTheDocument();
    expect(screen.getByText("B fresh queue")).toBeInTheDocument();
    expect(within(screen.getByTestId("turn-card-1")).getByText("B fresh optimism")).toBeInTheDocument();
    expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Busy");
    expect(screen.getByTestId("quota-exhausted-toolbar-pill")).toHaveTextContent("B fresh quota");
    h.setRead(async () => response({ code: "transcript_unavailable" }, 503));
    fireEvent.click(screen.getByTestId("agent-row-ferris"));
    await act(async () => {});
    expect(screen.queryAllByText("A pre-reset optimism")).toHaveLength(0);
    expect(screen.queryByText("Last good history")).toBeNull();
    expect(screen.queryByText("Late A before-reset data")).toBeNull();
    h.unmount();
  });

  it.each(["clear", "reforge", "teardown"] as const)("successful %s preserves a discovered new session for the same persona", async (mode) => {
    const h = harness(); await screen.findByText("Last good history");
    h.refresh({ session_id: "session-a", persona: "ferris", effective_body: "Old concrete optimism", delivered: true });
    await waitFor(() => expect(within(screen.getByTestId("turn-card-1")).getByText("Old concrete optimism")).toBeInTheDocument());
    let finish!: (value: never) => void;
    const pending = new Promise<never>((resolve) => { finish = resolve; });
    const reset = vi.spyOn(h.api, mode === "teardown" ? "decommissionSession" : "clearSession").mockReturnValue(pending);
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    if (mode === "teardown") {
      fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
      fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
      fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));
    } else {
      fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
      fireEvent.click(screen.getByTestId(`clear-modal-${mode}-btn`));
    }
    await waitFor(() => expect(reset).toHaveBeenCalledOnce());
    h.setSessions([{ id: "session-b", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t2" }]);
    h.setRead(async () => response({ session_id: "session-b", turns: [turn({ index: 0, content: "Discovered fresh B" })],
      queue: [{ id: "new-concrete", session_id: "session-b", prompt: "New concrete queue", by: "owner", created_at: "now" }], is_busy: true })); h.refresh();
    await screen.findByText("Discovered fresh B");
    h.refresh({ session_id: "session-b", persona: "ferris", effective_body: "New concrete optimism", delivered: true });
    await waitFor(() => expect(within(screen.getByTestId("turn-card-1")).getByText("New concrete optimism")).toBeInTheDocument());
    await act(async () => { finish({ ok: true } as never); await pending; });
    expect(screen.getByText("Discovered fresh B")).toBeInTheDocument();
    expect(screen.getByText("New concrete queue")).toBeInTheDocument();
    expect(within(screen.getByTestId("turn-card-1")).getByText("New concrete optimism")).toBeInTheDocument();
    h.setRead(async () => response({ code: "transcript_unavailable" }, 503));
    h.setSessions([{ id: "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t3" }]); h.refresh();
    await waitFor(() => expect(screen.queryByText("Discovered fresh B")).toBeNull());
    expect(screen.queryAllByText("Old concrete optimism")).toHaveLength(0);
    h.unmount();
  });

  it("completing an unselected teardown does not supersede the successor's pending read", async () => {
    const h = harness();
    h.setSessions([
      { id: "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" },
      { id: "session-b", source: "route", adapter: "claude", persona: "jarvis", status: "active", updated_at: "t1" },
    ]);
    await screen.findByText("Last good history");
    let finish!: (value: never) => void;
    const mutation = new Promise<never>((resolve) => { finish = resolve; });
    const reset = vi.spyOn(h.api, "decommissionSession").mockReturnValue(mutation);
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));
    await waitFor(() => expect(reset).toHaveBeenCalledOnce());
    let release!: (value: Response) => void;
    const read = new Promise<Response>((resolve) => { release = resolve; });
    let started = false;
    h.setRead(async () => { started = true; return read; });
    fireEvent.click(screen.getByTestId("agent-row-jarvis"));
    await waitFor(() => expect(started).toBe(true));
    await act(async () => { finish({ ok: true } as never); await mutation; });
    await act(async () => { release(response({ session_id: "session-b", turns: [turn({ index: 0, content: "Successor pending observation" })] })); await read; });
    await screen.findByText("Successor pending observation");
    h.unmount();
  });

  it.each(["clear", "reforge", "teardown"] as const)("pending %s suppresses target reads after navigating back and failure releases them", async (mode) => {
    const h = harness(); await screen.findByText("Last good history");
    h.refresh({ session_id: "session-a", persona: "ferris", effective_body: "Failure keeps A optimism", delivered: true });
    await waitFor(() => expect(within(screen.getByTestId("turn-card-1")).getByText("Failure keeps A optimism")).toBeInTheDocument());
    let fail!: (error: Error) => void;
    const pending = new Promise<never>((_resolve, reject) => { fail = reject; });
    const reset = vi.spyOn(h.api, mode === "teardown" ? "decommissionSession" : "clearSession").mockReturnValue(pending);
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    if (mode === "teardown") {
      fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
      fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
      fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));
    } else {
      fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
      fireEvent.click(screen.getByTestId(`clear-modal-${mode}-btn`));
    }
    await waitFor(() => expect(reset).toHaveBeenCalledOnce());
    h.setRead(async (target) => target.includes("session-a") ? observed("Premature target observation") : observed("Other agent fresh"));
    fireEvent.click(screen.getByTestId("agent-row-jarvis"));
    await screen.findByText("Other agent fresh");
    fireEvent.click(screen.getByTestId("agent-row-ferris"));
    await act(async () => {});
    expect(screen.queryByText("Premature target observation")).toBeNull();
    // Reject while another view is selected: it must remain usable and A's optimism must survive.
    fireEvent.click(screen.getByTestId("agent-row-jarvis"));
    await screen.findByText("Other agent fresh");
    await act(async () => { fail(new Error("Navigated reset refused")); await pending.catch(() => {}); });
    expect(screen.getByText("Other agent fresh")).toBeInTheDocument();
    h.setRead(async () => observed("After navigated failure"));
    fireEvent.click(screen.getByTestId("agent-row-ferris"));
    await screen.findByText("After navigated failure");
    expect(within(screen.getByTestId("turn-card-1")).getByText("Failure keeps A optimism")).toBeInTheDocument();
    h.unmount();
  });

  it.each(["cancel", "edit"] as const)("ignores a late %s reply after a different session installs metadata", async (operation) => {
    const item = { id: "old-operation", session_id: "session-a", prompt: "Old operation queue", by: "owner", created_at: "now" };
    const h = harness(async () => response({ session_id: "session-a", turns: [turn({ index: 0, content: "Original owner" })], queue: [item], is_busy: false }));
    await screen.findByText("Old operation queue");
    let release!: (value: never) => void;
    const pending = new Promise<never>((resolve) => { release = resolve; });
    vi.spyOn(h.api, operation === "cancel" ? "cancelQueuedPrompt" : "updateQueuedPrompt").mockReturnValue(pending);
    if (operation === "edit") {
      fireEvent.click(screen.getByTestId("queued-edit-old-operation"));
      fireEvent.change(screen.getByTestId("queued-edit-input-old-operation"), { target: { value: "Old edit" } });
      fireEvent.click(screen.getByTestId("queued-save-old-operation"));
    } else fireEvent.click(screen.getByTestId("queued-cancel-old-operation"));
    h.setSessions([{ id: "session-b", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t2" }]);
    h.setRead(async () => response({ session_id: "session-b", turns: [turn({ index: 0, content: "Successor owner" })],
      queue: [{ ...item, id: "new-operation", session_id: "session-b", prompt: "Successor queue" }], is_busy: true })); h.refresh();
    await screen.findByText("Successor queue");
    await act(async () => { release({ queue: [], is_busy: false } as never); await pending; });
    expect(screen.getByText("Successor queue")).toBeInTheDocument();
    expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Busy");
    h.unmount();
  });

  it.each(["cancel", "edit"] as const)("keeps a newer %s observation when an older App transcript completes", async (operation) => {
    const item = { id: "operation-read", session_id: "session-a", prompt: "Original operation queue", by: "owner", created_at: "now" };
    const h = harness(async () => response({ session_id: "session-a", turns: [turn({ index: 0, content: "Operation owner history" })], queue: [item], is_busy: false }));
    await screen.findByText("Original operation queue");
    let release!: (value: Response) => void;
    const pending = new Promise<Response>((resolve) => { release = resolve; });
    let started = false;
    h.setRead(async () => { started = true; return pending; }); h.refresh();
    await waitFor(() => expect(started).toBe(true));
    const queue = operation === "cancel" ? [] : [{ ...item, prompt: "Edited operation queue" }];
    vi.spyOn(h.api, operation === "cancel" ? "cancelQueuedPrompt" : "updateQueuedPrompt").mockResolvedValue({ queue, is_busy: true } as never);
    if (operation === "edit") {
      fireEvent.click(screen.getByTestId("queued-edit-operation-read"));
      fireEvent.change(screen.getByTestId("queued-edit-input-operation-read"), { target: { value: "Edited operation queue" } });
      fireEvent.click(screen.getByTestId("queued-save-operation-read"));
      await screen.findByText("Edited operation queue");
    } else {
      fireEvent.click(screen.getByTestId("queued-cancel-operation-read"));
      await waitFor(() => expect(screen.queryByText("Original operation queue")).toBeNull());
    }
    await act(async () => { release(response({ session_id: "session-a", turns: [turn({ index: 0, content: "Obsolete operation history" })], queue: [item], is_busy: false })); await pending; });
    expect(screen.queryByText("Obsolete operation history")).toBeNull();
    expect(screen.getByText("Operation owner history")).toBeInTheDocument();
    expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Busy");
    expect(screen.queryByText("Original operation queue")).toBeNull();
    h.unmount();
  });

  it.each(["partial send", "empty send", "partial event", "empty event"] as const)("does not relabel predecessor metadata after an unavailable successor and %s", async (operation) => {
    const h = harness(async () => response({ session_id: "session-a", turns: [turn({ index: 0, content: "Rich predecessor" })], is_busy: true,
      queue: [{ id: "predecessor", session_id: "session-a", prompt: "Predecessor queue", by: "owner", created_at: "now" }],
      context_usage: { used_tokens: 190000, max_tokens: 200000 }, quota_usage: { exhausted_reason: "Predecessor quota exhausted" } }));
    await screen.findByText("Rich predecessor");
    expect(screen.getByTestId("quota-exhausted-toolbar-pill")).toBeInTheDocument();
    h.setSessions([{ id: "session-b", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t2" }]);
    h.setRead(async () => response({ code: "transcript_unavailable" }, 503)); h.refresh();
    await screen.findByText("History unavailable");
    if (operation === "partial send" || operation === "empty send") {
      vi.spyOn(h.api, "sendSessionPrompt").mockResolvedValue({ session_id: "session-b", delivered: true, queued: false, ...(operation === "partial send" ? { is_busy: false } : {}) } as never);
      fireEvent.change(screen.getByLabelText("Message"), { target: { value: "Successor send" } });
      fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
      await screen.findByText("Successor send");
    } else h.refresh({ session_id: "session-b", persona: "ferris", delivered: false, ...(operation === "partial event" ? { is_busy: false } : {}) });
    await act(async () => {});
    expect(screen.queryByText("Predecessor queue")).toBeNull();
    expect(screen.queryByTestId("quota-exhausted-toolbar-pill")).toBeNull();
    expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Idle");
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    expect(screen.getByTestId("context-level-text")).not.toHaveTextContent("190k");
    expect(screen.queryByTestId("context-max")).toBeNull();
    expect(screen.getByText("History unavailable")).toBeInTheDocument();
    h.unmount();
  });

  it("preserves metadata installed for the current owner while its partial send reply was pending", async () => {
    const h = harness(async () => response({ code: "transcript_unavailable" }, 503));
    await screen.findByText("History unavailable");
    let release!: (value: never) => void;
    const pending = new Promise<never>((resolve) => { release = resolve; });
    vi.spyOn(h.api, "sendSessionPrompt").mockReturnValue(pending);
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "Pending partial send" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    h.setRead(async () => response({ session_id: "session-a", turns: [turn({ index: 0, content: "Current owner observation" })],
      queue: [{ id: "current", session_id: "session-a", prompt: "Current owner queue", by: "owner", created_at: "now" }], is_busy: true,
      context_usage: { used_tokens: 50000, max_tokens: 200000 }, quota_usage: { exhausted_reason: "Current owner quota" } })); h.refresh();
    await screen.findByText("Current owner observation");
    h.setRead(async () => response({ code: "transcript_unavailable" }, 503));
    await act(async () => { release({ session_id: "session-a", delivered: true, queued: false, is_busy: false } as never); await pending; });
    await screen.findByText("History refresh unavailable; showing last loaded history");
    expect(screen.getByText("Current owner queue")).toBeInTheDocument();
    expect(screen.getByTestId("quota-exhausted-toolbar-pill")).toHaveTextContent("Current owner quota");
    expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Idle");
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    expect(screen.getByTestId("context-max")).toHaveTextContent("200k");
    expect(screen.getByTestId("context-level-text")).toHaveTextContent("50k");
    h.unmount();
  });

  it("allows a new client's reads while the old client's reset is pending", async () => {
    const h = harness(); await screen.findByText("Last good history");
    let release!: (result: never) => void;
    const pending = new Promise<never>((resolve) => { release = resolve; });
    const clear = vi.spyOn(h.api, "clearSession").mockImplementation(() => pending);
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
    fireEvent.click(screen.getByTestId("clear-modal-clear-btn"));
    await waitFor(() => expect(clear).toHaveBeenCalledOnce());
    h.setRead(async () => observed("New client history"));
    const nextApi = new HubApiClient({ fetchImpl: h.fetchImpl as typeof fetch });
    h.rerender(<App apiClient={nextApi} wsClient={h.ws} />);
    expect(screen.queryByText("Last good history")).toBeNull();
    await screen.findByText("New client history");
    await act(async () => { release({ ok: true } as never); await pending; });
    expect(screen.getByText("New client history")).toBeInTheDocument();
    h.unmount();
  });

  it("rejects a pre-send transcript after a newer refused queue observation", async () => {
    const item = { id: "pre-send-read", session_id: "session-a", prompt: "Guarded queued prompt", by: "owner", created_at: "now" };
    const initial = () => response({ session_id: "session-a", turns: [turn({ index: 0, content: "Queue owner history" })], queue: [item], is_busy: false });
    const h = harness(async () => initial());
    await screen.findByText("Queue owner history");
    let release!: (result: Response) => void;
    const pending = new Promise<Response>((resolve) => { release = resolve; });
    let started = false;
    h.setRead(async () => { started = true; return pending; }); h.refresh();
    await waitFor(() => expect(started).toBe(true));
    vi.spyOn(h.api, "sendQueuedPromptNow").mockResolvedValue({ delivered: false, delivery_stage: "refused", delivery_error: "Queue refused",
      queue: [{ ...item, delivery_state: "refused" }] } as never);
    fireEvent.click(screen.getByTestId("queued-send-now-pre-send-read"));
    await screen.findByText("Queue refused");
    expect(screen.getByTestId("queued-send-now-pre-send-read")).toBeDisabled();
    await act(async () => { release(initial()); await pending; });
    expect(screen.getByTestId("queued-send-now-pre-send-read")).toBeDisabled();
    expect(screen.getByText("Queue owner history")).toBeInTheDocument();
    h.unmount();
  });

  it.each(["client", "token"] as const)("releases retired refresh coordination on automatic %s change", async (change) => {
    const h = harness(); await screen.findByText("Last good history");
    let releaseOld!: (result: Response) => void;
    const oldPending = new Promise<Response>((resolve) => { releaseOld = resolve; });
    let oldStarted = false;
    h.setRead(async () => { oldStarted = true; return oldPending; }); h.refresh();
    await waitFor(() => expect(oldStarted).toBe(true));
    h.setRead(async () => observed("New authority poll"));
    const nextApi = change === "client" ? new HubApiClient({ fetchImpl: h.fetchImpl as typeof fetch }) : h.api;
    if (change === "token") nextApi.setToken("replacement-token");
    h.rerender(<App apiClient={nextApi} wsClient={h.ws} />);
    h.refresh();
    await screen.findByText("New authority poll");
    expect(screen.queryByText("Last good history")).toBeNull();

    let releaseNew!: (result: Response) => void;
    const newPending = new Promise<Response>((resolve) => { releaseNew = resolve; });
    let newStarted = false;
    h.setRead(async () => { newStarted = true; return newPending; }); h.refresh();
    await waitFor(() => expect(newStarted).toBe(true));
    h.setRead(async () => observed("Trailing current authority")); h.refresh();
    await act(async () => { releaseOld(observed("Retired authority secret")); await oldPending; });
    expect(screen.queryByText("Retired authority secret")).toBeNull();
    expect(screen.queryByText("Trailing current authority")).toBeNull();
    expect(screen.getByText("New authority poll")).toBeInTheDocument();
    await act(async () => { releaseNew(observed("Current pending result")); await newPending; });
    await screen.findByText("Trailing current authority");
    h.unmount();
  });

  it.each([
    ["null tool call", { tool_calls: [null] }],
    ["invalid tool name", { tool_calls: [{ name: 9 }] }],
    ["invalid timestamp", { created_at: 42 }],
  ])("treats foreground %s as unavailable and preserves last good history", async (_name, malformed) => {
    const h = harness(); await screen.findByText("Last good history");
    h.setRead(async () => response({ session_id: "session-a", turns: [{ index: 8, role: "assistant", content: "Malformed foreground", ...malformed }], queue: [], is_busy: false }));
    h.refresh();
    await screen.findByText("History refresh unavailable; showing last loaded history");
    expect(screen.getByText("Last good history")).toBeInTheDocument();
    expect(screen.queryByText("Malformed foreground")).toBeNull();
    h.unmount();
  });

  it("shows unavailable for initial malformed nested history without inventing empty history", async () => {
    const h = harness(async () => response({ session_id: "session-a", turns: [{ index: 1, role: "assistant", content: "Malformed initial", tool_calls: [null] }] }));
    await screen.findByText("History unavailable");
    expect(screen.queryByTestId("chat-empty-state")).toBeNull();
    expect(screen.queryByText("Malformed initial")).toBeNull();
    h.unmount();
  });

  it.each([
    ["null tool call", { tool_calls: [null] }],
    ["invalid timestamp", { created_at: 42 }],
  ])("rejects background %s before caching badges or metadata", async (_name, malformed) => {
    window.localStorage.setItem(LAST_SEEN_ASSISTANT_TURN_STORAGE_KEY, JSON.stringify({ jarvis: 0 }));
    let backgroundRead = 0;
    let invalid = false;
    const h = harness(async (target) => {
      if (!target.includes("jarvis-session")) return observed("Last good history");
      backgroundRead++;
      return response({ session_id: "jarvis-session", turns: invalid
        ? [turn({ index: 1, content: "Good background" }), { index: 8, role: "assistant", content: "Malformed background", ...malformed }]
        : [turn({ index: 1, content: "Good background" })], is_busy: invalid,
        context_usage: { used_tokens: invalid ? 199999 : 1000, max_tokens: 200000 } });
    });
    const main = { id: "session-a", source: "route", adapter: "claude", persona: "ferris", status: "active", updated_at: "t1" };
    h.setSessions([main, { ...main, id: "jarvis-session", persona: "jarvis" }]);
    await screen.findByText("Last good history");
    await waitFor(() => expect(screen.getByTestId("unread-badge-jarvis")).toHaveTextContent("1"));
    expect(screen.getByTestId("context-ring-jarvis")).toHaveAttribute("data-percent", "1");
    invalid = true;
    h.setSessions([main, { ...main, id: "jarvis-session", persona: "jarvis", updated_at: "t2" }]); h.refresh();
    await waitFor(() => expect(backgroundRead).toBeGreaterThan(1));
    await act(async () => {});
    expect(screen.getByTestId("unread-badge-jarvis")).toHaveTextContent("1");
    expect(screen.getByTestId("context-ring-jarvis")).toHaveAttribute("data-percent", "1");
    // Invalid observations must not stamp the cache as successfully read: retry the same stamp.
    invalid = false; h.refresh();
    await waitFor(() => expect(backgroundRead).toBeGreaterThan(2));
    h.unmount();
  });

  it("keeps matching prompt_sent holds and busy metadata when a pre-event read finishes", async () => {
    const h = harness(async () => response({ session_id: "session-a", turns: [turn({ index: 0, content: "Event owner history" })], queue: [], is_busy: false,
      context_usage: { used_tokens: 100000, max_tokens: 200000 } }));
    await screen.findByText("Event owner history");
    expect(screen.getByTestId("context-ring-ferris")).toHaveAttribute("data-percent", "50");
    let release!: (result: Response) => void;
    const pending = new Promise<Response>((resolve) => { release = resolve; });
    let started = false;
    h.setRead(async () => { started = true; return pending; }); h.refresh();
    await waitFor(() => expect(started).toBe(true));
    h.setRead(async () => response({ code: "transcript_unavailable" }, 503));
    h.refresh({ session_id: "session-a", persona: "ferris", delivered: false, is_busy: true,
      queue: [{ id: "event-held", session_id: "session-a", prompt: "Event held queue", by: "owner", created_at: "now", delivery_state: "unconfirmed" }] });
    await screen.findByText("Event held queue");
    expect(screen.getByTestId("queued-send-now-event-held")).toBeDisabled();
    await act(async () => { release(response({ session_id: "session-a", turns: [turn({ index: 9, content: "Obsolete event predecessor" })], queue: [], is_busy: false,
      context_usage: { used_tokens: 199999, max_tokens: 200000 } })); await pending; });
    await screen.findByText("History refresh unavailable; showing last loaded history");
    expect(screen.getByText("Event held queue")).toBeInTheDocument();
    expect(screen.getByTestId("queued-send-now-event-held")).toBeDisabled();
    expect(screen.getByTestId("session-busy-badge")).toBeInTheDocument();
    expect(screen.getByTestId("context-ring-ferris")).toHaveAttribute("data-percent", "50");
    expect(screen.queryByText("Obsolete event predecessor")).toBeNull();
    expect(screen.getByText("Event owner history")).toBeInTheDocument();
    h.unmount();
  });

});
