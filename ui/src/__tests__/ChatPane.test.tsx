import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { HubApiClient, HubApiError } from "../api/client";
import {
  ALWAYS_SEND_NOW_STORAGE_KEY,
  ChatPane,
  sessionDeliveryReceipts,
  sessionDraftMemory,
  sessionScrollMemory,
} from "../components/ChatPane";
import {
  SAFE_AREA_MARGINS_STORAGE_KEY,
  readInitialShowSafeAreaMargins,
} from "../components/SettingsModal";
import type { QueuedOperatorPrompt, TranscriptTurn } from "../types/api";

describe("ChatPane", () => {
  beforeEach(() => {
    sessionScrollMemory.clear();
    sessionDraftMemory.clear();
    sessionDeliveryReceipts.clear();
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

  it.each([
    ["fetch", false, false], ["body", false, false],
    ["fetch", false, true], ["body", false, true],
    ["fetch", true, false], ["body", true, false],
    ["fetch", true, true], ["body", true, true],
    ["empty", false, true], ["malformed", false, false],
    ["empty", true, false], ["malformed", true, true],
    ["callback", false, true], ["callback", true, true],
    ["legacy", false, true], ["http500", true, false], ["transport", false, false],
  ] as const)("retains an unconfirmed Codex Send Now receipt after lost %s (queued=%s, callback=%s)", async (failure, queued, callback) => {
    const target = `lost-${failure}-${queued}-${callback}`;
    let release!: () => void;
    const pending = new Promise<void>((resolve) => { release = resolve; });
    let sends = 0;
    const apiClient = new HubApiClient({ fetchImpl: async (_input, init) => {
      if (init?.method !== "POST") return new Response(JSON.stringify({ turns: [], queue: [] }));
      sends++;
      await pending;
      if (failure === "fetch") throw new TypeError("fixture transport interruption");
      const body = failure === "malformed" ? "broken json" : failure === "legacy" ? JSON.stringify({ delivered: true }) : failure === "transport" ? JSON.stringify({ delivered: true, delivery_stage: "transport_accepted" }) : "";
      const response = new Response(body, { status: failure === "http500" ? 500 : 200 });
      if (failure === "body") vi.spyOn(response, "text").mockRejectedValue(new TypeError("fixture transport interruption"));
      return response;
    } });
    const rejectCallback = async () => { sends++; await pending; throw new Error("fixture refusal"); };
    window.localStorage.setItem(ALWAYS_SEND_NOW_STORAGE_KEY, "true");
    const pane = (sessionId: string) => <ChatPane agentName="freya" adapter="codex" sessionId={sessionId} turns={[]}
      queue={queued && sessionId === target ? [{ id: "lost-queued", session_id: target, prompt: "submitted once", by: "ferris", created_at: "2026-11-06T00:00:00Z" }] : []}
      {...(callback ? {
        onSendPrompt: failure === "callback" ? rejectCallback : (id, payload) => apiClient.sendSessionPrompt(id, payload),
        onSendQueuedNow: failure === "callback" ? rejectCallback : (id) => apiClient.sendQueuedPromptNow(target, id),
      } : { apiClient })} />;
    const view = render(pane(target));
    if (queued) fireEvent.click(screen.getByTestId("queued-send-now-lost-queued"));
    else {
      fireEvent.change(screen.getByLabelText("Message"), { target: { value: "submitted once" } });
      fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    }
    view.rerender(pane("lost-other"));
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "other draft" } });
    await act(async () => { release(); });
    expect(screen.queryByText(/Delivery unconfirmed/)).toBeNull();
    expect(screen.getByLabelText("Message")).toHaveValue("other draft");
    view.rerender(pane(target));
    expect(await screen.findAllByText(/Delivery unconfirmed/)).not.toHaveLength(0);
    expect(screen.getByLabelText("Message")).toHaveValue("");
    expect(window.localStorage.getItem(`ferrodeck.draft.${target}`)).toBeNull();
    if (queued) expect(screen.getByTestId("queued-send-now-lost-queued")).toBeDisabled();
    view.unmount();
    const remounted = render(pane(target));
    expect(screen.getAllByText(/Delivery unconfirmed/)).not.toHaveLength(0);
    expect(screen.getByLabelText("Message")).toHaveValue("");
    if (queued) {
      expect(screen.getByTestId("queued-send-now-lost-queued")).toBeDisabled();
      fireEvent.click(screen.getByTestId("queued-send-now-lost-queued"));
    }
    expect(sends).toBe(1);
    remounted.unmount();
  });

  it.each([
    ["refusal", "codex", true, "owner"], ["refusal-body", "codex", true, "owner"], ["refusal-stage", "codex", true, "owner"],
    ["fetch", "codex", false, "owner"], ["body", "codex", false, "owner"],
    ["fetch", "claude", true, "owner"], ["fetch", "codex", true, "peer"],
  ] as const)("restores drafts for %s with adapter=%s, Send Now=%s, authority=%s", async (failure, adapter, sendNow, tier) => {
    const target = `restore-${failure}-${adapter}-${sendNow}-${tier}`;
    const apiClient = new HubApiClient({ fetchImpl: async (_input, init) => {
      if (init?.method !== "POST") return new Response(JSON.stringify({ turns: [], queue: [] }));
      if (failure === "fetch") throw new TypeError("fixture transport interruption");
      const response = new Response(JSON.stringify(failure === "refusal-stage" ? { delivery_stage: "refused", delivery_error: "fixture refusal" } : { error: "fixture refusal" }), { status: failure === "refusal" || failure === "refusal-body" ? 403 : 200 });
      if (failure === "body" || failure === "refusal-body") vi.spyOn(response, "text").mockRejectedValue(new TypeError("fixture transport interruption"));
      return response;
    } });
    window.localStorage.setItem(ALWAYS_SEND_NOW_STORAGE_KEY, String(sendNow));
    render(<ChatPane agentName="freya" adapter={adapter} sessionId={target} turns={[]} apiClient={apiClient}
      authority={tier === "owner" ? { tier, login: "fixture" } : { tier, agent: "fixture" }} />);
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "safe draft" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    await waitFor(() => expect(screen.getByLabelText("Message")).toHaveValue("safe draft"));
    expect(window.localStorage.getItem(`ferrodeck.draft.${target}`)).toBe("safe draft");
    expect(screen.queryByText(/Delivery unconfirmed/)).toBeNull();
  });

  describe.each(["app_server_accepted", "unconfirmed"] as const)("after a prior %s delivery", (previousStage) => {
    it.each([
      ["refused", false, false], ["refused", false, true],
      ["refused", true, false], ["refused", true, true],
      ["not_connected", false, false], ["not_connected", false, true],
      ["not_connected", true, false], ["not_connected", true, true],
      ["http403", false, false], ["http403", false, true],
      ["http403", true, false], ["http403", true, true],
    ] as const)("clears stale acceptance and preserves uncertainty after %s (queued=%s, callback=%s)", async (refusal, queued, callback) => {
      const target = `prior-${previousStage}-${refusal}-${queued}-${callback}`;
      let sends = 0;
      const apiClient = new HubApiClient({ fetchImpl: async (_input, init) => {
        if (init?.method !== "POST") return new Response(JSON.stringify({ turns: [], queue: [] }));
        sends++;
        if (sends === 1) return new Response(JSON.stringify({ session_id: target, queued: false,
          delivered: previousStage !== "unconfirmed", delivery_stage: previousStage }));
        return new Response(JSON.stringify(refusal === "http403" ? { error: "second input refused" } : {
          session_id: target, queued: false, delivered: false, delivery_stage: refusal, delivery_error: "second input refused",
        }), { status: refusal === "http403" ? 403 : 200 });
      } });
      window.localStorage.setItem(ALWAYS_SEND_NOW_STORAGE_KEY, "true");
      const pane = <ChatPane agentName="freya" adapter="codex" sessionId={target} turns={[]}
        queue={queued ? ["first", "second"].map((id) => ({ id, session_id: target, prompt: `${id} input`,
          by: "ferris", created_at: "2026-11-06T00:00:00Z" })) : []}
        {...(callback ? {
          onSendPrompt: (id, payload) => apiClient.sendSessionPrompt(id, payload),
          onSendQueuedNow: (id) => apiClient.sendQueuedPromptNow(target, id),
        } : { apiClient })} />;
      const view = render(pane);
      if (queued) fireEvent.click(screen.getByTestId("queued-send-now-first"));
      else {
        fireEvent.change(screen.getByLabelText("Message"), { target: { value: "first input" } });
        fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
      }
      const previousStatus = previousStage === "unconfirmed" ? /Delivery unconfirmed/ : /Codex app server accepted the submission/;
      expect(await screen.findByText(previousStatus)).toBeInTheDocument();
      expect(screen.getByLabelText("Message")).toHaveValue("");
      fireEvent.change(screen.getByLabelText("Message"), { target: { value: "second input" } });
      if (queued) fireEvent.click(screen.getByTestId("queued-send-now-second"));
      else fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
      expect(await screen.findByRole("alert")).toHaveTextContent("second input refused");
      expect(screen.queryByText(/Codex app server accepted the submission/)).toBeNull();
      expect(screen.getByLabelText("Message")).toHaveValue("second input");
      expect(window.localStorage.getItem(`ferrodeck.draft.${target}`)).toBe("second input");
      if (previousStage === "unconfirmed") {
        expect(screen.getByText(/Delivery unconfirmed/)).toBeInTheDocument();
        if (queued) {
          expect(screen.getByTestId("queued-send-now-first")).toBeDisabled();
          fireEvent.click(screen.getByTestId("queued-send-now-first"));
        }
      } else expect(screen.queryByText(/Delivery unconfirmed/)).toBeNull();
      expect(sends).toBe(2);
      view.unmount();
      const remounted = render(pane);
      expect(screen.queryByText(/Codex app server accepted the submission/)).toBeNull();
      expect(screen.getByLabelText("Message")).toHaveValue("second input");
      if (previousStage === "unconfirmed") {
        expect(screen.getByText(/Delivery unconfirmed/)).toBeInTheDocument();
        if (queued) expect(screen.getByTestId("queued-send-now-first")).toBeDisabled();
      }
      remounted.unmount();
    });
  });

  it.each([
    ["vendor_queued", "Submitted to Codex queue.", false],
    ["vendor_queued", "Submitted to Codex queue.", true],
    ["app_server_accepted", "Codex app server accepted the submission for the targeted turn. This receipt does not confirm model consumption.", false],
    ["app_server_accepted", "Codex app server accepted the submission for the targeted turn. This receipt does not confirm model consumption.", true],
  ] as const)("keeps a historical %s receipt after later transcript output (%s, queued=%s)", async (delivery_stage, status, queued) => {
    const target = `historical-${delivery_stage}-${queued}`;
    const onSendPrompt = vi.fn().mockResolvedValue({
      session_id: target, queued: false, delivered: true, delivery_stage,
    });
    const onSendQueuedNow = vi.fn().mockResolvedValue({
      session_id: target, queued: false, delivered: true, delivery_stage,
    });
    const pane = (turns: TranscriptTurn[]) => <ChatPane agentName="ferris" adapter="codex" sessionId={target}
      turns={turns} onSendPrompt={onSendPrompt} onSendQueuedNow={onSendQueuedNow}
      queue={queued && turns.length === 0 ? [{ id: "historical-prompt", session_id: target,
        prompt: "Check messaging progress.", by: "operator", created_at: "2026-11-06T14:01:21Z" }] : []} />;
    const view = render(pane([]));
    if (queued) fireEvent.click(screen.getByTestId("queued-send-now-historical-prompt"));
    else {
      fireEvent.change(screen.getByLabelText("Message"), { target: { value: "Check messaging progress." } });
      fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    }
    await waitFor(() => expect(sessionDeliveryReceipts.has(target)).toBe(true));
    const laterTurns: TranscriptTurn[] = [
      { index: 1355, role: "user", content: "Check messaging progress.", thinking: null, tool_calls: [], created_at: "2026-11-06T14:01:21Z" },
      { index: 1358, role: "assistant", content: "Here is the progress report.", thinking: null, tool_calls: [], created_at: "2026-11-06T14:01:46Z" },
    ];
    view.rerender(pane(laterTurns));
    expect(screen.getByText("Here is the progress report.")).toBeInTheDocument();
    expect(screen.getByText(status)).toBeInTheDocument();
    expect(screen.queryByText(/Waiting for Codex to consume it|Accepted by the active Codex turn|Model consumption is not confirmed/)).toBeNull();
    expect(screen.getByLabelText("Message")).toHaveValue("");
    view.unmount();
    render(pane(laterTurns));
    expect(screen.getByText(status)).toBeInTheDocument();
    expect(screen.queryByText(/Waiting for Codex to consume it|Accepted by the active Codex turn|Model consumption is not confirmed/)).toBeNull();
    expect(queued ? onSendQueuedNow : onSendPrompt).toHaveBeenCalledTimes(1);
  });

  it.each([
    ["vendor_queued", "Submitted to Codex queue"],
    ["app_server_accepted", "Codex app server accepted the submission"],
    ["unconfirmed", "Delivery unconfirmed"],
  ])("keeps the submitted draft cleared for explicit %s outcomes", async (delivery_stage, status) => {
    const onSendPrompt = vi.fn().mockResolvedValue({
      session_id: "codex-fixture", queued: false, delivered: false,
      delivery_stage, delivery_error: delivery_stage === "unconfirmed" ? "ack lost" : null,
    });
    render(<ChatPane agentName="freya" adapter="codex" sessionId="codex-fixture"
      turns={[]} onSendPrompt={onSendPrompt} />);
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "steering fixture" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    await waitFor(() => expect(screen.getByText(new RegExp(status))).toBeInTheDocument());
    expect(screen.getByLabelText("Message")).toHaveValue("");
    expect(onSendPrompt).toHaveBeenCalledTimes(1);
    expect(window.localStorage.getItem("ferrodeck.draft.codex-fixture")).toBeNull();
  });

  it.each([
    ["vendor_queued", "Submitted to Codex queue"],
    ["app_server_accepted", "Codex app server accepted the submission"],
    ["unconfirmed", "Delivery unconfirmed"],
  ])("retains deferred %s receipts for their submitted target across switches and remounts", async (delivery_stage, status) => {
    for (const queued of [false, true]) {
      for (const useApi of [false, true]) {
        const target = `receipt-${delivery_stage}-${queued}-${useApi}`;
        let resolveSend!: (value: unknown) => void;
        const response = new Promise<unknown>((resolve) => { resolveSend = resolve; });
        const onSendPrompt = vi.fn((id: string) => id === target ? response : Promise.resolve({ delivered: true, delivery_stage: "transport_accepted" }));
        const onSendQueuedNow = vi.fn(() => response);
        const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
          const url = String(input);
          const body = url.includes(`/sessions/${target}/`) ? await response : { delivered: true, delivery_stage: "transport_accepted" };
          return new Response(JSON.stringify(body), { status: 200, headers: { "Content-Type": "application/json" } });
        });
        const apiClient = new HubApiClient({ fetchImpl: fetchImpl as typeof fetch });
        const pane = (sessionId: string) => <ChatPane agentName="freya" adapter="codex" sessionId={sessionId} turns={[]}
          queue={queued && sessionId === target ? [{ id: "pending-receipt", session_id: target, prompt: "queued fixture", by: "ferris", created_at: "2026-11-06T00:00:00Z" }] : []}
          {...(useApi ? { apiClient } : { onSendPrompt, onSendQueuedNow })} />;
        const view = render(pane(target));
        if (queued) fireEvent.click(screen.getByTestId("queued-send-now-pending-receipt"));
        else {
          fireEvent.change(screen.getByLabelText("Message"), { target: { value: "submitted once" } });
          fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
        }
        view.rerender(pane("receipt-other"));
        fireEvent.change(screen.getByLabelText("Message"), { target: { value: "other target draft" } });
        await act(async () => { resolveSend({ session_id: target, queued: queued && delivery_stage === "unconfirmed", delivered: delivery_stage !== "unconfirmed", delivery_stage }); });
        expect(screen.queryByText(new RegExp(status))).toBeNull();
        expect(screen.getByLabelText("Message")).toHaveValue("other target draft");
        view.rerender(pane(target));
        expect(await screen.findByText(new RegExp(status))).toBeInTheDocument();
        expect(screen.getByLabelText("Message")).toHaveValue("");
        // A successful send to another target must not overwrite this receipt.
        view.rerender(pane("receipt-other"));
        fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
        await act(async () => {});
        view.rerender(pane(target));
        expect(screen.getByText(new RegExp(status))).toBeInTheDocument();
        view.unmount();
        const remounted = render(pane(target));
        expect(screen.getByText(new RegExp(status))).toBeInTheDocument();
        expect(screen.getByLabelText("Message")).toHaveValue("");
        if (useApi) expect(fetchImpl.mock.calls.filter(([input]) => String(input).includes(`/sessions/${target}/`))).toHaveLength(1);
        else expect(queued ? onSendQueuedNow.mock.calls : onSendPrompt.mock.calls.filter(([id]) => id === target)).toHaveLength(1);
        remounted.unmount();
      }
    }
  });

  it.each([false, true])("retains a receipt that completes after its originating pane unmounts (queued=%s)", async (queued) => {
    const target = `receipt-unmounted-${queued}`;
    let resolveSend!: (value: unknown) => void;
    const response = new Promise<unknown>((resolve) => { resolveSend = resolve; });
    const onSendPrompt = vi.fn(() => response);
    const onSendQueuedNow = vi.fn(() => response);
    const pane = <ChatPane agentName="freya" adapter="codex" sessionId={target} turns={[]} onSendPrompt={onSendPrompt} onSendQueuedNow={onSendQueuedNow}
      queue={queued ? [{ id: "unmounted-receipt", session_id: target, prompt: "queued fixture", by: "ferris", created_at: "2026-11-06T00:00:00Z" }] : []} />;
    const view = render(pane);
    if (queued) fireEvent.click(screen.getByTestId("queued-send-now-unmounted-receipt"));
    else {
      fireEvent.change(screen.getByLabelText("Message"), { target: { value: "submitted once" } });
      fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    }
    view.unmount();
    const remounted = render(pane);
    await act(async () => { resolveSend({ session_id: target, queued: false, delivered: false, delivery_stage: "unconfirmed" }); });
    expect(await screen.findByText(/Delivery unconfirmed/)).toBeInTheDocument();
    expect(screen.getByLabelText("Message")).toHaveValue("");
    expect(queued ? onSendQueuedNow : onSendPrompt).toHaveBeenCalledTimes(1);
    remounted.unmount();
  });

  it.each([undefined, "refused"])("restores drafts for known failure with legacy or %s stage", async (delivery_stage) => {
    const onSendPrompt = vi.fn().mockResolvedValue({ session_id: "codex-refused", queued: false,
      delivered: false, delivery_stage, delivery_error: "input was refused" });
    render(<ChatPane agentName="freya" adapter="codex" sessionId="codex-refused" turns={[]} onSendPrompt={onSendPrompt} />);
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "safe draft" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    await waitFor(() => expect(screen.getByLabelText("Message")).toHaveValue("safe draft"));
    expect(screen.getByText("input was refused")).toBeInTheDocument();
    expect(onSendPrompt).toHaveBeenCalledTimes(1);
  });

  it.each(["vendor_queued", "app_server_accepted", "unconfirmed"])("shows explicit queued Send now %s outcome without retry", async (delivery_stage) => {
    const onSendQueuedNow = vi.fn().mockResolvedValue({ session_id: "codex-queued", queued: delivery_stage === "unconfirmed",
      delivered: delivery_stage !== "unconfirmed", delivery_stage });
    render(<ChatPane agentName="freya" adapter="codex" sessionId="codex-queued" turns={[]}
      queue={[{ id: "fixture-queue", session_id: "codex-queued", prompt: "queued fixture", by: "ferris", created_at: "2026-11-06T00:00:00Z" }]}
      onSendQueuedNow={onSendQueuedNow} />);
    fireEvent.click(screen.getByTestId("queued-send-now-fixture-queue"));
    const expected = delivery_stage === "vendor_queued" ? /Submitted to Codex queue/ : delivery_stage === "app_server_accepted" ? /Codex app server accepted the submission/ : /Delivery unconfirmed/;
    await waitFor(() => expect(screen.getByText(expected)).toBeInTheDocument());
    expect(onSendQueuedNow).toHaveBeenCalledTimes(1);
    expect(screen.queryByText(/delivery could not be confirmed/i)).toBeNull();
  });

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

    // Directed prose turn encapsulates internal work (1 thinking + 1 tool call) collapsed by default
    const internalToggle = screen.getByTestId("turn-internal-work-toggle-1");
    expect(internalToggle).toHaveAttribute("aria-expanded", "false");
    expect(internalToggle).toHaveTextContent("Internal work · 1 thinking · 1 tool call");
    fireEvent.click(internalToggle);
    expect(internalToggle).toHaveAttribute("aria-expanded", "true");

    // ThinkingBlock inside is collapsed by default
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

    fireEvent.click(screen.getByTestId("turn-internal-work-toggle-1"));

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
    expect(promptTextarea).toHaveAttribute("autocomplete", "off");
    expect(promptTextarea).not.toHaveAttribute("name");
    expect(promptTextarea.closest("form")).toHaveAttribute("autocomplete", "off");
    expect(promptTextarea).toHaveAttribute("role", "searchbox");
    expect(promptTextarea).toHaveAttribute("data-1p-ignore", "true");
    expect(promptTextarea).toHaveAttribute("data-lpignore", "true");
    expect(promptTextarea).toHaveAttribute("data-bwignore", "true");
    expect(promptTextarea).toHaveAttribute("data-form-type", "other");
    expect(promptTextarea).toHaveAttribute("aria-autocomplete", "none");
    expect(promptTextarea).toHaveAttribute("enterKeyHint", "enter");
    expect(promptTextarea).toHaveAttribute(
      "placeholder",
      "Send a message to Ferris... (Ctrl+Enter to submit, paste or attach files)",
    );

    // Verify readInitialShowSafeAreaMargins() returns false even when window.innerWidth is 390
    // and ignores stale v1 "fd_show_safe_margins" storage key
    const originalInnerWidth = window.innerWidth;
    try {
      window.localStorage.setItem("fd_show_safe_margins", "true");
      window.localStorage.removeItem(SAFE_AREA_MARGINS_STORAGE_KEY);
      Object.defineProperty(window, "innerWidth", {
        configurable: true,
        writable: true,
        value: 390,
      });
      fireEvent(window, new Event("resize"));
      expect(promptTextarea).toHaveAttribute(
        "placeholder",
        "Send a message to Ferris...",
      );
      expect(SAFE_AREA_MARGINS_STORAGE_KEY).toBe("fd_show_safe_margins_v2");
      expect(readInitialShowSafeAreaMargins()).toBe(false);
    } finally {
      window.localStorage.removeItem("fd_show_safe_margins");
      window.localStorage.removeItem(SAFE_AREA_MARGINS_STORAGE_KEY);
      Object.defineProperty(window, "innerWidth", {
        configurable: true,
        writable: true,
        value: originalInnerWidth,
      });
    }

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

    // Intermediate work turn is encapsulated into a collapsed encapsulated-work-block by default
    const workBlock = screen.getByTestId("encapsulated-work-block");
    expect(workBlock).toHaveAttribute("data-turn-indices", "912,913");
    const workToggle = screen.getByTestId("encapsulated-work-toggle");
    expect(workToggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.getByTestId("encapsulated-work-range")).toHaveTextContent("#912");
    expect(screen.getByTestId("encapsulated-work-summary")).toHaveTextContent(
      "1 step · 1 tool call (read_file) · 1 output",
    );

    // Expand the encapsulated work block to inspect turn-card-912
    fireEvent.click(workToggle);
    expect(workToggle).toHaveAttribute("aria-expanded", "true");

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

  it("encapsulates consecutive intermediate thinking/tool/output turns (#2051–#2058) into a single collapsed work block", () => {
    const multiStepWorkTurns: TranscriptTurn[] = [
      {
        index: 2051,
        role: "assistant",
        content: "",
        thinking: "Inspecting hub discovery and App.tsx.",
        tool_calls: [
          { name: "read_file", arguments: { Path: "ui/src/App.tsx" } },
          { name: "run_command", arguments: { CommandLine: "git status" } },
        ],
        created_at: "2026-11-01T23:03:23Z",
      },
      {
        index: 2052,
        role: "tool",
        content: "Created At: 2026-11-01T23:03:23Z\nline 2\nline 3",
        thinking: null,
        tool_calls: [],
      },
      {
        index: 2053,
        role: "tool",
        content:
          "Created At: 2026-11-01T23:03:24Z\nCommand exited with code 0\nline 3\nline 4\nline 5\nline 6",
        thinking: null,
        tool_calls: [],
      },
      {
        index: 2054,
        role: "assistant",
        content: "",
        thinking: "Running test suite.\nChecking output.",
        tool_calls: [{ name: "run_command", arguments: { CommandLine: "cargo test" } }],
        created_at: "2026-11-01T23:03:37Z",
      },
      {
        index: 2055,
        role: "tool",
        content: "Created At: 2026-11-01T23:03:38Z\nCommand exited with code 0",
        thinking: null,
        tool_calls: [],
      },
      {
        index: 2056,
        role: "assistant",
        content: "",
        thinking: "Reading ChatPane.tsx.",
        tool_calls: [{ name: "read_file", arguments: { Path: "ui/src/components/ChatPane.tsx" } }],
        created_at: "2026-11-01T23:04:01Z",
      },
      {
        index: 2057,
        role: "tool",
        content: "Created At: 2026-11-01T23:04:02Z\nShowing lines 1-90",
        thinking: null,
        tool_calls: [],
      },
      {
        index: 2058,
        role: "assistant",
        content: "",
        thinking: "Checking branch status.",
        tool_calls: [{ name: "run_command", arguments: { CommandLine: "git diff" } }],
        created_at: "2026-11-01T23:04:22Z",
      },
      {
        index: 2059,
        role: "tool",
        content: "Created At: 2026-11-01T23:04:23Z\nCommand exited with code 0",
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
        turns={multiStepWorkTurns}
        onSendPrompt={vi.fn()}
      />,
    );

    const blocks = screen.getAllByTestId("encapsulated-work-block");
    expect(blocks.length).toBe(1);
    expect(screen.getByTestId("encapsulated-work-range")).toHaveTextContent("#2051–#2058");
    expect(screen.getByTestId("encapsulated-work-summary")).toHaveTextContent(
      "4 steps · 5 tool calls (read_file, run_command) · 5 outputs",
    );
    expect(screen.queryByTestId("turn-card-2051")).toBeNull();
    expect(screen.queryByTestId("turn-card-2058")).toBeNull();

    fireEvent.click(screen.getByTestId("encapsulated-work-toggle"));
    expect(screen.getByTestId("turn-card-2051")).toBeInTheDocument();
    expect(screen.getByTestId("turn-card-2054")).toBeInTheDocument();
    expect(screen.getByTestId("turn-card-2056")).toBeInTheDocument();
    expect(screen.getByTestId("turn-card-2058")).toBeInTheDocument();
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

describe("ChatPane AgentSettingsModal shows Execution Mode instead of vendor account quota", () => {
  const codexQuota = () => ({
    tier: "plus",
    primary_window: {
      used_percent: 2,
      window_minutes: 300,
      resets_at_ms: Date.now() + (3 * 60 + 14) * 60_000,
    },
    secondary_window: {
      used_percent: 1,
      window_minutes: 10_080,
      resets_at_ms: Date.now() + (6 * 24 * 60 + 22 * 60) * 60_000,
    },
  });

  it("keeps the toolbar free of the quota badge and omits the quota summary from the modal, which shows the Execution Mode block", () => {
    render(
      <ChatPane
        agentName="ferris"
        agentDisplayName="Ferris"
        adapter="codex"
        sessionId="sess-codex-01"
        turns={[]}
        agentSettingsOpen={true}
        quotaUsage={codexQuota()}
        onSendPrompt={vi.fn()}
      />,
    );

    expect(screen.getByTestId("agent-settings-execution-mode")).toHaveTextContent(
      "Execution Mode",
    );
    expect(screen.getByTestId("execution-mode-badge")).toHaveTextContent("Unknown");
    expect(screen.queryByTestId("agent-settings-quota-summary")).toBeNull();
    expect(screen.queryByTestId("quota-usage-badge")).toBeNull();
    expect(screen.queryByTestId("quota-tier-chip")).toBeNull();
    expect(screen.queryByTestId("quota-primary-window")).toBeNull();
    expect(screen.queryByTestId("quota-secondary-window")).toBeNull();
    expect(screen.queryByText("Vendor Account Quota")).toBeNull();
  });

  it("shows the exhausted-quota toolbar pill, and opening the modal from it shows no quota details there", () => {
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

    fireEvent.click(toolbarPill);

    expect(screen.getByTestId("agent-settings-execution-mode")).toBeInTheDocument();
    expect(screen.queryByTestId("quota-usage-badge")).toBeNull();
    expect(screen.queryByTestId("quota-exhausted-alert")).toBeNull();
    expect(screen.queryByTestId("quota-primary-window")).toBeNull();
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
  it("opens the Cancel | Decommission | Prepare for Decommission modal and auto-hides the ? help popover after 6 seconds", () => {
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
      expect(screen.getByTestId("decommission-teardown-btn")).toHaveTextContent(/^Decommission$/);
      expect(screen.getByTestId("decommission-prepare-btn")).toHaveTextContent(
        /^Prepare for Decommission$/,
      );
      expect(document.body).not.toHaveTextContent(/Tear Down/);
      expect(document.body).not.toHaveTextContent(/Prepare & Decommission/);

      expect(screen.queryByTestId("decommission-help-popover")).toBeNull();
      fireEvent.click(screen.getByTestId("decommission-help-btn"));

      const popover = screen.getByTestId("decommission-help-popover");
      expect(popover).toHaveTextContent(/Prepare for Decommission:/);
      expect(popover).toHaveTextContent(/STATE\.md\/FerroState/);
      expect(popover).toHaveTextContent(/Decommission:/);
      expect(popover).not.toHaveTextContent(/Tear Down/);
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
      "Prepared for decommission",
    );

    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));

    await waitFor(() => {
      expect(onDecommissionSession).toHaveBeenCalledWith("teardown");
    });
    expect(await screen.findByTestId("compact-feedback")).toHaveTextContent(
      "Decommissioned session",
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
    fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));

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

describe("ChatPane Decommission asks for a second confirmation (Record #128)", () => {
  function renderPane(onDecommissionSession = vi.fn(async () => undefined)) {
    render(
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
    return onDecommissionSession;
  }

  it("does not decommission when Decommission is clicked; it opens the confirmation naming the agent", () => {
    const onDecommissionSession = renderPane();
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));

    expect(onDecommissionSession).not.toHaveBeenCalled();
    const dialog = screen.getByTestId("decommission-confirm-dialog");
    expect(dialog).toHaveAttribute("aria-modal", "true");
    expect(dialog).toHaveTextContent(/Are you sure you want to decommission\s+Kacey\?/);
    expect(dialog).toHaveTextContent(
      "This immediately stops the runtime session, clears queues and routes, and unregisters the persona.",
    );
    expect(screen.getByTestId("decommission-confirm-cancel-btn")).toHaveTextContent("Cancel");
    expect(screen.getByTestId("decommission-confirm-submit-btn")).toHaveTextContent(
      "Yes, Decommission",
    );
    // The choices dialog is replaced, not stacked under the confirmation.
    expect(screen.queryByTestId("decommission-modal")).toBeNull();
    // The safe button takes the focus so a stray Enter does not decommission.
    expect(screen.getByTestId("decommission-confirm-cancel-btn")).toHaveFocus();
  });

  it("Cancel returns to the choices without decommissioning", () => {
    const onDecommissionSession = renderPane();
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    fireEvent.click(screen.getByTestId("decommission-confirm-cancel-btn"));

    expect(onDecommissionSession).not.toHaveBeenCalled();
    expect(screen.queryByTestId("decommission-confirm-dialog")).toBeNull();
    expect(screen.getByTestId("decommission-modal")).toBeInTheDocument();
  });

  it("Escape and a backdrop click leave the confirmation one step at a time without decommissioning", () => {
    const onDecommissionSession = renderPane();
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByTestId("decommission-confirm-dialog")).toBeNull();
    expect(screen.getByTestId("decommission-modal")).toBeInTheDocument();

    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    const backdrop = screen.getByTestId("decommission-confirm-dialog").parentElement as HTMLElement;
    fireEvent.click(backdrop);
    expect(screen.queryByTestId("decommission-confirm-dialog")).toBeNull();
    expect(screen.getByTestId("decommission-modal")).toBeInTheDocument();

    // A second Escape closes the choices dialog itself.
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByTestId("decommission-modal")).toBeNull();
    expect(onDecommissionSession).not.toHaveBeenCalled();
  });

  it("Yes, Decommission calls teardown exactly once and closes both dialogs", async () => {
    const onDecommissionSession = renderPane();
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));

    await waitFor(() => {
      expect(onDecommissionSession).toHaveBeenCalledTimes(1);
    });
    expect(onDecommissionSession).toHaveBeenCalledWith("teardown");
    await waitFor(() => {
      expect(screen.queryByTestId("decommission-confirm-dialog")).toBeNull();
    });
    expect(screen.queryByTestId("decommission-modal")).toBeNull();
  });

  it("Prepare for Decommission still acts in one step, with no confirmation", async () => {
    const onDecommissionSession = renderPane();
    fireEvent.click(screen.getByTestId("decommission-prepare-btn"));

    await waitFor(() => {
      expect(onDecommissionSession).toHaveBeenCalledWith("prepare");
    });
    expect(screen.queryByTestId("decommission-confirm-dialog")).toBeNull();
  });

  it("reopening the modal after a cancelled confirmation starts at the choices", () => {
    renderPane();
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    fireEvent.keyDown(window, { key: "Escape" });
    fireEvent.click(screen.getByTestId("decommission-cancel-btn"));
    expect(screen.queryByTestId("decommission-modal")).toBeNull();

    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    expect(screen.getByTestId("decommission-modal")).toBeInTheDocument();
    expect(screen.queryByTestId("decommission-confirm-dialog")).toBeNull();
  });

  it("focuses the safe Cancel button when the choices open and when returning from the confirmation", () => {
    renderPane();
    expect(screen.getByTestId("decommission-cancel-btn")).toHaveFocus();

    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    expect(screen.getByTestId("decommission-confirm-cancel-btn")).toHaveFocus();
    fireEvent.click(screen.getByTestId("decommission-confirm-cancel-btn"));
    expect(screen.getByTestId("decommission-cancel-btn")).toHaveFocus();
  });

  function paneElement(
    agentName: string,
    sessionId: string,
    onDecommissionSession: (mode: "prepare" | "teardown") => Promise<unknown>,
  ) {
    return (
      <ChatPane
        agentName={agentName}
        agentDisplayName={agentName}
        adapter="claude"
        sessionId={sessionId}
        turns={[]}
        agentSettingsOpen={true}
        onDecommissionSession={onDecommissionSession as never}
      />
    );
  }

  it("switching the target agent while the confirmation is open closes everything and reopens at the choices", () => {
    const onDecommissionSession = vi.fn(async () => undefined);
    const { rerender } = render(paneElement("kacey", "sess-kacey-01", onDecommissionSession));
    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    expect(screen.getByTestId("decommission-confirm-dialog")).toBeInTheDocument();

    rerender(paneElement("ferris", "sess-ferris-01", onDecommissionSession));
    expect(screen.queryByTestId("decommission-confirm-dialog")).toBeNull();
    expect(screen.queryByTestId("decommission-modal")).toBeNull();

    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    expect(screen.getByTestId("decommission-modal")).toBeInTheDocument();
    expect(screen.queryByTestId("decommission-confirm-dialog")).toBeNull();
    expect(onDecommissionSession).not.toHaveBeenCalled();
  });

  it("keeps the confirmation open and both buttons disabled while the teardown is in flight", async () => {
    let resolveTeardown: () => void = () => undefined;
    const onDecommissionSession = vi.fn(
      () => new Promise<void>((resolve) => {
        resolveTeardown = resolve;
      }),
    );
    renderPane(onDecommissionSession as never);
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));

    await waitFor(() => {
      expect(screen.getByTestId("decommission-confirm-submit-btn")).toBeDisabled();
    });
    expect(screen.getByTestId("decommission-confirm-cancel-btn")).toBeDisabled();

    fireEvent.keyDown(window, { key: "Escape" });
    const backdrop = screen.getByTestId("decommission-confirm-dialog").parentElement as HTMLElement;
    fireEvent.click(backdrop);
    expect(screen.getByTestId("decommission-confirm-dialog")).toBeInTheDocument();

    fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));
    expect(onDecommissionSession).toHaveBeenCalledTimes(1);

    await act(async () => {
      resolveTeardown();
    });
    await waitFor(() => {
      expect(screen.queryByTestId("decommission-confirm-dialog")).toBeNull();
    });
    expect(screen.queryByTestId("decommission-modal")).toBeNull();
  });

  it("closes both dialogs and shows the error when the teardown fails", async () => {
    const onDecommissionSession = vi.fn(async () => {
      throw new Error("boom");
    });
    renderPane(onDecommissionSession);
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));

    await waitFor(() => {
      expect(screen.queryByTestId("decommission-confirm-dialog")).toBeNull();
    });
    expect(screen.queryByTestId("decommission-modal")).toBeNull();
    expect(screen.getByRole("alert")).toHaveTextContent("boom");
  });
});

