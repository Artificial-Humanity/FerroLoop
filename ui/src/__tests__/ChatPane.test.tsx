import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  ALWAYS_SEND_NOW_STORAGE_KEY,
  ChatPane,
  sessionScrollMemory,
} from "../components/ChatPane";
import type { QueuedOperatorPrompt, TranscriptTurn } from "../types/api";

describe("ChatPane", () => {
  beforeEach(() => {
    sessionScrollMemory.clear();
    window.localStorage.removeItem(ALWAYS_SEND_NOW_STORAGE_KEY);
  });
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

    const { rerender } = render(
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
    rerender(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId={null}
        turns={[]}
        onSendPrompt={onSendPrompt}
      />,
    );
    fireEvent.change(promptInput, { target: { value: "Second prompt via shortcut" } });
    fireEvent.keyDown(promptInput, { key: "Enter", ctrlKey: true });

    await waitFor(() => {
      expect(onSendPrompt).toHaveBeenCalledWith("ferris", {
        prompt: "Second prompt via shortcut",
        urgency: "urgent",
        subject: undefined,
      });
    });
  });

  it("auto-scrolls scrollTop to scrollHeight when turns load or sessionId changes, and remembers scroll position per session", () => {
    const { container, rerender } = render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-1"
        turns={[]}
        wsConnected={true}
        onSendPrompt={vi.fn()}
      />,
    );

    const scrollDiv = container.querySelector(".fd-scroll") as HTMLDivElement;
    expect(scrollDiv).toBeTruthy();

    Object.defineProperty(scrollDiv, "scrollHeight", {
      configurable: true,
      value: 920,
    });
    Object.defineProperty(scrollDiv, "clientHeight", {
      configurable: true,
      value: 300,
    });
    scrollDiv.scrollTop = 0;

    // Initial load of turns should auto-scroll to bottom
    rerender(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-1"
        turns={sampleTurns}
        wsConnected={true}
        onSendPrompt={vi.fn()}
      />,
    );
    expect(scrollDiv.scrollTop).toBe(920);

    // Simulate user scrolling up in sess-1 (not pinned to bottom)
    scrollDiv.scrollTop = 180;
    fireEvent.scroll(scrollDiv);

    // Switch to sess-2 (first visit -> should jump to bottom)
    Object.defineProperty(scrollDiv, "scrollHeight", {
      configurable: true,
      value: 1450,
    });

    rerender(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-2"
        turns={sampleTurns}
        wsConnected={true}
        onSendPrompt={vi.fn()}
      />,
    );
    expect(scrollDiv.scrollTop).toBe(1450);

    // Switch back to sess-1 -> should restore saved scrollTop (180)
    rerender(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-1"
        turns={sampleTurns}
        wsConnected={true}
        onSendPrompt={vi.fn()}
      />,
    );
    expect(scrollDiv.scrollTop).toBe(180);
  });

  it("applies anti-autofill attributes and renders composer as non-form region", () => {
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

    const composer = screen.getByTestId("prompt-composer");
    expect(composer.tagName.toLowerCase()).toBe("div");
    expect(composer).toHaveAttribute("role", "region");

    const filterInput = screen.getByLabelText("Filter transcript");
    const subjectInput = screen.getByLabelText("Prompt Subject");
    const promptTextarea = screen.getByLabelText("Prompt Message");

    for (const el of [filterInput, subjectInput, promptTextarea]) {
      expect(el).toHaveAttribute("autocomplete", "off");
      expect(el).toHaveAttribute("data-1p-ignore", "true");
      expect(el).toHaveAttribute("data-lpignore", "true");
      expect(el).toHaveAttribute("data-form-type", "other");
    }
  });

  it("attaches a file via prompt-file-input, shows attachment chip, and submits attachments in PromptRequest", async () => {
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

    const fileInput = screen.getByTestId("prompt-file-input") as HTMLInputElement;
    const file = new File(["PNGDATA"], "screenshot.png", { type: "image/png" });

    fireEvent.change(fileInput, { target: { files: [file] } });

    await waitFor(() => {
      expect(screen.getByTestId("attachment-chip-list")).toBeInTheDocument();
      expect(screen.getByText("screenshot.png")).toBeInTheDocument();
    });

    const sendButton = screen.getByRole("button", { name: /Send Prompt/i });
    expect(sendButton).not.toBeDisabled();
    fireEvent.click(sendButton);

    await waitFor(() => {
      expect(onSendPrompt).toHaveBeenCalledWith(
        "sess-ferris-01",
        expect.objectContaining({
          prompt: "Attached file(s).",
          urgency: "normal",
          attachments: [
            expect.objectContaining({
              name: "screenshot.png",
              media_type: "image/png",
              data_base64: btoa("PNGDATA"),
            }),
          ],
        }),
      );
    });

    await waitFor(() => {
      expect(screen.queryByTestId("attachment-chip-list")).toBeNull();
    });
  });

  it("renders Operator for user turns, Wire · sender for wire turns, and inline attachment image previews", () => {
    const turnsWithWireAndAttachment: TranscriptTurn[] = [
      {
        index: 0,
        role: "user",
        content: "Here is the iPad screenshot.\n[Attached file: /tmp/shot.png (/api/v1/attachments/shot.png)]",
        thinking: null,
        tool_calls: [],
      },
      {
        index: 1,
        role: "wire:jarvis",
        content: "Lab Manager review complete.",
        thinking: null,
        tool_calls: [],
      },
    ];

    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-ferris-01"
        turns={turnsWithWireAndAttachment}
        onSendPrompt={vi.fn()}
      />,
    );

    expect(screen.getByText("Operator")).toBeInTheDocument();
    expect(screen.getByText("Wire · jarvis")).toBeInTheDocument();

    const attachmentsBlock = screen.getByTestId("turn-attachments-0");
    expect(attachmentsBlock).toBeInTheDocument();
    const img = attachmentsBlock.querySelector("img") as HTMLImageElement;
    expect(img).toBeTruthy();
    expect(img.getAttribute("src")).toBe("/api/v1/attachments/shot.png");
    expect(img.getAttribute("alt")).toBe("shot.png");
  });

  it("renders tool/terminal output turns collapsed by default with ▶ and expands when clicked", () => {
    const toolTurns: TranscriptTurn[] = [
      {
        index: 0,
        role: "tool",
        content:
          "Created At: 2026-10-27T15:30:00Z\nCommand exited with code 0.\n\nCommand output:\ncargo test passed 42 tests",
        thinking: null,
        tool_calls: [],
      },
    ];

    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-ferris-01"
        turns={toolTurns}
        onSendPrompt={vi.fn()}
      />,
    );

    expect(screen.getByText("Tool Output")).toBeInTheDocument();
    const toggleBtn = screen.getByTestId("turn-tool-output-toggle-0");
    expect(toggleBtn).toHaveAttribute("aria-expanded", "false");
    expect(toggleBtn.textContent).toContain("▶");
    expect(screen.queryByText(/cargo test passed 42 tests/i)).toBeNull();

    fireEvent.click(toggleBtn);
    expect(toggleBtn).toHaveAttribute("aria-expanded", "true");
    expect(toggleBtn.textContent).toContain("▼");
    expect(screen.getByText(/cargo test passed 42 tests/i)).toBeInTheDocument();
  });

  it("renders Queued Prompts bar and dispatches Send Now or Remove actions", async () => {
    const onSendQueuedNow = vi.fn(async () => {});
    const onCancelQueued = vi.fn(async () => {});
    const queuedItems: QueuedOperatorPrompt[] = [
      {
        id: "qp-100-1",
        session_id: "sess-ferris-01",
        persona: "ferris",
        prompt: "First queued prompt in line",
        by: "owner",
        created_at: "2026-10-27T16:10:00Z",
      },
      {
        id: "qp-100-2",
        session_id: "sess-ferris-01",
        persona: "ferris",
        prompt: "Second queued prompt with attachment",
        by: "owner",
        attachments: [
          {
            name: "diag.png",
            media_type: "image/png",
            path: "/tmp/diag.png",
            url: "/api/v1/attachments/diag.png",
          },
        ],
        created_at: "2026-10-27T16:10:05Z",
      },
    ];

    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-ferris-01"
        turns={sampleTurns}
        isBusy={true}
        queue={queuedItems}
        onSendPrompt={vi.fn()}
        onSendQueuedNow={onSendQueuedNow}
        onCancelQueued={onCancelQueued}
      />,
    );

    expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Busy · Working");
    const queueBar = screen.getByTestId("prompt-queue-bar");
    expect(queueBar).toBeInTheDocument();
    expect(screen.getByText("Queued Prompts (2)")).toBeInTheDocument();
    expect(screen.getByTestId("queued-prompt-pos-0")).toHaveTextContent("#1");
    expect(screen.getByTestId("queued-prompt-pos-1")).toHaveTextContent("#2");
    expect(screen.getByText("First queued prompt in line")).toBeInTheDocument();
    expect(screen.getByText("Second queued prompt with attachment")).toBeInTheDocument();

    fireEvent.click(screen.getByTestId("queued-send-now-qp-100-1"));
    await waitFor(() => {
      expect(onSendQueuedNow).toHaveBeenCalledWith("qp-100-1");
    });

    fireEvent.click(screen.getByTestId("queued-cancel-qp-100-2"));
    await waitFor(() => {
      expect(onCancelQueued).toHaveBeenCalledWith("qp-100-2");
    });
  });

  it("persists Always send now toggle in localStorage and passes send_now: true to onSendPrompt", async () => {
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

    const toggle = screen.getByTestId("always-send-now-toggle") as HTMLInputElement;
    expect(toggle.checked).toBe(false);

    fireEvent.click(toggle);
    expect(toggle.checked).toBe(true);
    expect(window.localStorage.getItem(ALWAYS_SEND_NOW_STORAGE_KEY)).toBe("true");

    const promptInput = screen.getByLabelText("Prompt Message") as HTMLTextAreaElement;
    fireEvent.change(promptInput, { target: { value: "Immediate override prompt" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Prompt/i }));

    await waitFor(() => {
      expect(onSendPrompt).toHaveBeenCalledWith("sess-ferris-01", {
        prompt: "Immediate override prompt",
        urgency: "normal",
        subject: undefined,
        send_now: true,
      });
    });
  });
});
