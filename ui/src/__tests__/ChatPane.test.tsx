import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import {
  ALWAYS_SEND_NOW_STORAGE_KEY,
  ChatPane,
  sessionDraftMemory,
  sessionScrollMemory,
} from "../components/ChatPane";
import type { QueuedOperatorPrompt, TranscriptTurn } from "../types/api";

describe("ChatPane", () => {
  beforeEach(() => {
    sessionScrollMemory.clear();
    sessionDraftMemory.clear();
    window.localStorage.removeItem(ALWAYS_SEND_NOW_STORAGE_KEY);
    for (const key of Object.keys(window.localStorage)) {
      if (key.startsWith("ferrodeck.draft.")) window.localStorage.removeItem(key);
    }
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
      within(screen.getByTestId("turn-card-0")).getByText(
        /Check the status of AI-Lab-0 services and inspect the worktree\./i,
      ),
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
    const toggleBtn = screen.getByRole("button", { name: /Thinking\.\.\./i });
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

  it("submits prompt via Send Prompt button and Ctrl+Enter, then clears input", async () => {
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

    const promptInput = screen.getByPlaceholderText(/Send a message/i) as HTMLTextAreaElement;
    const sendButton = screen.getByRole("button", { name: /Send Message/i });

    fireEvent.change(promptInput, { target: { value: "Run fl arch check now" } });

    fireEvent.click(sendButton);

    await waitFor(() => {
      expect(onSendPrompt).toHaveBeenCalledWith("sess-ferris-01", {
        prompt: "Run fl arch check now",
        urgency: "normal",
        subject: undefined,
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
        urgency: "normal",
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

  it("applies anti-autofill attributes, omits Optional subject/Urgency fields, and places textarea above the bottom action bar", () => {
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

    expect(screen.queryByPlaceholderText(/Optional subject/i)).toBeNull();
    expect(screen.queryByLabelText(/Prompt Subject/i)).toBeNull();
    expect(screen.queryByLabelText(/Urgency/i)).toBeNull();

    const filterInput = screen.getByLabelText("Filter transcript");
    expect(filterInput).toHaveAttribute("autocomplete", "off");
    expect(filterInput).toHaveAttribute("data-1p-ignore", "true");
    expect(filterInput).toHaveAttribute("data-lpignore", "true");
    expect(filterInput).toHaveAttribute("data-form-type", "other");

    const promptTextarea = screen.getByLabelText("Message");
    expect(promptTextarea).toHaveAttribute("autocomplete", "one-time-code");
    expect(promptTextarea).toHaveAttribute("role", "searchbox");
    expect(promptTextarea).toHaveAttribute("data-1p-ignore", "true");
    expect(promptTextarea).toHaveAttribute("data-lpignore", "true");
    expect(promptTextarea).toHaveAttribute("data-bwignore", "true");
    expect(promptTextarea).toHaveAttribute("data-form-type", "other");
    expect(promptTextarea).toHaveAttribute("aria-autocomplete", "none");

    const actionBar = screen.getByTestId("composer-action-bar");
    const stopButton = screen.getByTestId("interrupt-session-btn");
    const sendButton = screen.getByRole("button", { name: /Send Message/i });
    expect(actionBar).toContainElement(stopButton);
    expect(actionBar).toContainElement(sendButton);
    expect(
      promptTextarea.compareDocumentPosition(actionBar) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    expect(
      promptTextarea.compareDocumentPosition(sendButton) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    expect(
      stopButton.compareDocumentPosition(sendButton) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
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

    const sendButton = screen.getByRole("button", { name: /Send Message/i });
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

  it("encapsulates consecutive tool output turns into the preceding assistant TurnCard box with an orange arrow toggle", () => {
    const turnsWithToolOutput: TranscriptTurn[] = [
      {
        index: 912,
        role: "assistant",
        content: "",
        thinking: null,
        tool_calls: [
          {
            name: "read_file",
            arguments: { Path: "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/ui/src/App.tsx" },
          },
        ],
        created_at: "2026-11-01T15:52:10Z",
      },
      {
        index: 913,
        role: "tool",
        content:
          "Created At: 2026-11-01T15:52:10Z\nCompleted At: 2026-11-01T15:52:11Z\nShowing lines 1-125 of 1631\nimport React from 'react';",
        thinking: null,
        tool_calls: [],
        created_at: "2026-11-01T15:52:11Z",
      },
    ];

    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-ferris-01"
        turns={turnsWithToolOutput}
        onSendPrompt={vi.fn()}
      />,
    );

    // Only a single TurnCard (turn-card-912) is rendered; turn 913 is encapsulated inside it
    const assistantCard = screen.getByTestId("turn-card-912");
    expect(assistantCard).toBeInTheDocument();
    expect(screen.queryByTestId("turn-card-913")).toBeNull();

    // Both the tool call block and the attached tool output toggle live inside turn-card-912
    const toolCallBlock = screen.getByTestId("turn-tools-912");
    const attachedOutputsBlock = screen.getByTestId("turn-attached-tool-outputs-912");
    expect(assistantCard).toContainElement(toolCallBlock);
    expect(assistantCard).toContainElement(attachedOutputsBlock);

    const outputToggle = screen.getByTestId("turn-tool-output-toggle-913");
    expect(attachedOutputsBlock).toContainElement(outputToggle);
    expect(outputToggle).toHaveAttribute("aria-expanded", "false");
    expect(outputToggle).toHaveTextContent("Tool / Terminal Output (4 lines)");

    // Arrow uses orange styling
    const orangeArrow = screen.getByTestId("turn-tool-output-arrow-913");
    expect(orangeArrow.style.color).toContain("#F59E0B");

    // Clicking the toggle expands the tool output in place inside turn-card-912
    expect(screen.queryByText(/Showing lines 1-125 of 1631/)).toBeNull();
    fireEvent.click(outputToggle);
    expect(outputToggle).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText(/Showing lines 1-125 of 1631/)).toBeInTheDocument();
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
    expect(screen.getByText("Queued Messages (2)")).toBeInTheDocument();
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

    const promptInput = screen.getByLabelText("Message") as HTMLTextAreaElement;
    fireEvent.change(promptInput, { target: { value: "Immediate override prompt" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));

    await waitFor(() => {
      expect(onSendPrompt).toHaveBeenCalledWith("sess-ferris-01", {
        prompt: "Immediate override prompt",
        urgency: "normal",
        subject: undefined,
        send_now: true,
      });
    });
  });

  it("renders Antigravity-style rich Markdown headings, lists, bold, warm amber code pills, clickable URL code pills, and file-reference chips", () => {
    const richTurns: TranscriptTurn[] = [
      {
        index: 0,
        role: "assistant",
        content: [
          "### Summary of Changes",
          "",
          "- Updated [Home.jsx](file:///home/lmcfarlin/Projects/Artificial-Humanity/ArtificialHumanity.io/src/pages/Home.jsx) and [`TurnCard`](file:///home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/ui/src/components/TurnCard.tsx#L10-L20) with **rich formatting**.",
          "- Preview server running at `http://ai-lab-0:4173/` using `npm run build`.",
        ].join("\n"),
        thinking: null,
        tool_calls: [],
      },
    ];

    render(
      <ChatPane
        agentName="wesley"
        agentDisplayName="Wesley"
        adapter="antigravity"
        sessionId="sess-wesley-01"
        turns={richTurns}
        onSendPrompt={vi.fn()}
      />,
    );

    const turnContent = screen.getByTestId("turn-content-0");

    // 1. Heading
    const heading = turnContent.querySelector("h3");
    expect(heading).toBeTruthy();
    expect(heading).toHaveTextContent("Summary of Changes");

    // 2. Bulleted list
    const ul = turnContent.querySelector("ul");
    expect(ul).toBeTruthy();
    const listItems = turnContent.querySelectorAll("li");
    expect(listItems.length).toBe(2);

    // 3. Bold text
    const strong = turnContent.querySelector("strong");
    expect(strong).toBeTruthy();
    expect(strong).toHaveTextContent("rich formatting");

    // 4. Warm amber inline code pills & clickable URL code pill
    const codeSpan = screen.getByText("npm run build");
    expect(codeSpan.tagName.toLowerCase()).toBe("code");

    const urlCodeText = screen.getByText("http://ai-lab-0:4173/");
    const urlLink = urlCodeText.closest("a");
    expect(urlLink).toBeTruthy();
    expect(urlLink).toHaveAttribute("href", "http://ai-lab-0:4173/");
    expect(urlLink).toHaveAttribute("target", "_blank");

    // 5. Antigravity-style File-Reference Chips (with .jsx/.tsx ⚛ icon, stripped backticks, and no raw file:/// text)
    const chips = screen.getAllByTestId("file-reference-chip");
    expect(chips.length).toBe(2);
    expect(chips[0]).toHaveTextContent("⚛");
    expect(chips[0]).toHaveTextContent("Home.jsx");
    expect(chips[0]).toHaveAttribute(
      "title",
      "file:///home/lmcfarlin/Projects/Artificial-Humanity/ArtificialHumanity.io/src/pages/Home.jsx",
    );
    expect(chips[1]).toHaveTextContent("⚛");
    expect(chips[1]).toHaveTextContent("TurnCard");
    expect(chips[1].textContent).not.toContain("`");
    expect(turnContent.textContent).not.toContain("file:///");
  });

  it("refuses non-http(s) URL schemes such as javascript: and data: in Markdown links", () => {
    const unsafeTurns: TranscriptTurn[] = [
      {
        index: 0,
        role: "assistant",
        content: "Links: [bad-js](javascript:alert(1)) and [bad-data](data:text/html,<script>alert(1)</script>) and [good-https](https://artificialhumanity.io)",
        thinking: null,
        tool_calls: [],
      },
    ];

    render(
      <ChatPane
        agentName="wesley"
        agentDisplayName="Wesley"
        adapter="antigravity"
        sessionId="sess-wesley-01"
        turns={unsafeTurns}
        onSendPrompt={vi.fn()}
      />,
    );

    const turnContent = screen.getByTestId("turn-content-0");
    const anchors = Array.from(turnContent.querySelectorAll("a"));
    expect(anchors.length).toBe(1);
    expect(anchors[0]).toHaveAttribute("href", "https://artificialhumanity.io");
    expect(screen.getByText("bad-js").closest("a")).toBeNull();
    expect(screen.getByText("bad-data").closest("a")).toBeNull();
  });

  it("windows transcripts with more than 100 turns and expands older turns on demand", () => {
    const manyTurns: TranscriptTurn[] = Array.from({ length: 125 }, (_, i) => ({
      index: i,
      role: i % 2 === 0 ? "user" : "assistant",
      content: `Turn number ${i}`,
      thinking: null,
      tool_calls: [],
    }));

    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-ferris-01"
        turns={manyTurns}
        onSendPrompt={vi.fn()}
      />,
    );

    // First 25 turns are hidden initially; latest 100 (25..124) are rendered
    expect(screen.queryByTestId("turn-card-0")).toBeNull();
    expect(screen.getByTestId("turn-card-25")).toBeInTheDocument();
    expect(screen.getByTestId("turn-card-124")).toBeInTheDocument();

    const loadEarlierBtn = screen.getByTestId("load-earlier-turns-btn");
    expect(loadEarlierBtn).toHaveTextContent("Show earlier turns (25 older)");

    fireEvent.click(loadEarlierBtn);
    expect(screen.getByTestId("turn-card-0")).toBeInTheDocument();
    expect(screen.queryByTestId("load-earlier-turns-btn")).toBeNull();
  });

  it("allows editing a queued message in-place before it leaves the queue", async () => {
    const onUpdateQueued = vi.fn().mockResolvedValue(undefined);
    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-ferris-01"
        turns={sampleTurns}
        isBusy={true}
        queue={[
          {
            id: "qp-edit-1",
            session_id: "sess-ferris-01",
            persona: "ferris",
            prompt: "Draft message in queue",
            effective_body: "Draft message in queue",
            by: "owner",
            attachments: [],
            created_at: "2026-10-28T16:50:00Z",
          },
        ]}
        onUpdateQueued={onUpdateQueued}
      />,
    );

    const editBtn = screen.getByTestId("queued-edit-qp-edit-1");
    fireEvent.click(editBtn);

    const editInput = screen.getByTestId("queued-edit-input-qp-edit-1") as HTMLInputElement;
    expect(editInput.value).toBe("Draft message in queue");

    fireEvent.change(editInput, { target: { value: "Updated message in queue" } });
    fireEvent.click(screen.getByTestId("queued-save-qp-edit-1"));

    await waitFor(() => {
      expect(onUpdateQueued).toHaveBeenCalledWith("qp-edit-1", "Updated message in queue");
    });
  });
});

describe("ChatPane draft persistence", () => {
  it("restores an in-progress draft after unmount and remount", () => {
    const props = {
      agentName: "ferris",
      agentDisplayName: "Ferris",
      adapter: "claude",
      sessionId: "sess-draft-01",
      turns: [] as TranscriptTurn[],
      onSendPrompt: vi.fn(),
    };

    const first = render(<ChatPane {...props} />);
    const textarea = screen.getByLabelText("Message") as HTMLTextAreaElement;
    fireEvent.change(textarea, {
      target: { value: "Half-written prompt to ferris" },
    });
    expect(window.localStorage.getItem("ferrodeck.draft.sess-draft-01")).toBe(
      "Half-written prompt to ferris",
    );

    // Switching tabs unmounts ChatPane in App.
    first.unmount();

    render(<ChatPane {...props} />);
    expect(
      (screen.getByLabelText("Message") as HTMLTextAreaElement).value,
    ).toBe("Half-written prompt to ferris");
  });

  it("never writes the old draft under the new target key on session switch", () => {
    window.localStorage.setItem("ferrodeck.draft.sess-switch-b", "Draft for B");
    const props = {
      agentName: "ferris",
      agentDisplayName: "Ferris",
      adapter: "claude",
      sessionId: "sess-switch-a",
      turns: [] as TranscriptTurn[],
      onSendPrompt: vi.fn(),
    };

    const { rerender } = render(<ChatPane {...props} />);
    fireEvent.change(screen.getByLabelText("Message"), {
      target: { value: "Draft for A" },
    });
    expect(window.localStorage.getItem("ferrodeck.draft.sess-switch-a")).toBe(
      "Draft for A",
    );

    const setItemSpy = vi.spyOn(Storage.prototype, "setItem");
    try {
      rerender(<ChatPane {...props} sessionId="sess-switch-b" />);
      const bWrites = setItemSpy.mock.calls.filter(
        ([key]) => key === "ferrodeck.draft.sess-switch-b",
      );
      expect(bWrites.every(([, value]) => value !== "Draft for A")).toBe(true);
    } finally {
      setItemSpy.mockRestore();
    }

    expect(
      (screen.getByLabelText("Message") as HTMLTextAreaElement).value,
    ).toBe("Draft for B");
    expect(window.localStorage.getItem("ferrodeck.draft.sess-switch-b")).toBe(
      "Draft for B",
    );
  });
});

describe("ChatPane vendor account quota allowance badge in AgentSettingsModal", () => {
  it("keeps top toolbar compact without quota-usage-badge and renders Codex plus tier with 5h session and 7d weekly allowance windows in AgentSettingsModal", () => {
    const now = Date.now();
    const { rerender } = render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="codex"
        sessionId="sess-codex-01"
        turns={[]}
        quotaUsage={{
          tier: "plus",
          primary_window: {
            used_percent: 2,
            window_minutes: 300,
            resets_at_ms: now + (3 * 60 + 14) * 60_000,
          },
          secondary_window: {
            used_percent: 1,
            window_minutes: 10_080,
            resets_at_ms: now + (6 * 24 * 60 + 22 * 60) * 60_000,
          },
        }}
        onSendPrompt={vi.fn()}
      />,
    );

    expect(screen.queryByTestId("quota-usage-badge")).toBeNull();
    expect(screen.queryByTestId("quota-exhausted-toolbar-pill")).toBeNull();

    rerender(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="codex"
        sessionId="sess-codex-01"
        turns={[]}
        agentSettingsOpen={true}
        quotaUsage={{
          tier: "plus",
          primary_window: {
            used_percent: 2,
            window_minutes: 300,
            resets_at_ms: now + (3 * 60 + 14) * 60_000,
          },
          secondary_window: {
            used_percent: 1,
            window_minutes: 10_080,
            resets_at_ms: now + (6 * 24 * 60 + 22 * 60) * 60_000,
          },
        }}
        onSendPrompt={vi.fn()}
      />,
    );

    const badge = screen.getByTestId("quota-usage-badge");
    expect(badge).not.toHaveAttribute("data-exhausted", "true");
    expect(screen.getByTestId("quota-tier-chip")).toHaveTextContent("PLUS");
    expect(screen.getByTestId("quota-primary-window")).toHaveTextContent("5h: 98% left");
    expect(screen.getByTestId("quota-primary-window")).toHaveTextContent("renews in 3h 14m");
    expect(screen.getByTestId("quota-secondary-window")).toHaveTextContent("7d: 99% left");
    expect(screen.getByTestId("quota-secondary-window")).toHaveTextContent("renews in 6d 22h");
  });

  it("renders Grok SuperGrok tier with weekly allowance window in AgentSettingsModal", () => {
    const now = Date.now();
    const { rerender } = render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="grok"
        sessionId="sess-grok-01"
        turns={[]}
        quotaUsage={{
          tier: "SuperGrok",
          secondary_window: {
            used_percent: 15,
            window_minutes: 10_080,
            resets_at_ms: now + (4 * 24 * 60 + 5 * 60) * 60_000,
          },
        }}
        onSendPrompt={vi.fn()}
      />,
    );

    expect(screen.queryByTestId("quota-usage-badge")).toBeNull();
    rerender(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="grok"
        sessionId="sess-grok-01"
        turns={[]}
        agentSettingsOpen={true}
        quotaUsage={{
          tier: "SuperGrok",
          secondary_window: {
            used_percent: 15,
            window_minutes: 10_080,
            resets_at_ms: now + (4 * 24 * 60 + 5 * 60) * 60_000,
          },
        }}
        onSendPrompt={vi.fn()}
      />,
    );

    const badge = screen.getByTestId("quota-usage-badge");
    expect(badge).not.toHaveAttribute("data-exhausted", "true");
    expect(screen.getByTestId("quota-tier-chip")).toHaveTextContent("SUPERGROK");
    expect(screen.queryByTestId("quota-primary-window")).toBeNull();
    expect(screen.getByTestId("quota-secondary-window")).toHaveTextContent("7d: 85% left");
    expect(screen.getByTestId("quota-secondary-window")).toHaveTextContent("renews in 4d 5h");
  });

  it("shows compact quota-exhausted-toolbar-pill in toolbar when exhausted and highlights full Muse 429 quota details in AgentSettingsModal", () => {
    const now = Date.now();
    render(
      <ChatPane
        agentName="felicia"
        agentDisplayName="Felicia"
        adapter="muse"
        sessionId="sess-muse-01"
        turns={[]}
        quotaUsage={{
          tier: "muse",
          primary_window: {
            used_percent: 100,
            window_minutes: 300,
            resets_at_ms: now + 42 * 60_000,
          },
          exhausted_reason: "HTTP 429: quota exhausted, resets at 03:15 UTC",
        }}
        onSendPrompt={vi.fn()}
      />,
    );

    const toolbarPill = screen.getByTestId("quota-exhausted-toolbar-pill");
    expect(toolbarPill).toHaveTextContent("HTTP 429: quota exhausted, resets at 03:15 UTC");
    expect(screen.queryByTestId("quota-usage-badge")).toBeNull();

    fireEvent.click(toolbarPill);

    const badge = screen.getByTestId("quota-usage-badge");
    expect(badge).toHaveAttribute("data-exhausted", "true");
    expect(badge).toHaveClass("is-exhausted");
    expect(badge).toHaveAttribute(
      "title",
      "HTTP 429: quota exhausted, resets at 03:15 UTC",
    );
    expect(screen.getByTestId("quota-primary-window")).toHaveTextContent("5h: 0% left");
    expect(screen.getByTestId("quota-primary-window")).toHaveTextContent("renews in 42m");
    expect(screen.getByTestId("quota-exhausted-alert")).toHaveTextContent(
      "HTTP 429: quota exhausted, resets at 03:15 UTC",
    );
  });
});

describe("ChatPane Clear | Reforge modal and ephemeral ? help", () => {
  it("opens the Cancel | Clear | Reforge modal and auto-hides the ? help popover after 5 seconds", () => {
    vi.useFakeTimers();
    try {
      render(
        <ChatPane
          agentName="ferris"
          agentDisplayName="Ferris"
          adapter="claude"
          sessionId="sess-ferris-01"
          turns={[]}
          agentSettingsOpen={true}
          onSendPrompt={vi.fn()}
        />,
      );

      const clearBtn = screen.getByTestId("open-clear-reforge-modal-btn");
      expect(clearBtn).not.toBeDisabled();
      fireEvent.click(clearBtn);

      const modal = screen.getByTestId("clear-session-modal");
      expect(modal).toHaveAttribute("role", "dialog");
      expect(modal).toHaveAttribute("aria-modal", "true");
      expect(screen.getByTestId("clear-modal-cancel-btn")).toHaveTextContent("Cancel");
      expect(screen.getByTestId("clear-modal-clear-btn")).toHaveTextContent("Clear");
      expect(screen.getByTestId("clear-modal-reforge-btn")).toHaveTextContent("Reforge");

      expect(screen.queryByTestId("clear-modal-help-popover")).toBeNull();
      fireEvent.click(screen.getByTestId("clear-modal-help-btn"));

      const popover = screen.getByTestId("clear-modal-help-popover");
      expect(popover).toHaveTextContent(/Clear:/);
      expect(popover).toHaveTextContent(/Reforge:/);
      expect(popover).toHaveTextContent(/registered identity, project, and role prompt/i);

      act(() => {
        vi.advanceTimersByTime(5_000);
      });
      expect(screen.queryByTestId("clear-modal-help-popover")).toBeNull();

      fireEvent.click(screen.getByTestId("clear-modal-cancel-btn"));
      expect(screen.queryByTestId("clear-session-modal")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("calls POST /api/v1/sessions/:id/clear with mode 'clear' and 'reforge'", async () => {
    const clearCalls: Array<{ url: string; body: unknown }> = [];
    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === "/api/v1/sessions/sess-ferris-01/clear" && init?.method === "POST") {
        const body = JSON.parse(String(init.body));
        clearCalls.push({ url, body });
        return new Response(
          JSON.stringify({
            ok: true,
            session_id: "sess-ferris-01",
            persona: "ferris",
            mode: body.mode,
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      return new Response(JSON.stringify({}), { status: 200 });
    });
    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });

    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="claude"
        sessionId="sess-ferris-01"
        turns={[]}
        agentSettingsOpen={true}
        apiClient={apiClient}
      />,
    );

    // 1. Clear mode
    fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
    fireEvent.click(screen.getByTestId("clear-modal-clear-btn"));

    await waitFor(() => {
      expect(clearCalls).toEqual([
        { url: "/api/v1/sessions/sess-ferris-01/clear", body: { mode: "clear" } },
      ]);
    });
    expect(screen.queryByTestId("clear-session-modal")).toBeNull();
    expect(await screen.findByTestId("compact-feedback")).toHaveTextContent("Cleared context");

    // 2. Reforge mode
    fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
    fireEvent.click(screen.getByTestId("clear-modal-reforge-btn"));

    await waitFor(() => {
      expect(clearCalls).toEqual([
        { url: "/api/v1/sessions/sess-ferris-01/clear", body: { mode: "clear" } },
        { url: "/api/v1/sessions/sess-ferris-01/clear", body: { mode: "reforge" } },
      ]);
    });
    expect(screen.queryByTestId("clear-session-modal")).toBeNull();
    expect(await screen.findByTestId("compact-feedback")).toHaveTextContent("Reforged session");
  });
});

describe("ChatPane Decommission modal and 6-second ephemeral ? help", () => {
  it("opens the Cancel | Tear Down | Prepare & Decommission modal and auto-hides the ? help popover after 6 seconds", () => {
    vi.useFakeTimers();
    try {
      render(
        <ChatPane
          agentName="kacey"
          agentDisplayName="Kacey"
          adapter="claude"
          sessionId="sess-kacey-01"
          turns={[]}
          agentSettingsOpen={true}
          onSendPrompt={vi.fn()}
        />,
      );

      const decommissionBtn = screen.getByTestId("open-decommission-modal-btn");
      expect(decommissionBtn).not.toBeDisabled();
      fireEvent.click(decommissionBtn);

      const modal = screen.getByTestId("decommission-modal");
      expect(modal).toHaveAttribute("role", "dialog");
      expect(modal).toHaveAttribute("aria-modal", "true");
      expect(screen.getByTestId("decommission-cancel-btn")).toHaveTextContent("Cancel");
      expect(screen.getByTestId("decommission-teardown-btn")).toHaveTextContent("Tear Down");
      expect(screen.getByTestId("decommission-prepare-btn")).toHaveTextContent(
        "Prepare & Decommission",
      );

      expect(screen.queryByTestId("decommission-help-popover")).toBeNull();
      fireEvent.click(screen.getByTestId("decommission-help-btn"));

      const popover = screen.getByTestId("decommission-help-popover");
      expect(popover).toHaveTextContent(/Prepare & Decommission:/);
      expect(popover).toHaveTextContent(/STATE\.md\/FerroState/);
      expect(popover).toHaveTextContent(/Tear Down:/);
      expect(popover).toHaveTextContent(/unregisters the persona/i);

      act(() => {
        vi.advanceTimersByTime(5_000);
      });
      expect(screen.getByTestId("decommission-help-popover")).toBeInTheDocument();

      act(() => {
        vi.advanceTimersByTime(1_000);
      });
      expect(screen.queryByTestId("decommission-help-popover")).toBeNull();

      fireEvent.click(screen.getByTestId("decommission-cancel-btn"));
      expect(screen.queryByTestId("decommission-modal")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("calls onDecommissionSession and POST /api/v1/sessions/:id/decommission with 'prepare' and 'teardown'", async () => {
    const onDecommissionSession = vi.fn(async () => undefined);
    const { unmount } = render(
      <ChatPane
        agentName="kacey"
        agentDisplayName="Kacey"
        adapter="claude"
        sessionId="sess-kacey-01"
        turns={[]}
        agentSettingsOpen={true}
        onDecommissionSession={onDecommissionSession}
      />,
    );

    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    fireEvent.click(screen.getByTestId("decommission-prepare-btn"));

    await waitFor(() => {
      expect(onDecommissionSession).toHaveBeenCalledWith("prepare");
    });
    expect(screen.queryByTestId("decommission-modal")).toBeNull();
    expect(await screen.findByTestId("compact-feedback")).toHaveTextContent(
      "Prepared decommission",
    );

    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));

    await waitFor(() => {
      expect(onDecommissionSession).toHaveBeenCalledWith("teardown");
    });
    expect(await screen.findByTestId("compact-feedback")).toHaveTextContent(
      "Tore down session",
    );
    unmount();

    const decommCalls: Array<{ url: string; body: unknown }> = [];
    const transcriptCalls: string[] = [];
    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === "/api/v1/sessions/sess-kacey-01/decommission" && init?.method === "POST") {
        const body = JSON.parse(String(init.body));
        decommCalls.push({ url, body });
        return new Response(
          JSON.stringify({
            session_id: "sess-kacey-01",
            persona: "kacey",
            mode: body.mode,
            status: body.mode === "prepare" ? "decommissioning" : "decommissioned",
            handshake_dispatched: body.mode === "prepare",
            unregistered: body.mode === "teardown",
            runtime_stopped: body.mode === "teardown",
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      if (url.includes("/api/v1/sessions/sess-kacey-01/transcript")) {
        transcriptCalls.push(url);
        return new Response(
          JSON.stringify({
            turns: [
              {
                index: 0,
                role: "assistant",
                content: "Standalone transcript turn",
                thinking: null,
                tool_calls: [],
              },
            ],
            is_busy: true,
            queue: [
              {
                id: "qp-1",
                session_id: "sess-kacey-01",
                persona: "kacey",
                prompt: "Queued item",
                effective_body: "Queued item",
                by: "owner",
                attachments: [],
                created_at: "2026-10-30T15:00:00Z",
              },
            ],
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      return new Response(JSON.stringify({}), { status: 200 });
    });
    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });

    render(
      <ChatPane
        agentName="kacey"
        agentDisplayName="Kacey"
        adapter="claude"
        sessionId="sess-kacey-01"
        agentSettingsOpen={true}
        apiClient={apiClient}
      />,
    );

    expect(await screen.findByText("Standalone transcript turn")).toBeInTheDocument();

    transcriptCalls.length = 0;

    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    fireEvent.click(screen.getByTestId("decommission-prepare-btn"));

    await waitFor(() => {
      expect(decommCalls).toEqual([
        { url: "/api/v1/sessions/sess-kacey-01/decommission", body: { mode: "prepare" } },
      ]);
    });
    expect(transcriptCalls.length).toBeGreaterThan(0);

    transcriptCalls.length = 0;

    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));

    await waitFor(() => {
      expect(decommCalls).toEqual([
        { url: "/api/v1/sessions/sess-kacey-01/decommission", body: { mode: "prepare" } },
        { url: "/api/v1/sessions/sess-kacey-01/decommission", body: { mode: "teardown" } },
      ]);
    });
    await waitFor(() => {
      expect(screen.queryByText("Standalone transcript turn")).toBeNull();
    });
    expect(transcriptCalls).toEqual([]);
  });
});

describe("Resizable chat composer (Record 57)", () => {
  it("renders composer-resize-handle and resizes the textarea height within 72px..420px on mouse drag", () => {
    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        sessionId="sess-ferris-01"
        turns={[]}
      />,
    );

    const handle = screen.getByTestId("composer-resize-handle");
    const textarea = screen.getByLabelText("Message") as HTMLTextAreaElement;

    expect(handle).toBeInTheDocument();
    expect(textarea.style.height).toBe("80px");

    // Drag upward by 60px (clientY: 400 -> 340 => 80 + 60 = 140px)
    fireEvent.mouseDown(handle, { clientY: 400 });
    fireEvent.mouseMove(window, { clientY: 340 });
    fireEvent.mouseUp(window);
    expect(textarea.style.height).toBe("140px");

    // Drag upward beyond max (clientY: 400 -> 0 => clamped at 420px)
    fireEvent.mouseDown(handle, { clientY: 400 });
    fireEvent.mouseMove(window, { clientY: 0 });
    fireEvent.mouseUp(window);
    expect(textarea.style.height).toBe("420px");

    // Drag downward beyond min (clientY: 100 -> 600 => clamped at 72px)
    fireEvent.mouseDown(handle, { clientY: 100 });
    fireEvent.mouseMove(window, { clientY: 600 });
    fireEvent.mouseUp(window);
    expect(textarea.style.height).toBe("72px");
  });

  it("resets selectedModel to opts[0] when switching from a Claude agent to an Antigravity agent with model={null}", () => {
    const { rerender } = render(
      <ChatPane
        agentName="felicity"
        agentDisplayName="Felicity"
        adapter="claude"
        model={null}
        turns={[]}
        agentSettingsOpen={true}
      />,
    );

    expect(screen.getByTestId("chat-active-model-badge")).toHaveTextContent(
      "claude · claude-opus-5-5 (high)",
    );

    rerender(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="antigravity"
        model={null}
        turns={[]}
        agentSettingsOpen={true}
      />,
    );

    expect(screen.getByTestId("chat-active-model-badge")).toHaveTextContent(
      "antigravity · gemini-4-argon (high)",
    );

    const select = screen.getByTestId("session-model-select") as HTMLSelectElement;
    expect(select.value).toBe("gemini-4-argon");
  });

  it("renders docked since-last-prompt-bar with activity summary, expands step details, and scrolls to prompt turn on jump", () => {
    const activityTurns: TranscriptTurn[] = [
      {
        index: 10,
        role: "user",
        content: "Update SettingsModal.tsx with the 3-tab layout and run UI tests.",
        thinking: null,
        tool_calls: [],
        created_at: "2026-11-01T16:10:00Z",
      },
      {
        index: 11,
        role: "assistant",
        content: "Editing SettingsModal.tsx and running Vitest now.",
        thinking: null,
        tool_calls: [
          {
            name: "edit_file",
            arguments: {
              Path: "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/ui/src/components/SettingsModal.tsx",
            },
          },
          {
            name: "run_command",
            arguments: {
              CommandLine: "npm --prefix ui test",
            },
          },
        ],
        created_at: "2026-11-01T16:10:05Z",
      },
      {
        index: 12,
        role: "wire:ferris",
        content: "[WIRE from ferris · normal] Looks good, proceed.",
        thinking: null,
        tool_calls: [],
        created_at: "2026-11-01T16:10:12Z",
      },
    ];

    render(
      <ChatPane
        agentName="felix"
        agentDisplayName="Felix"
        adapter="antigravity"
        sessionId="sess-felix-01"
        turns={activityTurns}
        onSendPrompt={vi.fn()}
      />,
    );

    const bar = screen.getByTestId("since-last-prompt-bar");
    expect(bar).toBeInTheDocument();
    expect(bar).not.toHaveTextContent(/Your last message/i);
    expect(bar).not.toHaveTextContent(/Jump to prompt/i);
    const promptTextEl = screen.getByTestId("since-last-prompt-text");
    expect(promptTextEl).toHaveTextContent(
      "Update SettingsModal.tsx with the 3-tab layout and run UI tests.",
    );

    const counts = screen.getByTestId("since-last-prompt-counts");
    const repliesBtn = screen.getByTestId("since-last-prompt-replies-btn");
    const toolsBtn = screen.getByTestId("since-last-prompt-tools-btn");
    const wireBtn = screen.getByTestId("since-last-prompt-wire-btn");
    expect(repliesBtn).toHaveTextContent("1 reply");
    expect(toolsBtn).toHaveTextContent("2 tool calls");
    expect(toolsBtn).not.toHaveTextContent(/run_command/i);
    expect(counts).toHaveTextContent("1 file touched");
    expect(wireBtn).toHaveTextContent("1 wire");

    // Collapsed by default
    const toggle = screen.getByTestId("since-last-prompt-toggle");
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByTestId("since-last-prompt-details")).toBeNull();

    // Clicking "1 reply" opens details filtered ONLY to replies
    fireEvent.click(repliesBtn);
    expect(screen.getByTestId("since-last-prompt-details")).toBeInTheDocument();
    expect(screen.getByTestId("since-last-prompt-step-reply-11")).toBeInTheDocument();
    expect(screen.queryByTestId("since-last-prompt-step-tool-11-1")).toBeNull();
    expect(screen.queryByTestId("since-last-prompt-step-wire-12")).toBeNull();

    // Clicking "1 reply" again collapses details
    fireEvent.click(repliesBtn);
    expect(screen.queryByTestId("since-last-prompt-details")).toBeNull();

    // Clicking "2 tool calls" opens details filtered ONLY to tool/file steps
    fireEvent.click(toolsBtn);
    expect(screen.getByTestId("since-last-prompt-details")).toBeInTheDocument();
    expect(screen.getByTestId("since-last-prompt-step-file-11-0")).toBeInTheDocument();
    expect(screen.getByTestId("since-last-prompt-step-tool-11-1")).toBeInTheDocument();
    expect(screen.queryByTestId("since-last-prompt-step-reply-11")).toBeNull();
    expect(
      screen.getByTestId("since-last-prompt-file-SettingsModal.tsx"),
    ).toBeInTheDocument();

    // Clicking "1 wire" switches details filter ONLY to wire items
    fireEvent.click(wireBtn);
    expect(screen.getByTestId("since-last-prompt-step-wire-12")).toBeInTheDocument();
    expect(screen.queryByTestId("since-last-prompt-step-tool-11-1")).toBeNull();

    // Clicking the amber prompt text (since-last-prompt-text / since-last-prompt-jump-btn) scrolls turn-card-10 into view
    const promptCard = screen.getByTestId("turn-card-10");
    const scrollIntoViewMock = vi.fn();
    promptCard.scrollIntoView = scrollIntoViewMock;

    fireEvent.click(screen.getByTestId("since-last-prompt-text"));
    expect(scrollIntoViewMock).toHaveBeenCalledWith({
      behavior: "smooth",
      block: "center",
    });
  });
});