describe("Resizable chat composer (Record 57 & Record 112)", () => {
  it("renders composer-resize-handle and resizes the textarea height within 36px..420px on mouse and touch drag", () => {
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
    const composer = screen.getByTestId("prompt-composer");

    expect(handle).toBeInTheDocument();
    expect(handle.style.touchAction).toBe("none");
    expect(textarea.style.height).toBe("80px");
    expect(textarea.style.minHeight).toBe("36px");
    expect(composer.style.padding).toBe("8px clamp(8px, 1.4vw, 14px)");

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

    // Drag downward beyond min (clientY: 100 -> 600 => clamped at 36px)
    fireEvent.mouseDown(handle, { clientY: 100 });
    fireEvent.mouseMove(window, { clientY: 600 });
    fireEvent.mouseUp(window);
    expect(textarea.style.height).toBe("36px");

    // Touch drag upward on iPad/iPhone (clientY: 500 -> 410 => 36 + 90 = 126px)
    fireEvent.touchStart(handle, { touches: [{ clientY: 500 }] });
    fireEvent.touchMove(window, { touches: [{ clientY: 410 }] });
    fireEvent.touchEnd(window);
    expect(textarea.style.height).toBe("126px");

    // Touch drag downward to minimum 36px
    fireEvent.touchStart(handle, { touches: [{ clientY: 300 }] });
    fireEvent.touchMove(window, { touches: [{ clientY: 600 }] });
    fireEvent.touchEnd(window);
    expect(textarea.style.height).toBe("36px");
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
    const jumpBtn = screen.getByTestId("since-last-prompt-jump-btn");
    expect(jumpBtn).toHaveClass("fd-badge");
    expect(jumpBtn.style.border).toBe("1px solid var(--border-subtle)");
    expect(jumpBtn.style.background).toBe("var(--bg-elevated)");
    expect(jumpBtn.style.borderRadius).toBe("var(--radius-sm, 4px)");
    expect(jumpBtn.style.padding).toBe("2px 8px");
    expect(jumpBtn.style.maxWidth).toBe("100%");
    expect(jumpBtn.style.overflow).toBe("hidden");
    expect(jumpBtn.style.flex).toBe("");
    fireEvent.mouseEnter(jumpBtn);
    expect(jumpBtn.style.borderColor).toBe("var(--border-accent)");
    fireEvent.mouseLeave(jumpBtn);
    expect(jumpBtn.style.borderColor).toBe("var(--border-subtle)");

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

describe("standalone transcript ownership and availability", () => {
  const history = [{ index: 0, role: "assistant", content: "Owned history", thinking: null, tool_calls: [] }];
  function deferred<T>() {
    let resolve!: (value: T) => void;
    let reject!: (error: unknown) => void;
    const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
    return { promise, resolve, reject };
  }
  function setup() {
    const reads: ReturnType<typeof deferred<import("../types/api").SessionTranscriptResponse>>[] = [];
    const client = new HubApiClient({ fetchImpl: async () => new Response("{}") });
    vi.spyOn(client, "getSessionTranscript").mockImplementation(() => {
      const read = deferred<import("../types/api").SessionTranscriptResponse>();
      reads.push(read);
      return read.promise;
    });
    const body = (content = history, extra = {}) => ({ session_id: "session-a", turns: content, ...extra });
    const pane = (sessionId: string | null = "session-a", agentName = "freya", authority: import("../types/api").HubAuthority | null = null) =>
      <ChatPane apiClient={client} agentName={agentName} adapter="claude" sessionId={sessionId} authority={authority} agentSettingsOpen />;
    return { reads, client, body, pane };
  }
  const unavailable = () => new HubApiError(503, "temporarily unavailable", undefined, undefined, { code: "transcript_unavailable" });
  const staleNotice = "History refresh unavailable; showing last loaded history";
  it.each(["503", "network", "malformed", "malformed turn"])("retains exact-session history after %s and recovers, while observed empty clears", async (failure) => {
    const { pane, reads, body } = setup();
    render(pane());
    await act(async () => reads[0].resolve(body()));
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "protected draft" } });
    fireEvent.click(screen.getByTestId("chat-refresh-btn"));
    await act(async () => {
      if (failure === "malformed") reads[1].resolve({ session_id: "session-a" } as never);
      else if (failure === "malformed turn") reads[1].resolve(body([{ ...history[0], tool_calls: "invalid" }] as never));
      else reads[1].reject(failure === "503" ? unavailable() : new TypeError("network failed"));
    });
    expect(screen.getByText(staleNotice)).toBeInTheDocument();
    expect(screen.getByText("Owned history")).toBeInTheDocument();
    expect(screen.getByLabelText("Message")).toHaveValue("protected draft");
    fireEvent.click(screen.getByTestId("chat-refresh-btn"));
    await act(async () => reads[2].resolve(body()));
    expect(screen.queryByText(staleNotice)).toBeNull();
    fireEvent.click(screen.getByTestId("chat-refresh-btn"));
    await act(async () => reads[3].resolve(body([])));
    expect(screen.queryByText("Owned history")).toBeNull();
    expect(screen.getByTestId("chat-empty-state")).toBeInTheDocument();
  });
  it("keeps a confirmed optimistic send before any successful transcript through an unavailable followup", async () => {
    const { pane, reads, client, body } = setup();
    const send = deferred<import("../types/api").PromptResponse>();
    vi.spyOn(client, "sendSessionPrompt").mockReturnValue(send.promise);
    render(pane());
    await act(async () => reads[0].reject(unavailable()));
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "Confirmed local prompt" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    expect(screen.getByTestId("optimistic-user-turn")).toHaveTextContent("Confirmed local prompt");
    await act(async () => send.resolve({ session_id: "session-a", queued: false, delivered: true } as never));
    expect(screen.queryByTestId("optimistic-user-turn")).toBeNull();
    expect(within(screen.getByTestId("turn-card-0")).getByText("Confirmed local prompt")).toBeInTheDocument();
    expect(screen.queryByTestId("chat-empty-state")).toBeNull();
    await act(async () => reads[1].reject(unavailable()));
    expect(within(screen.getByTestId("turn-card-0")).getByText("Confirmed local prompt")).toBeInTheDocument();
    expect(screen.getByText("History unavailable")).toBeInTheDocument();
    expect(screen.queryByText(staleNotice)).toBeNull();
    expect(screen.queryByTestId("chat-empty-state")).toBeNull();
    fireEvent.click(screen.getByTestId("chat-refresh-btn"));
    await act(async () => reads[2].resolve(body([])));
    expect(screen.queryByText("Confirmed local prompt")).toBeNull();
    expect(screen.getByTestId("chat-empty-state")).toBeInTheDocument();
  });
  it.each(["clear", "reforge", "teardown"] as const)("preserves confirmed optimistic rows on failed %s and discards them on success", async (mode) => {
    const { pane, reads, client } = setup();
    vi.spyOn(client, "sendSessionPrompt").mockResolvedValue({ session_id: "session-a", queued: false, delivered: true } as never);
    const reset = mode === "teardown" ? vi.spyOn(client, "decommissionSession") : vi.spyOn(client, "clearSession");
    reset.mockRejectedValueOnce(new Error("reset refused")).mockResolvedValue({} as never);
    render(pane());
    await act(async () => reads[0].reject(unavailable()));
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "Locally confirmed before reset" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    await waitFor(() => expect(reads).toHaveLength(2));
    await act(async () => reads[1].reject(unavailable()));
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "Draft survives reset" } });
    const beginReset = () => {
      if (mode === "teardown") {
        fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
        fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
        fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));
      } else {
        fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
        fireEvent.click(screen.getByTestId(`clear-modal-${mode}-btn`));
      }
    };
    beginReset();
    expect(await screen.findByText("reset refused")).toBeInTheDocument();
    expect(screen.getByTestId("turn-card-0")).toHaveTextContent("Locally confirmed before reset");
    expect(screen.getByLabelText("Message")).toHaveValue("Draft survives reset");
    expect(screen.queryByTestId("chat-empty-state")).toBeNull();
    beginReset();
    await waitFor(() => expect(reset).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(screen.queryByText("Locally confirmed before reset")).toBeNull());
    if (mode !== "teardown") {
      await waitFor(() => expect(reads).toHaveLength(3));
      await act(async () => reads[2].reject(unavailable()));
    }
    expect(screen.getByText("History unavailable")).toBeInTheDocument();
    expect(screen.getByLabelText("Message")).toHaveValue("Draft survives reset");
    expect(screen.queryByTestId("chat-empty-state")).toBeNull();
  });
  it.each(["principal", "token", "A to B to A"])("masks confirmed optimistic rows immediately after %s changes", async (change) => {
    const { pane, reads, client } = setup();
    vi.spyOn(client, "sendSessionPrompt").mockResolvedValue({ session_id: "session-a", queued: false, delivered: true } as never);
    const first = { tier: "owner", login: "first" } as const;
    const view = render(pane("session-a", "freya", first));
    await act(async () => reads[0].reject(unavailable()));
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "Private confirmed prompt" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    await waitFor(() => expect(reads).toHaveLength(2));
    await act(async () => reads[1].reject(unavailable()));
    expect(screen.getByTestId("turn-card-0")).toHaveTextContent("Private confirmed prompt");
    if (change === "token") client.setToken("replacement-token");
    if (change === "A to B to A") view.rerender(pane("session-b", "freya", first));
    view.rerender(pane("session-a", "freya", change === "principal" ? { tier: "owner", login: "second" } : first));
    expect(screen.queryByText("Private confirmed prompt")).toBeNull();
    await act(async () => reads[reads.length - 1].reject(unavailable()));
    expect(screen.queryByText("Private confirmed prompt")).toBeNull();
    expect(screen.getByText("History unavailable")).toBeInTheDocument();
  });
  it.each(["client", "principal", "token", "A to B to A"])("rejects a late send's old reader and local metadata after %s replacement", async (change) => {
    const { pane, reads, client, body } = setup();
    const replacement = setup();
    const send = deferred<import("../types/api").PromptResponse>();
    vi.spyOn(client, "sendSessionPrompt").mockReturnValue(send.promise);
    const first = { tier: "owner", login: "first" } as const;
    const view = render(pane("session-a", "freya", first));
    await act(async () => reads[0].resolve(body([])));
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "Old owner's send" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    if (change === "token") client.setToken("fixture-replacement-token");
    if (change === "A to B to A") view.rerender(pane("session-b", "freya", first));
    view.rerender(change === "client" ? replacement.pane("session-a", "freya", first) : pane(
      "session-a", "freya", change === "principal" ? { tier: "owner", login: "second" } : first,
    ));
    const currentReads = change === "client" ? replacement.reads : reads;
    const currentRead = currentReads[currentReads.length - 1];
    const oldReadCount = reads.length;
    await act(async () => send.resolve({ session_id: "session-a", queued: false, delivered: true,
      effective_body: "Stale optimistic body", is_busy: true,
      queue: [{ id: "stale-send", session_id: "session-a", prompt: "Stale send queue", by: "owner", created_at: "" }],
    } as never));
    expect(screen.queryByTestId("optimistic-user-turn")).toBeNull();
    expect(screen.queryByText("Stale send queue")).toBeNull();
    expect(screen.queryByText("Stale optimistic body")).toBeNull();
    expect(screen.getByTestId("session-busy-badge")).not.toHaveTextContent("Busy · Working");
    expect(reads).toHaveLength(oldReadCount);
    expect(screen.getByTestId("chat-refresh-btn")).toBeDisabled();
    await act(async () => currentRead.resolve(body([{ ...history[0], content: "Current owner's history" }])));
    expect(screen.getByText("Current owner's history")).toBeInTheDocument();
    expect(screen.getByTestId("chat-refresh-btn")).not.toBeDisabled();
    expect(client.sendSessionPrompt).toHaveBeenCalledTimes(1);
  });
  it("shows initial unavailable separately from observed empty history", async () => {
    const { pane, reads } = setup();
    render(pane());
    expect(screen.queryByTestId("chat-empty-state")).toBeNull();
    await act(async () => reads[0].reject(unavailable()));
    expect(screen.getByText("History unavailable")).toBeInTheDocument();
    expect(screen.queryByTestId("chat-empty-state")).toBeNull();
    expect(screen.queryByText(staleNotice)).toBeNull();
  });
  it.each([401, 403, 404])("hides last-good history on HTTP %s instead of stale success", async (status) => {
    const { pane, reads, body } = setup();
    render(pane());
    await act(async () => reads[0].resolve(body()));
    fireEvent.click(screen.getByTestId("chat-refresh-btn"));
    await act(async () => reads[1].reject(new HubApiError(status, "access refused")));
    expect(screen.queryByText("Owned history")).toBeNull();
    expect(screen.queryByText(staleNotice)).toBeNull();
    expect(screen.queryByTestId("chat-empty-state")).toBeNull();
    expect(screen.getByRole("alert")).toHaveTextContent("access refused");
  });
  it.each(["session", "agent", "principal", "token", "client"])("masks history immediately on %s change and rejects the old read's metadata and finally", async (change) => {
    const { pane, reads, body, client } = setup();
    const view = render(pane("session-a", "freya", { tier: "owner", login: "first" }));
    await act(async () => reads[0].resolve(body()));
    fireEvent.click(screen.getByTestId("chat-refresh-btn"));
    const replacement = setup();
    if (change === "token") client.setToken("fixture-new-token");
    view.rerender(change === "client" ? replacement.pane() : pane(
      change === "session" ? "session-b" : "session-a",
      change === "agent" ? "felix" : "freya",
      { tier: "owner", login: change === "principal" ? "second" : "first" },
    ));
    expect(screen.queryByText("Owned history")).toBeNull();
    await act(async () => reads[1].resolve(body(history, { is_busy: true, queue: [{ id: "old", prompt: "Old queue", session_id: "session-a", by: "owner", created_at: "" }] })));
    expect(screen.queryByText("Owned history")).toBeNull();
    expect(screen.queryByText("Old queue")).toBeNull();
    expect(screen.getByTestId("chat-refresh-btn")).toBeDisabled();
    const latest = change === "client" ? replacement.reads[0] : reads[2];
    await act(async () => latest.resolve(body([])));
    expect(screen.getByTestId("chat-refresh-btn")).not.toBeDisabled();
  });
  it("rejects a same-session response from the first A after A to B to A", async () => {
    const { pane, reads, body } = setup();
    const view = render(pane());
    view.rerender(pane("session-b"));
    view.rerender(pane());
    await act(async () => reads[2].reject(unavailable()));
    await act(async () => reads[0].resolve(body()));
    expect(screen.queryByText("Owned history")).toBeNull();
    expect(screen.getByText("History unavailable")).toBeInTheDocument();
    await act(async () => reads[1].reject(new Error("old failure")));
    expect(screen.queryByText("old failure")).toBeNull();
  });
  it.each([false, true])("requires positive alias resolution for reusable last-good (resolved=%s)", async (resolved) => {
    const { pane, reads, body } = setup();
    render(pane(null));
    await act(async () => reads[0].resolve(body(history, resolved ? { resolved_session_id: "session-a" } : {})));
    expect(screen.getByText("Owned history")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("chat-refresh-btn"));
    await act(async () => reads[1].reject(unavailable()));
    expect(Boolean(screen.queryByText("Owned history"))).toBe(resolved);
    expect(Boolean(screen.queryByText(staleNotice))).toBe(resolved);
  });
  it("does not reuse a concrete snapshot after switching to an unknown alias or accept contradictory resolution", async () => {
    const { pane, reads, body } = setup();
    const view = render(pane());
    await act(async () => reads[0].resolve(body()));
    view.rerender(pane(null));
    expect(screen.queryByText("Owned history")).toBeNull();
    await act(async () => reads[1].reject(unavailable()));
    expect(screen.queryByText("Owned history")).toBeNull();
    view.rerender(pane());
    await act(async () => reads[2].resolve(body(history, { resolved_session_id: "session-b" })));
    expect(screen.queryByText("Owned history")).toBeNull();
    expect(screen.getByText("History unavailable")).toBeInTheDocument();
  });
  it.each(["clear", "reforge"] as const)("fences a pending read during %s, preserves history on reset failure, discards on success", async (mode) => {
    const { pane, reads, body, client } = setup();
    const reset = deferred<never>();
    vi.spyOn(client, "clearSession").mockReturnValueOnce(reset.promise).mockResolvedValue({} as never);
    render(pane());
    await act(async () => reads[0].resolve(body()));
    fireEvent.click(screen.getByTestId("chat-refresh-btn"));
    fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
    fireEvent.click(screen.getByTestId(`clear-modal-${mode}-btn`));
    await act(async () => reads[1].resolve(body([{ ...history[0], content: "pre-reset late result" }])));
    expect(screen.queryByText("pre-reset late result")).toBeNull();
    await act(async () => reset.reject(new Error("reset failed")));
    expect(screen.getByText("Owned history")).toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent("reset failed");
    fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
    fireEvent.click(screen.getByTestId(`clear-modal-${mode}-btn`));
    await waitFor(() => expect(client.clearSession).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(reads).toHaveLength(3));
    expect(screen.queryByText("Owned history")).toBeNull();
    await act(async () => reads[reads.length - 1].resolve(body([])));
    expect(screen.getByTestId("chat-empty-state")).toBeInTheDocument();
  });
  it.each(["newer success", "newer failure", "newer still loading"])("rejects overlapping old results after %s, including metadata and finally", async (outcome) => {
    const { pane, reads, body, client } = setup();
    vi.spyOn(client, "sendSessionPrompt").mockResolvedValue({ session_id: "session-a", queued: true } as never);
    render(pane());
    await act(async () => reads[0].resolve(body()));
    fireEvent.click(screen.getByTestId("chat-refresh-btn"));
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "queued exactly once" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    await waitFor(() => expect(reads).toHaveLength(3));
    if (outcome === "newer success") await act(async () => reads[2].resolve(body([{ ...history[0], content: "New observation" }])));
    if (outcome === "newer failure") await act(async () => reads[2].reject(unavailable()));
    await act(async () => reads[1].resolve(body([{ ...history[0], content: "Old observation" }], { is_busy: true, queue: [{ id: "old", prompt: "Old queue", session_id: "session-a", by: "owner", created_at: "" }] })));
    expect(screen.queryByText("Old observation")).toBeNull();
    expect(screen.queryByText("Old queue")).toBeNull();
    if (outcome === "newer success") expect(screen.getByText("New observation")).toBeInTheDocument();
    if (outcome === "newer failure") expect(screen.getByText(staleNotice)).toBeInTheDocument();
    expect(screen.getByTestId("chat-refresh-btn").hasAttribute("disabled")).toBe(outcome === "newer still loading");
    expect(client.sendSessionPrompt).toHaveBeenCalledTimes(1);
  });
  it.each(["success", "failure"])("fences pending reads on teardown %s and allows recovery after refusal", async (outcome) => {
    const { pane, reads, body, client } = setup();
    const reset = deferred<never>();
    vi.spyOn(client, "decommissionSession").mockReturnValue(reset.promise);
    render(pane());
    await act(async () => reads[0].resolve(body()));
    fireEvent.click(screen.getByTestId("chat-refresh-btn"));
    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));
    await act(async () => reads[1].resolve(body([{ ...history[0], content: "Pre-teardown result" }])));
    expect(screen.queryByText("Pre-teardown result")).toBeNull();
    await act(async () => outcome === "success" ? reset.resolve({} as never) : reset.reject(new Error("teardown refused")));
    expect(Boolean(screen.queryByText("Owned history"))).toBe(outcome === "failure");
    if (outcome === "failure") expect(screen.getByRole("alert")).toHaveTextContent("teardown refused");
    fireEvent.click(screen.getByTestId("chat-refresh-btn"));
    await act(async () => reads[2].resolve(body([])));
    expect(screen.getByTestId("chat-empty-state")).toBeInTheDocument();
  });
  it("does not resurrect successful reset history when the post-reset read is unavailable", async () => {
    const { pane, reads, body, client } = setup();
    vi.spyOn(client, "clearSession").mockResolvedValue({} as never);
    render(pane());
    await act(async () => reads[0].resolve(body()));
    fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
    fireEvent.click(screen.getByTestId("clear-modal-clear-btn"));
    await waitFor(() => expect(reads).toHaveLength(2));
    await act(async () => reads[1].reject(unavailable()));
    expect(screen.queryByText("Owned history")).toBeNull();
    expect(screen.queryByText(staleNotice)).toBeNull();
    expect(screen.getByText("History unavailable")).toBeInTheDocument();
  });
  it.each([false, true])("shows independently observed send queue/busy before transcript success (controlled=%s)", async (controlled) => {
    const { client, reads } = setup();
    vi.spyOn(client, "sendSessionPrompt").mockResolvedValue({ session_id: "session-a", queued: true, is_busy: true,
      queue: [{ id: "independent", session_id: "session-a", prompt: "Independently observed queue", by: "owner", created_at: "" }],
    } as never);
    render(<ChatPane agentName="freya" sessionId="session-a" apiClient={client} adapter="claude" {...(controlled ? { turns: [] } : {})} />);
    if (!controlled) await act(async () => reads[0].reject(unavailable()));
    fireEvent.change(screen.getByLabelText("Message"), { target: { value: "queued submission" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    expect(await screen.findByText("Independently observed queue")).toBeInTheDocument();
    expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Busy · Working");
    if (!controlled) {
      await act(async () => reads[1].reject(unavailable()));
      expect(screen.getByText("Independently observed queue")).toBeInTheDocument();
      expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Busy · Working");
    }
  });
  it("delegates controlled reset once without issuing the direct reset", async () => {
    const { client } = setup();
    const reset = vi.fn(async () => undefined);
    const direct = vi.spyOn(client, "clearSession");
    render(<ChatPane agentName="freya" sessionId="session-a" turns={history} apiClient={client} onResetSession={reset} transcriptRefreshState="stale" agentSettingsOpen />);
    expect(screen.getByText(staleNotice)).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
    fireEvent.click(screen.getByTestId("clear-modal-clear-btn"));
    await waitFor(() => expect(reset).toHaveBeenCalledWith("session-a", "clear"));
    expect(reset).toHaveBeenCalledTimes(1);
    expect(direct).not.toHaveBeenCalled();
  });
});
