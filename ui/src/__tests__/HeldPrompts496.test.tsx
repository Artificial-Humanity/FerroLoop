import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import { ChatPane, isComposerHeldPrompt, sessionDraftMemory } from "../components/ChatPane";
import type { QueuedOperatorPrompt, SessionTranscriptResponse } from "../types/api";

const HOLD_REASON =
  "composer_occupied_by_non_wire_input: composer holds non-wire input or active dialog; holding delivery without sending keys";

function heldItem(overrides: Partial<QueuedOperatorPrompt> = {}): QueuedOperatorPrompt {
  return {
    id: "held-1",
    session_id: "sess-kayla-496",
    persona: "kayla",
    prompt: "Please review the rollout plan",
    effective_body: "Please review the rollout plan",
    by: "owner",
    created_at: "2026-11-10T20:00:00Z",
    delivery_state: "pending",
    delivery_error: HOLD_REASON,
    hold_reason: "composer_occupied",
    ...overrides,
  };
}

describe("held operator prompts (#496 defect 1)", () => {
  beforeEach(() => {
    sessionDraftMemory.clear();
    window.localStorage.clear();
  });

  it("detects composer-held prompts by their structured hold_reason, not error text", () => {
    expect(isComposerHeldPrompt(heldItem())).toBe(true);
    expect(isComposerHeldPrompt(heldItem({ hold_reason: "composer_unlocated" }))).toBe(true);
    expect(isComposerHeldPrompt(heldItem({ hold_reason: "session_busy" }))).toBe(true);
    expect(isComposerHeldPrompt(heldItem({ hold_reason: "composer_occupied_zero_keys" }))).toBe(false);
    // Post-paste error shares the wire prefix but is not a zero-key hold (B1).
    expect(isComposerHeldPrompt(heldItem({ hold_reason: null, delivery_error: "composer_occupied_by_non_wire_input: composer holds non-wire input after paste; aborting Enter retries" }))).toBe(false);
    expect(isComposerHeldPrompt(heldItem({ hold_reason: null, delivery_error: null }))).toBe(false);
  });

  it("restores a direct Claude draft and adopts the held reply's session state", async () => {
    const target = "sess-kayla-direct-hold";
    const draft = "  Please review the next draft  ";
    const effectiveBody = "Please review the next draft";
    const queue = [heldItem({ id: "other-held", session_id: target })];
    const client = new HubApiClient({ fetchImpl: async () => new Response("{}") });
    let resolveRefresh!: (response: SessionTranscriptResponse) => void;
    const refresh = new Promise<SessionTranscriptResponse>((resolve) => { resolveRefresh = resolve; });
    vi.spyOn(client, "getSessionTranscript")
      .mockResolvedValueOnce({ session_id: target, turns: [] })
      .mockReturnValueOnce(refresh);
    vi.spyOn(client, "sendSessionPrompt").mockResolvedValue({
      session_id: target,
      status: "sent",
      queued: false,
      is_busy: true,
      delivered: false,
      delivery_stage: "held",
      delivery_error: HOLD_REASON,
      prompt: effectiveBody,
      effective_body: effectiveBody,
      queue,
    });
    render(<ChatPane agentName="kayla" adapter="claude" sessionId={target} apiClient={client} />);
    await screen.findByTestId("chat-empty-state");

    const composer = screen.getByLabelText("Message");
    fireEvent.change(composer, { target: { value: draft } });
    fireEvent.click(screen.getByRole("button", { name: "Send Message" }));

    expect(await screen.findByText(/Nothing was sent or queued/)).toBeInTheDocument();
    expect(composer).toHaveValue(draft);
    expect(screen.queryByTestId("optimistic-user-turn")).not.toBeInTheDocument();
    expect(screen.queryByText(effectiveBody, { ignore: "textarea" })).not.toBeInTheDocument();
    expect(screen.getByTestId("queued-prompt-item-other-held")).toBeInTheDocument();
    expect(screen.getByTestId("session-busy-badge")).toHaveTextContent("Busy");
    await act(async () => resolveRefresh({
      session_id: target,
      turns: [{ index: 0, role: "assistant", content: "Latest session history", thinking: null, tool_calls: [] }],
    }));
    expect(screen.getByText("Latest session history")).toBeInTheDocument();
    expect(screen.queryByText(effectiveBody, { ignore: "textarea" })).not.toBeInTheDocument();
  });

  it("does not promise automatic retry for a held queue item that requires Send Now", async () => {
    const target = "sess-kayla-manual-hold";
    const queue = [heldItem({
      id: "manual-hold",
      session_id: target,
      prompt: "Queued text awaiting delivery",
      effective_body: "Queued text awaiting delivery",
      retries_exhausted: true,
    })];
    const client = new HubApiClient({ fetchImpl: async () => new Response("{}") });
    vi.spyOn(client, "getSessionTranscript").mockResolvedValue({ session_id: target, turns: [], queue });
    vi.spyOn(client, "sendQueuedPromptNow").mockResolvedValue({
      session_id: target,
      status: "delivery_failed",
      queued: true,
      is_busy: true,
      delivered: false,
      delivery_stage: "held",
      delivery_error: HOLD_REASON,
      prompt: "Queued text awaiting delivery",
      effective_body: "Queued text awaiting delivery",
      queue,
    });
    render(<ChatPane agentName="kayla" adapter="claude" sessionId={target} apiClient={client} />);

    fireEvent.click(await screen.findByTestId("queued-send-now-manual-hold"));
    expect(await screen.findByText("Held: the session composer is occupied, so nothing was sent.")).toBeInTheDocument();
    expect(screen.getByTestId("queued-hold-reason-manual-hold")).toHaveTextContent(/use Send Now/);
    expect(screen.queryByText(/will retry automatically/)).not.toBeInTheDocument();
    const queuedText = screen.getByText("Queued text awaiting delivery");
    expect(screen.getByTestId("queued-prompt-item-manual-hold")).toContainElement(queuedText);
  });

  it("shows the owner's text and sender with the hold reason as a separate status line", () => {
    render(
      <ChatPane
        agentName="kayla"
        sessionId="sess-kayla-496"
        turns={[]}
        isBusy={false}
        queue={[heldItem()]}
        onSendQueuedNow={vi.fn()}
        onCancelQueued={vi.fn()}
      />,
    );

    const item = screen.getByTestId("queued-prompt-item-held-1");
    const body = screen.getByText("Please review the rollout plan");
    expect(body).toBeInTheDocument();
    expect(body.textContent).not.toContain("composer_occupied_by_non_wire_input");

    expect(screen.getByTestId("queued-prompt-sender-held-1")).toHaveTextContent("by owner");

    const holdLine = screen.getByTestId("queued-hold-reason-held-1");
    expect(holdLine).toHaveAttribute("role", "status");
    // Human-readable hold text, not the raw internal error string (nit).
    expect(holdLine).toHaveTextContent(/Held: the session composer is occupied/);
    expect(holdLine.textContent).not.toContain("composer_occupied_by_non_wire_input");
    expect(body.contains(holdLine)).toBe(false);
    expect(holdLine.contains(body)).toBe(false);
    expect(item.textContent).not.toContain("Delivery unconfirmed");
  });

  it("always enables Delete for a held prompt, even when busy or marked unconfirmed", async () => {
    const onCancelQueued = vi.fn(async () => {});
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    const { rerender } = render(
      <ChatPane
        agentName="kayla"
        sessionId="sess-kayla-del-busy"
        turns={[]}
        isBusy
        queue={[heldItem({ id: "held-busy", session_id: "sess-kayla-del-busy" })]}
        onSendQueuedNow={vi.fn()}
        onCancelQueued={onCancelQueued}
      />,
    );
    expect(screen.getByTestId("queued-cancel-held-busy")).toBeEnabled();

    rerender(
      <ChatPane
        agentName="kayla"
        sessionId="sess-kayla-del-busy"
        turns={[]}
        isBusy={false}
        queue={[heldItem({
          id: "held-busy",
          session_id: "sess-kayla-del-busy",
          delivery_state: "unconfirmed",
        })]}
        onSendQueuedNow={vi.fn()}
        onCancelQueued={onCancelQueued}
      />,
    );
    const cancel = screen.getByTestId("queued-cancel-held-busy");
    expect(cancel).toBeEnabled();
    fireEvent.click(cancel);
    await waitFor(() => expect(onCancelQueued).toHaveBeenCalledWith("held-busy"));
    // Held prompts are known not to have landed, so no unconfirmed-removal confirmation.
    expect(confirm).not.toHaveBeenCalled();
    confirm.mockRestore();
  });

  it("enables Send now when idle with an empty composer and re-runs it through the send-now path", async () => {
    const onSendQueuedNow = vi.fn(async () => {});
    render(
      <ChatPane
        agentName="kayla"
        sessionId="sess-kayla-send-idle"
        turns={[]}
        isBusy={false}
        queue={[heldItem({ id: "held-idle", session_id: "sess-kayla-send-idle" })]}
        onSendQueuedNow={onSendQueuedNow}
        onCancelQueued={vi.fn()}
      />,
    );
    const sendNow = screen.getByTestId("queued-send-now-held-idle");
    expect(sendNow).toBeEnabled();
    fireEvent.click(sendNow);
    await waitFor(() => expect(onSendQueuedNow).toHaveBeenCalledWith("held-idle"));
  });

  it("disables Send now for a held prompt when the session is busy", () => {
    render(
      <ChatPane
        agentName="kayla"
        sessionId="sess-kayla-send-busy"
        turns={[]}
        isBusy
        queue={[heldItem({ id: "held-busy-send", session_id: "sess-kayla-send-busy" })]}
        onSendQueuedNow={vi.fn()}
        onCancelQueued={vi.fn()}
      />,
    );
    expect(screen.getByTestId("queued-send-now-held-busy-send")).toBeDisabled();
    // Delete stays enabled while Send now is gated.
    expect(screen.getByTestId("queued-cancel-held-busy-send")).toBeEnabled();
  });

  it("disables Send now for a held prompt while the composer holds text, and re-enables when cleared", () => {
    render(
      <ChatPane
        agentName="kayla"
        sessionId="sess-kayla-send-composer"
        turns={[]}
        isBusy={false}
        queue={[heldItem({ id: "held-composer", session_id: "sess-kayla-send-composer" })]}
        onSendQueuedNow={vi.fn()}
        onCancelQueued={vi.fn()}
      />,
    );
    const sendNow = screen.getByTestId("queued-send-now-held-composer");
    expect(sendNow).toBeEnabled();

    const composer = screen.getByLabelText("Message");
    fireEvent.change(composer, { target: { value: "draft in progress" } });
    expect(sendNow).toBeDisabled();

    fireEvent.change(composer, { target: { value: "" } });
    expect(sendNow).toBeEnabled();
  });

  it("S7: Delete is disabled while a non-held prompt is sending", () => {
    render(
      <ChatPane agentName="kayla" sessionId="sess-kayla-sending" turns={[]} isBusy={false}
        queue={[heldItem({ id: "sending-1", session_id: "sess-kayla-sending", hold_reason: null, delivery_error: null, delivery_state: "sending" })]}
        onSendQueuedNow={vi.fn()} onCancelQueued={vi.fn()} />,
    );
    expect(screen.getByTestId("queued-cancel-sending-1")).toBeDisabled();
    expect(screen.getByTestId("queued-send-now-sending-1")).toBeDisabled();
  });

  it("S7: lost-reply fence does not mask a held prompt and Send Now still works", async () => {
    const onSendQueuedNow = vi.fn(async () => { throw new TypeError("lost reply"); });
    const props = {
      agentName: "kayla", adapter: "codex" as const, turns: [] as never[], isBusy: false,
      queue: [heldItem({ id: "lost-held", session_id: "sess-kayla-lost" })],
      onSendQueuedNow, onCancelQueued: vi.fn(),
    };
    const { rerender } = render(<ChatPane sessionId="sess-kayla-lost" {...props} />);
    fireEvent.click(screen.getByTestId("queued-send-now-lost-held"));
    await waitFor(() => expect(onSendQueuedNow).toHaveBeenCalledTimes(1));
    rerender(<ChatPane sessionId="sess-kayla-lost" {...props} />);
    const item = screen.getByTestId("queued-prompt-item-lost-held");
    expect(within(item).queryByText(/Delivery unconfirmed/)).toBeNull();
    expect(screen.getByTestId("queued-hold-reason-lost-held")).toBeInTheDocument();
    const sendNow = screen.getByTestId("queued-send-now-lost-held");
    expect(sendNow).toBeEnabled();
    fireEvent.click(sendNow);
    await waitFor(() => expect(onSendQueuedNow).toHaveBeenCalledTimes(2));
  });

  it("S5: retries-exhausted held prompt says retries stopped", () => {
    render(
      <ChatPane agentName="kayla" sessionId="sess-kayla-exhausted" turns={[]} isBusy={false}
        queue={[heldItem({ id: "exh-1", session_id: "sess-kayla-exhausted", retries_exhausted: true })]}
        onSendQueuedNow={vi.fn()} onCancelQueued={vi.fn()} />,
    );
    expect(screen.getByTestId("queued-hold-reason-exh-1")).toHaveTextContent(/Automatic retries have stopped/);
    expect(screen.getByTestId("queued-retries-exhausted")).toHaveTextContent(/blocked behind it/);
  });

  it("keeps non-held queue behaviour unchanged while busy", async () => {
    const onSendQueuedNow = vi.fn(async () => {});
    render(
      <ChatPane
        agentName="kayla"
        sessionId="sess-kayla-plain"
        turns={[]}
        isBusy
        queue={[{
          id: "plain-1",
          session_id: "sess-kayla-plain",
          prompt: "Ordinary queued prompt",
          by: "owner",
          created_at: "2026-11-10T20:00:00Z",
          delivery_state: "pending",
        }]}
        onSendQueuedNow={onSendQueuedNow}
        onCancelQueued={vi.fn()}
      />,
    );
    expect(screen.queryByTestId("queued-hold-reason-plain-1")).toBeNull();
    const sendNow = screen.getByTestId("queued-send-now-plain-1");
    expect(sendNow).toBeEnabled();
    fireEvent.click(sendNow);
    await waitFor(() => expect(onSendQueuedNow).toHaveBeenCalledWith("plain-1"));
  });
});
