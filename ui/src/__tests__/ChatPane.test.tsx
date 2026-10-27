import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ChatPane } from "../components/ChatPane";
import type { TranscriptTurn } from "../types/api";

describe("ChatPane", () => {
  const sampleTurns: TranscriptTurn[] = [
    {
      index: 0,
      role: "user",
      content: "Check the status of AI-Lab-0 services and inspect the worktree.",
      thinking: null,
      tool_calls: [],
      created_at: "2026-10-27T04:10:00Z",
    },
    {
      index: 1,
      role: "assistant",
      content: "All AI-Lab-0 services are healthy.\n```bash\nsystemctl status fl-hub\n```",
      thinking: "First I should inspect the hub health endpoint.\nThen verify the worktree status.",
      tool_calls: [
        {
          name: "run_command",
          arguments: {
            command: "fl status",
            cwd: "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop",
          },
        },
      ],
      created_at: "2026-10-27T04:10:05Z",
    },
  ];

  it("renders user and assistant TranscriptTurn items with prose and code blocks", () => {
    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-ferris-01"
        turns={sampleTurns}
        onSendPrompt={vi.fn()}
      />,
    );

    expect(
      screen.getByText(/Check the status of AI-Lab-0 services and inspect the worktree\./i),
    ).toBeInTheDocument();
    expect(screen.getByText(/All AI-Lab-0 services are healthy\./i)).toBeInTheDocument();
    expect(screen.getByText(/systemctl status fl-hub/i)).toBeInTheDocument();
  });

  it("expands and collapses ThinkingBlock when clicking its toggle button", () => {
    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-ferris-01"
        turns={sampleTurns}
        onSendPrompt={vi.fn()}
      />,
    );

    // Collapsed by default
    const toggleBtn = screen.getByRole("button", { name: /Thinking trace/i });
    expect(toggleBtn).toHaveAttribute("aria-expanded", "false");
    expect(
      screen.queryByText(/First I should inspect the hub health endpoint\./i),
    ).toBeNull();

    // Expand
    fireEvent.click(toggleBtn);
    expect(toggleBtn).toHaveAttribute("aria-expanded", "true");
    expect(
      screen.getByText(/First I should inspect the hub health endpoint\./i),
    ).toBeInTheDocument();

    // Collapse again
    fireEvent.click(toggleBtn);
    expect(toggleBtn).toHaveAttribute("aria-expanded", "false");
    expect(
      screen.queryByText(/First I should inspect the hub health endpoint\./i),
    ).toBeNull();
  });

  it("expands and collapses ToolCallBlock when clicking its toggle button and displays formatted JSON arguments", () => {
    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-ferris-01"
        turns={sampleTurns}
        onSendPrompt={vi.fn()}
      />,
    );

    const toolBtn = screen.getByRole("button", { name: /run_command/i });
    expect(toolBtn).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByText(/"fl status"/i)).toBeNull();

    // Expand tool block
    fireEvent.click(toolBtn);
    expect(toolBtn).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText(/"fl status"/i)).toBeInTheDocument();
    expect(
      screen.getByText(/\/home\/lmcfarlin\/Projects\/Artificial-Humanity\/FerroLoop/i),
    ).toBeInTheDocument();

    // Collapse tool block
    fireEvent.click(toolBtn);
    expect(toolBtn).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByText(/"fl status"/i)).toBeNull();
  });

  it("submits prompt with urgency and subject via Send Prompt button and Ctrl+Enter, then clears input", async () => {
    const onSendPrompt = vi.fn(async () => {});

    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-ferris-01"
        turns={[]}
        onSendPrompt={onSendPrompt}
      />,
    );

    // Empty state is shown when no turns exist
    expect(screen.getByTestId("chat-empty-state")).toBeInTheDocument();

    const promptInput = screen.getByPlaceholderText(/Send a prompt/i) as HTMLTextAreaElement;
    const urgencySelect = screen.getByLabelText(/Urgency/i) as HTMLSelectElement;
    const subjectInput = screen.getByPlaceholderText(/Optional subject/i) as HTMLInputElement;
    const sendButton = screen.getByRole("button", { name: /Send Prompt/i });

    fireEvent.change(promptInput, { target: { value: "Run fl arch check now" } });
    fireEvent.change(urgencySelect, { target: { value: "urgent" } });
    fireEvent.change(subjectInput, { target: { value: "Architecture check" } });

    fireEvent.click(sendButton);

    await waitFor(() => {
      expect(onSendPrompt).toHaveBeenCalledWith("sess-ferris-01", {
        prompt: "Run fl arch check now",
        urgency: "urgent",
        subject: "Architecture check",
      });
    });

    await waitFor(() => {
      expect(promptInput.value).toBe("");
    });

    // Test Ctrl+Enter shortcut fallback to agentName when sessionId is null
    fireEvent.change(promptInput, { target: { value: "Second prompt via shortcut" } });
    fireEvent.keyDown(promptInput, { key: "Enter", ctrlKey: true });

    await waitFor(() => {
      expect(onSendPrompt).toHaveBeenCalledWith("sess-ferris-01", {
        prompt: "Second prompt via shortcut",
        urgency: "urgent",
        subject: undefined,
      });
    });
  });
});
