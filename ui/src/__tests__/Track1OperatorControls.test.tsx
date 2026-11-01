import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "../App";
import { HubApiClient } from "../api/client";
import { HubWsClient } from "../api/ws";
import { ChatPane, getVendorModelOptions } from "../components/ChatPane";
import { SettingsModal } from "../components/SettingsModal";

describe("Track 1 FerroDeck+ Operator Controls UI (Records 47, 48, 44, 30)", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  describe("Record 47 — Session Interrupt / Stop Execution Control with Accidental-Click Guard", () => {
    it("arms for 3 seconds on first click without firing, cancels or auto-disarms after 3s, and fires only on Confirm Stop", async () => {
      vi.useFakeTimers();
      const onInterruptSession = vi.fn().mockResolvedValue({
        session_id: "sess-ferris-01",
        interrupted: true,
        vendor: "claude",
        method: "sigint",
      });

      render(
        <ChatPane
          agentName="ferris"
          agentDisplayName="Ferris"
          adapter="claude"
          sessionId="sess-ferris-01"
          turns={[]}
          isBusy={true}
          onInterruptSession={onInterruptSession}
        />,
      );

      // 1. Stop button is placed inside composer-action-bar immediately to the left of Send Message
      const actionBar = screen.getByTestId("composer-action-bar");
      const stopBtn = screen.getByTestId("interrupt-session-btn");
      const sendBtn = screen.getByTestId("send-prompt-btn");
      expect(actionBar).toContainElement(stopBtn);
      expect(actionBar).toContainElement(sendBtn);
      expect(
        stopBtn.compareDocumentPosition(sendBtn) & Node.DOCUMENT_POSITION_FOLLOWING,
      ).toBeTruthy();

      // Clicking Stop once enters armed state without calling onInterruptSession
      fireEvent.click(stopBtn);
      expect(onInterruptSession).not.toHaveBeenCalled();
      expect(screen.getByTestId("interrupt-confirm-btn")).toBeInTheDocument();
      expect(screen.getByTestId("interrupt-cancel-btn")).toBeInTheDocument();
      expect(actionBar).toContainElement(screen.getByTestId("interrupt-confirm-btn"));

      // 2. Clicking Cancel disarms without firing
      fireEvent.click(screen.getByTestId("interrupt-cancel-btn"));
      expect(screen.queryByTestId("interrupt-confirm-btn")).toBeNull();
      expect(screen.getByTestId("interrupt-session-btn")).toBeInTheDocument();
      expect(onInterruptSession).not.toHaveBeenCalled();

      // 3. Re-arm and let 3 seconds elapse -> auto-disarms without firing
      fireEvent.click(screen.getByTestId("interrupt-session-btn"));
      expect(screen.getByTestId("interrupt-confirm-btn")).toBeInTheDocument();
      act(() => {
        vi.advanceTimersByTime(3_000);
      });
      expect(screen.queryByTestId("interrupt-confirm-btn")).toBeNull();
      expect(onInterruptSession).not.toHaveBeenCalled();

      // 4. Re-arm and click Confirm Stop -> invokes onInterruptSession and resets armed state
      fireEvent.click(screen.getByTestId("interrupt-session-btn"));
      await act(async () => {
        fireEvent.click(screen.getByTestId("interrupt-confirm-btn"));
      });
      expect(onInterruptSession).toHaveBeenCalledTimes(1);
      expect(screen.queryByTestId("interrupt-confirm-btn")).toBeNull();
      expect(screen.getByTestId("compact-feedback")).toHaveTextContent("Stopped session");
    });
  });

  describe("Records 48 & 57 — Per-Agent Model & Thinking Level Selector in AgentSettingsModal", () => {
    it("offers accurate ai-lab-0 vendor model options, respects availableModels, and preserves custom session.model", () => {
      expect(getVendorModelOptions("claude")).toEqual([
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-4-5",
      ]);
      expect(getVendorModelOptions("antigravity")).toEqual([
        "gemini-4-argon",
        "gemini-3.8-flash",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "gpt-oss-120b",
      ]);
      expect(getVendorModelOptions("agy")).toEqual([
        "gemini-4-argon",
        "gemini-3.8-flash",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "gpt-oss-120b",
      ]);
      expect(getVendorModelOptions("codex")).toEqual([
        "gpt-6.1-terra",
        "gpt-6.1-sol",
        "gpt-6-astra",
        "gpt-6-sol",
        "gpt-6-luna",
      ]);
      expect(getVendorModelOptions("grok")).toEqual([
        "grok-4.7",
        "grok-4.7-build-fast",
        "grok-4.6",
        "grok-4.5",
      ]);
      expect(getVendorModelOptions("muse")).toEqual(["muse-spark"]);
      expect(getVendorModelOptions("claude", "claude-custom-preview")).toEqual([
        "claude-custom-preview",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-4-5",
      ]);
      expect(
        getVendorModelOptions("codex", "gpt-6.1-terra", [
          "gpt-6.1-terra",
          "gpt-6.1-sol",
        ]),
      ).toEqual(["gpt-6.1-terra", "gpt-6.1-sol"]);
    });

    it("renders session-model-select and session-thinking-select inside AgentSettingsModal and calls onSetSessionModel on change", async () => {
      const onSetSessionModel = vi.fn().mockResolvedValue({
        session_id: "sess-felix-01",
        vendor: "antigravity",
        model: "gemini-3.8-flash",
        thinking_level: "xhigh",
        switched: true,
        method: "slash_model",
      });

      const { rerender } = render(
        <ChatPane
          agentName="felix"
          agentDisplayName="Felix"
          adapter="antigravity"
          model="gemini-4-argon"
          thinkingLevel="high"
          sessionId="sess-felix-01"
          turns={[]}
          onSetSessionModel={onSetSessionModel}
        />,
      );

      // Top ChatPane bar stays compact without verbose model/turn/context badges
      expect(screen.queryByTestId("chat-active-model-badge")).toBeNull();
      expect(screen.queryByTestId("turn-count-badge")).toBeNull();

      // Open AgentSettingsModal
      rerender(
        <ChatPane
          agentName="felix"
          agentDisplayName="Felix"
          adapter="antigravity"
          model="gemini-4-argon"
          thinkingLevel="high"
          sessionId="sess-felix-01"
          turns={[]}
          agentSettingsOpen={true}
          onSetSessionModel={onSetSessionModel}
        />,
      );
      expect(screen.getByTestId("agent-settings-modal")).toBeInTheDocument();
      expect(screen.getByTestId("chat-active-model-badge")).toHaveTextContent(
        "antigravity · gemini-4-argon (high)",
      );
      expect(screen.getByTestId("turn-count-badge")).toHaveTextContent("0 turns");

      const select = screen.getByTestId("session-model-select") as HTMLSelectElement;
      const thinkingSelect = screen.getByTestId(
        "session-thinking-select",
      ) as HTMLSelectElement;
      expect(select.value).toBe("gemini-4-argon");
      expect(thinkingSelect.value).toBe("high");

      fireEvent.change(select, { target: { value: "gemini-3.8-flash" } });

      await waitFor(() => {
        expect(onSetSessionModel).toHaveBeenCalledWith("gemini-3.8-flash", "high");
        expect(screen.getByTestId("compact-feedback")).toHaveTextContent(
          "Model: gemini-3.8-flash (high)",
        );
      });

      fireEvent.change(thinkingSelect, { target: { value: "low" } });

      await waitFor(() => {
        expect(onSetSessionModel).toHaveBeenCalledWith("gemini-3.8-flash", "low");
        expect(screen.getByTestId("compact-feedback")).toHaveTextContent(
          "Model: gemini-3.8-flash (low)",
        );
      });
    });
  });

  describe("Records 4 & 44 — Unified Settings Modal with Isolated Bulk Session Operations", () => {
    it("requires two-step confirmation before invoking bulkSessionAction in SettingsModal", async () => {
      const onBulkSessionAction = vi.fn().mockResolvedValue({
        action: "reforge",
        total: 4,
        succeeded: 4,
        results: [],
      });
      const onClose = vi.fn();

      render(
        <SettingsModal
          isOpen={true}
          onClose={onClose}
          onBulkSessionAction={onBulkSessionAction}
        />,
      );

      expect(screen.getByTestId("settings-modal")).toBeInTheDocument();

      // Clicking Compact All arms confirmation without firing immediately
      fireEvent.click(screen.getByTestId("bulk-compact-btn"));
      expect(onBulkSessionAction).not.toHaveBeenCalled();
      expect(screen.getByTestId("bulk-confirm-btn")).toBeInTheDocument();
      expect(screen.getByTestId("bulk-cancel-btn")).toBeInTheDocument();

      // Clicking Cancel aborts confirmation
      fireEvent.click(screen.getByTestId("bulk-cancel-btn"));
      expect(screen.queryByTestId("bulk-confirm-btn")).toBeNull();
      expect(onBulkSessionAction).not.toHaveBeenCalled();

      // Clicking Reforge All then Confirm invokes onBulkSessionAction("reforge")
      fireEvent.click(screen.getByTestId("bulk-reforge-btn"));
      fireEvent.click(screen.getByTestId("bulk-confirm-btn"));

      await waitFor(() => {
        expect(onBulkSessionAction).toHaveBeenCalledWith("reforge");
        expect(screen.getByTestId("bulk-action-result")).toHaveTextContent(
          "Completed Reforge All: 4 / 4 sessions succeeded.",
        );
      });
    });

    it("opens SettingsModal from ContextHeader settings-modal-btn in App", async () => {
      const mockFetch = vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.endsWith("/api/v1/auth/session")) {
          return new Response(
            JSON.stringify({
              authenticated: true,
              authority: { tier: "owner", login: "lmcfarlin" },
            }),
            { status: 200 },
          );
        }
        if (url.endsWith("/api/v1/sessions/bulk-action")) {
          return new Response(
            JSON.stringify({
              action: "clear",
              total: 2,
              succeeded: 2,
              results: [],
            }),
            { status: 200 },
          );
        }
        return new Response(JSON.stringify({}), { status: 200 });
      });

      const apiClient = new HubApiClient({
        fetchImpl: mockFetch as unknown as typeof fetch,
      });
      const wsClient = new HubWsClient({
        autoReconnect: false,
        webSocketFactory: () =>
          ({
            readyState: 1,
            onopen: null,
            onmessage: null,
            onclose: null,
            onerror: null,
            send: () => {},
            close: () => {},
          }) as unknown as WebSocket,
      });

      render(<App apiClient={apiClient} wsClient={wsClient} />);

      const settingsBtn = await screen.findByTestId("settings-modal-btn");
      fireEvent.click(settingsBtn);

      expect(screen.getByTestId("settings-modal")).toBeInTheDocument();
      fireEvent.click(screen.getByTestId("bulk-clear-btn"));
      fireEvent.click(screen.getByTestId("bulk-confirm-btn"));

      await waitFor(() => {
        expect(screen.getByTestId("bulk-action-result")).toHaveTextContent(
          "Completed Clear All: 2 / 2 sessions succeeded.",
        );
      });
    });
  });

  describe("Record 30 — Forward-Slash (/) In-Chat Autocomplete Menu", () => {
    it("shows floating slash-command-menu on leading /, filters items, and supports click, Enter/Tab insertion, and Escape dismissal", () => {
      render(
        <ChatPane
          agentName="ferris"
          agentDisplayName="Ferris"
          adapter="claude"
          sessionId="sess-ferris-01"
          turns={[]}
        />,
      );

      const textarea = screen.getByLabelText("Message") as HTMLTextAreaElement;

      // Typing "/" opens menu with default commands
      fireEvent.change(textarea, { target: { value: "/" } });
      expect(screen.getByTestId("slash-command-menu")).toBeInTheDocument();
      expect(screen.getByTestId("slash-command-item-compact")).toBeInTheDocument();
      expect(screen.getByTestId("slash-command-item-plan")).toBeInTheDocument();

      // Filtering by "/co" narrows to compact & code-review
      fireEvent.change(textarea, { target: { value: "/co" } });
      expect(screen.getByTestId("slash-command-item-compact")).toBeInTheDocument();
      expect(screen.getByTestId("slash-command-item-code-review")).toBeInTheDocument();
      expect(screen.queryByTestId("slash-command-item-plan")).toBeNull();

      // Pressing ArrowDown then Enter selects the second matching item (code-review)
      fireEvent.keyDown(textarea, { key: "ArrowDown" });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(textarea.value).toBe("/code-review ");
      expect(screen.queryByTestId("slash-command-menu")).toBeNull();

      // Typing "/pl" and clicking the item inserts "/plan "
      fireEvent.change(textarea, { target: { value: "/pl" } });
      expect(screen.getByTestId("slash-command-item-plan")).toBeInTheDocument();
      fireEvent.click(screen.getByTestId("slash-command-item-plan"));
      expect(textarea.value).toBe("/plan ");
      expect(screen.queryByTestId("slash-command-menu")).toBeNull();

      // Typing "/" and pressing Escape dismisses the menu
      fireEvent.change(textarea, { target: { value: "/" } });
      expect(screen.getByTestId("slash-command-menu")).toBeInTheDocument();
      fireEvent.keyDown(textarea, { key: "Escape" });
      expect(screen.queryByTestId("slash-command-menu")).toBeNull();
    });
  });
});
