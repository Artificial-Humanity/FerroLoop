import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "../App";
import { HubApiClient } from "../api/client";
import { HubWsClient } from "../api/ws";
import {
  ChatPane,
  formatModelOptionLabel,
  getVendorModelOptions,
  MUSE_MODEL_OPTIONS,
} from "../components/ChatPane";
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
      expect(getVendorModelOptions("muse")).toEqual([
        "muse-wonder-full-1.0",
        "muse-wonder-1.0",
        "muse-spark-1.3",
      ]);
      expect(MUSE_MODEL_OPTIONS).toEqual([
        {
          value: "muse-wonder-full-1.0",
          label: "Muse Wonder Full (muse-wonder-full-1.0)",
        },
        {
          value: "muse-wonder-1.0",
          label: "Muse Wonder (muse-wonder-1.0)",
        },
        {
          value: "muse-spark-1.3",
          label: "Muse Spark (muse-spark-1.3)",
        },
      ]);
      expect(formatModelOptionLabel("muse-wonder-full-1.0")).toBe(
        "Muse Wonder Full (muse-wonder-full-1.0)",
      );
      expect(formatModelOptionLabel("muse-wonder-1.0")).toBe(
        "Muse Wonder (muse-wonder-1.0)",
      );
      expect(formatModelOptionLabel("muse-spark-1.3")).toBe(
        "Muse Spark (muse-spark-1.3)",
      );
      expect(formatModelOptionLabel("gpt-6.1-terra")).toBe(
        "Codex Terra 6.1 (gpt-6.1-terra)",
      );
      expect(formatModelOptionLabel("gpt-6.1-sol")).toBe(
        "Codex Sol 6.1 (gpt-6.1-sol)",
      );
      expect(
        getVendorModelOptions("codex", null, ["gpt-6-astra", "gpt-6-sol"]),
      ).toEqual(["gpt-6.1-terra", "gpt-6.1-sol", "gpt-6-astra", "gpt-6-sol"]);
      expect(getVendorModelOptions("unknown")).toEqual([]);
      expect(getVendorModelOptions(null)).toEqual([]);
      expect(getVendorModelOptions("unknown", "gemini-4-argon")).toEqual([]);
      expect(getVendorModelOptions("muse", "gemini-4-argon")).toEqual([
        "muse-wonder-full-1.0",
        "muse-wonder-1.0",
        "muse-spark-1.3",
      ]);
      expect(getVendorModelOptions("unknown", "muse-spark-1.3")).toEqual([
        "muse-wonder-full-1.0",
        "muse-wonder-1.0",
        "muse-spark-1.3",
      ]);
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

    it("never falls back to gemini-4-argon in AgentSettingsModal for unknown or muse adapters and renders Muse 1.4.14 option labels", () => {
      const { rerender } = render(
        <ChatPane
          agentName="fenris"
          agentDisplayName="Fenris"
          adapter="unknown"
          model={null}
          thinkingLevel="high"
          sessionId="sess-fenris-01"
          turns={[]}
          agentSettingsOpen={true}
        />,
      );

      expect(screen.getByTestId("agent-settings-modal")).toBeInTheDocument();
      expect(screen.getByTestId("chat-active-model-badge")).toHaveTextContent(
        "default (high)",
      );
      expect(screen.getByTestId("chat-active-model-badge")).not.toHaveTextContent(
        "gemini-4-argon",
      );
      const unknownSelect = screen.getByTestId(
        "session-model-select",
      ) as HTMLSelectElement;
      expect(unknownSelect.value).toBe("");
      expect(
        Array.from(unknownSelect.options).map((o) => o.value),
      ).not.toContain("gemini-4-argon");

      // When adapter resolves to muse (with model={null}), selects muse-wonder-full-1.0 and renders Muse 1.4.14 labels
      rerender(
        <ChatPane
          agentName="fenris"
          agentDisplayName="Fenris"
          adapter="muse"
          model={null}
          thinkingLevel="high"
          sessionId="sess-fenris-01"
          turns={[]}
          agentSettingsOpen={true}
        />,
      );

      expect(screen.getByTestId("chat-active-model-badge")).toHaveTextContent(
        "muse · muse-wonder-full-1.0 (high)",
      );
      const museSelect = screen.getByTestId(
        "session-model-select",
      ) as HTMLSelectElement;
      expect(museSelect.value).toBe("muse-wonder-full-1.0");
      expect(
        Array.from(museSelect.options).map((o) => ({
          value: o.value,
          label: o.textContent,
        })),
      ).toEqual([
        {
          value: "muse-wonder-full-1.0",
          label: "Muse Wonder Full (muse-wonder-full-1.0)",
        },
        {
          value: "muse-wonder-1.0",
          label: "Muse Wonder (muse-wonder-1.0)",
        },
        {
          value: "muse-spark-1.3",
          label: "Muse Spark (muse-spark-1.3)",
        },
      ]);

      // Preserves explicit session.model on Muse agent
      rerender(
        <ChatPane
          agentName="fenris"
          agentDisplayName="Fenris"
          adapter="muse"
          model="muse-spark-1.3"
          thinkingLevel="high"
          sessionId="sess-fenris-01"
          turns={[]}
          agentSettingsOpen={true}
        />,
      );
      expect(screen.getByTestId("chat-active-model-badge")).toHaveTextContent(
        "muse · muse-spark-1.3 (high)",
      );
      expect(
        (screen.getByTestId("session-model-select") as HTMLSelectElement).value,
      ).toBe("muse-spark-1.3");
    });

    it("renders session-model-select and session-thinking-select inside AgentSettingsModal and calls onSetSessionModel on change", async () => {
      const onSetSessionModel = vi.fn().mockResolvedValue({
        session_id: "sess-felix-01",
        vendor: "antigravity",
        model: "gemini-3.8-flash",
        thinking_level: "xhigh",
        switched: true,
        method: "slash_model",
        persisted: true, record_id: "hosted-felix", restart_applies: true,
        live: { status: "applied", mechanism: "slash_model" },
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

    it.each([
      [true, "applied", true, /model: claude-sonnet-5-5/i, "claude-sonnet-5-5"],
      [true, "deferred", true, /applies on next restart/i, "claude-sonnet-5-5"],
      [true, "deferred", false, /saved.*deferred/i, "claude-sonnet-5-5"],
      [true, "held", true, /saved.*held/i, "claude-sonnet-5-5"],
      [true, "failed", true, /saved.*failed/i, "claude-sonnet-5-5"],
      [false, "deferred", false, /not.*saved/i, "claude-opus-5-5"],
      [false, "failed", false, /not.*saved/i, "claude-opus-5-5"],
    ])("reports callback result persisted=%s live=%s restart=%s", async (persisted, status, restartApplies, message, expectedModel) => {
      const onSetSessionModel = vi.fn().mockResolvedValue({
        session_id: "sess-ada-01", vendor: "claude", model: "claude-sonnet-5-5",
        persisted, record_id: persisted ? "hosted-ada" : null,
        live: { status, mechanism: "terminal_paste" }, restart_applies: restartApplies,
      });
      render(<ChatPane agentName="ada" adapter="claude" model="claude-opus-5-5"
        thinkingLevel="high" sessionId="sess-ada-01" turns={[]} agentSettingsOpen
        onSetSessionModel={onSetSessionModel} />);
      const select = screen.getByTestId("session-model-select") as HTMLSelectElement;
      fireEvent.change(select, { target: { value: "claude-sonnet-5-5" } });
      await waitFor(() => expect(screen.getByTestId("compact-feedback")).toHaveTextContent(message));
      expect(select.value).toBe(expectedModel);
      if (!persisted || !restartApplies || status !== "deferred") {
        expect(screen.getByTestId("compact-feedback")).not.toHaveTextContent(/applies on next restart/i);
      }
    });

    it.each([undefined, null, {}, { persisted: true }, { persisted: true, live: {} }, { persisted: true, live: { status: "unknown" } }])("restores the previous choice when outcome fields are missing (%s)", async (response) => {
      const onSetSessionModel = vi.fn().mockResolvedValue(response);
      render(<ChatPane agentName="ada" adapter="claude" model="claude-opus-5-5"
        thinkingLevel="high" sessionId="sess-ada-01" turns={[]} agentSettingsOpen
        onSetSessionModel={onSetSessionModel} />);
      const select = screen.getByTestId("session-model-select") as HTMLSelectElement;
      fireEvent.change(select, { target: { value: "claude-sonnet-5-5" } });
      await waitFor(() => expect(screen.getByTestId("compact-feedback")).toHaveTextContent(/status unavailable/i));
      expect(select.value).toBe("claude-opus-5-5");
    });

    it.each(["held", "failed"])("reports partial model delivery when the effort is %s", async (status) => {
      const onSetSessionModel = vi.fn().mockResolvedValue({
        persisted: false, record_id: null, restart_applies: false,
        live: { status, mechanism: "terminal_paste", queued: false, model_applied: true },
      });
      render(<ChatPane agentName="ada" adapter="claude" model="claude-opus-5-5"
        thinkingLevel="high" sessionId="sess-ada-01" turns={[]} agentSettingsOpen
        onSetSessionModel={onSetSessionModel} />);
      const model = screen.getByTestId("session-model-select") as HTMLSelectElement;
      fireEvent.change(model, { target: { value: "claude-sonnet-5-5" } });
      await waitFor(() => expect(screen.getByTestId("compact-feedback")).toHaveTextContent(/model.*applied.*effort/i));
      expect(screen.getByTestId("compact-feedback")).toHaveTextContent(/not saved/i);
      expect(model.value).toBe("claude-sonnet-5-5");
    });

    it("says a held live delivery was not queued and must be retried", async () => {
      const onSetSessionModel = vi.fn().mockResolvedValue({
        persisted: true, record_id: "hosted-ada", restart_applies: true,
        live: { status: "held", mechanism: "terminal_paste", queued: false, model_applied: false },
      });
      render(<ChatPane agentName="ada" adapter="claude" model="claude-opus-5-5"
        thinkingLevel="high" sessionId="sess-ada-01" turns={[]} agentSettingsOpen
        onSetSessionModel={onSetSessionModel} />);
      fireEvent.change(screen.getByTestId("session-model-select"), { target: { value: "claude-sonnet-5-5" } });
      await waitFor(() => expect(screen.getByTestId("compact-feedback")).toHaveTextContent(/not queued.*retry/i));
    });

    it("restores the previous choice when persistence fails", async () => {
      const onSetSessionModel = vi.fn().mockRejectedValue(new Error("Save failed"));
      render(<ChatPane agentName="ada" adapter="claude" model="claude-opus-5-5"
        thinkingLevel="high" sessionId="sess-ada-01" turns={[]} agentSettingsOpen
        onSetSessionModel={onSetSessionModel} />);
      const select = screen.getByTestId("session-model-select") as HTMLSelectElement;
      fireEvent.change(select, { target: { value: "claude-sonnet-5-5" } });
      await waitFor(() => expect(screen.getByTestId("model-switch-feedback")).toHaveTextContent("Save failed"));
      expect(select.value).toBe("claude-opus-5-5");
    });

    it("uses the direct client response and keeps a saved choice when live delivery fails", async () => {
      const setSessionModel = vi.fn().mockResolvedValue({
        session_id: "sess-ada-01", vendor: "claude", model: "claude-sonnet-5-5",
        persisted: true, record_id: "hosted-ada", restart_applies: true,
        live: { status: "failed", mechanism: "terminal_paste" },
      });
      render(<ChatPane agentName="ada" adapter="claude" model="claude-opus-5-5"
        thinkingLevel="high" sessionId="sess-ada-01" turns={[]} agentSettingsOpen
        apiClient={{ setSessionModel, getToken: () => null, listSlashCommands: async () => ({ commands: [] }) } as unknown as HubApiClient} />);
      const select = screen.getByTestId("session-model-select") as HTMLSelectElement;
      fireEvent.change(select, { target: { value: "claude-sonnet-5-5" } });
      await waitFor(() => expect(screen.getByTestId("compact-feedback")).toHaveTextContent(/saved.*failed/i));
      expect(select.value).toBe("claude-sonnet-5-5");
    });

    it("replaces stale model feedback with the Antigravity thinking validation in the dialog", async () => {
      const onSetSessionModel = vi.fn().mockResolvedValue({ persisted: true, record_id: "hosted-felix", restart_applies: true,
        live: { status: "deferred", mechanism: "override", model_applied: false, queued: false } });
      const props = { agentName: "felix", adapter: "antigravity", model: "gemini-4-argon", sessionId: "sess-felix", turns: [], agentSettingsOpen: true, onSetSessionModel };
      const { rerender } = render(<ChatPane {...props} thinkingLevel="high" />);
      fireEvent.change(screen.getByTestId("session-model-select"), { target: { value: "gemini-3.8-flash" } });
      await waitFor(() => expect(screen.getByTestId("model-switch-feedback")).toHaveTextContent(/applies on next restart/i));
      rerender(<ChatPane {...props} thinkingLevel={null} />);
      await waitFor(() => expect((screen.getByTestId("session-thinking-select") as HTMLSelectElement).value).toBe(""));
      fireEvent.change(screen.getByTestId("session-model-select"), { target: { value: "gemini-4-argon" } });
      expect(screen.getByTestId("model-switch-feedback")).toHaveTextContent(/choose a thinking level/i);
      expect(screen.getByTestId("model-switch-feedback")).not.toHaveTextContent(/restart/i);
      expect(onSetSessionModel).toHaveBeenCalledTimes(1);
    });

    it("announces the model result inside the open settings dialog", async () => {
      const onSetSessionModel = vi.fn().mockResolvedValue({ persisted: true, record_id: "hosted-ada", restart_applies: true,
        live: { status: "deferred", mechanism: "override", model_applied: false, queued: false } });
      render(<ChatPane agentName="ada" adapter="claude" model="claude-opus-5-5" thinkingLevel="high"
        sessionId="sess-ada" turns={[]} agentSettingsOpen onSetSessionModel={onSetSessionModel} />);
      fireEvent.change(screen.getByTestId("session-model-select"), { target: { value: "claude-sonnet-5-5" } });
      const dialog = screen.getByTestId("agent-settings-modal");
      await waitFor(() => expect(within(dialog).getByRole("status")).toHaveTextContent(/applies on next restart/i));
    });

    it.each(["resolve", "reject"])("ignores model request %s after changing agents", async (outcome) => {
      let finish!: (value: unknown) => void;
      let fail!: (error: Error) => void;
      const pending = new Promise((resolve, reject) => { finish = resolve; fail = reject; });
      const onSetSessionModel = vi.fn().mockReturnValue(pending);
      const { rerender } = render(<ChatPane agentName="ada" adapter="claude" model="claude-opus-5-5"
        thinkingLevel="high" sessionId="sess-ada" turns={[]} agentSettingsOpen onSetSessionModel={onSetSessionModel} />);
      fireEvent.change(screen.getByTestId("session-model-select"), { target: { value: "claude-sonnet-5-5" } });
      rerender(<ChatPane agentName="bob" adapter="claude" model="claude-haiku-4-5" thinkingLevel="low"
        sessionId="sess-bob" turns={[]} agentSettingsOpen onSetSessionModel={onSetSessionModel} />);
      await act(async () => { if (outcome === "reject") fail(new Error("old target error")); else finish({ persisted: false, live: { status: "failed" } }); });
      expect((screen.getByTestId("session-model-select") as HTMLSelectElement).value).toBe("claude-haiku-4-5");
      expect(screen.getByTestId("session-model-select")).not.toBeDisabled();
      expect(screen.queryByText("old target error")).toBeNull();
      expect(screen.queryByText(/choice was not saved/i)).toBeNull();
    });

    it("keeps the previous model after App refreshes an unpersisted and unapplied change", async () => {
      let sessionReads = 0;
      const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        const json = (body: unknown) => new Response(JSON.stringify(body), {
          status: 200, headers: { "Content-Type": "application/json" },
        });
        if (url.endsWith("/api/v1/auth/session")) return json({ authenticated: true, authority: { tier: "owner", login: "test" } });
        if (url.endsWith("/api/v1/agents")) return json({ agents: [{ name_key: "ferris", display_name: "Ferris", project: "FerroLoop", role: "lead" }], routes: [] });
        if (url.endsWith("/model")) return json({ session_id: "sess-ferris-live", vendor: "claude", model: "claude-sonnet-5-5", persisted: false, record_id: null, restart_applies: false, live: { status: "held", mechanism: "terminal_paste", queued: false, model_applied: false } });
        if (url.endsWith("/api/v1/sessions")) {
          sessionReads++;
          return json({ sessions: [{ id: "sess-ferris-live", persona: "ferris", adapter: "claude", model: "claude-opus-5-5", thinking_level: "high", status: "running" }] });
        }
        return json({});
      });
      const apiClient = new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
      const wsClient = {
        setToken: vi.fn(), connect: vi.fn(), disconnect: vi.fn(),
        onStatusChange: () => () => {}, subscribe: () => () => {},
      } as unknown as HubWsClient;
      render(<App apiClient={apiClient} wsClient={wsClient} />);
      fireEvent.click(await screen.findByTestId("agent-settings-btn-ferris"));
      const select = await screen.findByTestId("session-model-select") as HTMLSelectElement;
      await waitFor(() => expect(select.value).toBe("claude-opus-5-5"));
      const readsBefore = sessionReads;
      fireEvent.change(select, { target: { value: "claude-sonnet-5-5" } });
      await waitFor(() => expect(screen.getByTestId("compact-feedback")).toHaveTextContent(/not saved.*held/i));
      await waitFor(() => expect(sessionReads).toBeGreaterThan(readsBefore));
      expect(select.value).toBe("claude-opus-5-5");
      expect(screen.getByTestId("chat-active-model-badge")).toHaveTextContent("claude-opus-5-5");
    });

    it("does not fabricate high thinking level when unknown: omits thinking_level on Claude model-only change and blocks Antigravity model-only change until a level is chosen (#351)", async () => {
      const onSetSessionModel = vi.fn().mockResolvedValue({
        session_id: "sess-ada-01",
        vendor: "claude",
        model: "claude-sonnet-5-5",
        switched: true,
        method: "operator-prompt",
        persisted: true, record_id: "hosted-ada", restart_applies: true,
        live: { status: "applied", mechanism: "terminal_paste" },
      });

      const { rerender } = render(
        <ChatPane
          agentName="ada"
          agentDisplayName="Ada"
          adapter="claude"
          model="claude-opus-5-5"
          thinkingLevel={null}
          sessionId="sess-ada-01"
          turns={[]}
          agentSettingsOpen={true}
          onSetSessionModel={onSetSessionModel}
        />,
      );

      expect(screen.getByTestId("chat-active-model-badge")).toHaveTextContent(
        "claude · claude-opus-5-5 (unknown)",
      );
      const modelSelect = screen.getByTestId("session-model-select") as HTMLSelectElement;
      const thinkingSelect = screen.getByTestId("session-thinking-select") as HTMLSelectElement;
      expect(thinkingSelect.value).toBe("");
      expect(modelSelect).not.toBeDisabled();
      expect(screen.queryByTestId("agy-thinking-required-hint")).toBeNull();

      // Claude model-only change with unknown effort sends `undefined` for thinking_level (no guessed "high").
      fireEvent.change(modelSelect, { target: { value: "claude-sonnet-5-5" } });
      await waitFor(() => {
        expect(onSetSessionModel).toHaveBeenCalledWith("claude-sonnet-5-5", undefined);
        expect(screen.getByTestId("compact-feedback")).toHaveTextContent(
          "Model: claude-sonnet-5-5",
        );
      });

      onSetSessionModel.mockClear();

      // Antigravity session with unknown thinking_level: model select stays enabled so the operator
      // can pick and change their mind, while Save/apply is blocked until a thinking level is chosen.
      rerender(
        <ChatPane
          agentName="felix"
          agentDisplayName="Felix"
          adapter="antigravity"
          model="gemini-4-argon"
          thinkingLevel={null}
          sessionId="sess-felix-01"
          turns={[]}
          agentSettingsOpen={true}
          onSetSessionModel={onSetSessionModel}
        />,
      );

      expect(screen.getByTestId("chat-active-model-badge")).toHaveTextContent(
        "antigravity · gemini-4-argon (unknown)",
      );
      expect(thinkingSelect.value).toBe("");
      expect(modelSelect).not.toBeDisabled();
      expect(screen.getByTestId("agy-thinking-required-hint")).toHaveTextContent(
        /choose a thinking level/i,
      );
      // Antigravity thinking options must not label `high` as `(Default)`.
      const highOption = Array.from(thinkingSelect.options).find((o) => o.value === "high");
      expect(highOption?.textContent).toBe("High");
      expect(highOption?.textContent).not.toContain("Default");

      // Operator picks a model, then changes their mind to another model before picking a level:
      // the select stays enabled and updates, and no API call is made yet.
      fireEvent.change(modelSelect, { target: { value: "gemini-3.8-flash" } });
      expect(modelSelect.value).toBe("gemini-3.8-flash");
      expect(modelSelect).not.toBeDisabled();
      expect(onSetSessionModel).not.toHaveBeenCalled();

      fireEvent.change(modelSelect, { target: { value: "claude-sonnet-5-5" } });
      expect(modelSelect.value).toBe("claude-sonnet-5-5");
      expect(modelSelect).not.toBeDisabled();
      expect(onSetSessionModel).not.toHaveBeenCalled();

      // Once the operator picks an explicit thinking level, the change is sent with the chosen model and level.
      fireEvent.change(thinkingSelect, { target: { value: "low" } });
      await waitFor(() => {
        expect(onSetSessionModel).toHaveBeenCalledWith("claude-sonnet-5-5", "low");
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
