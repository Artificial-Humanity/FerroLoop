import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ChatPane, isComposerHeldPrompt, sessionDraftMemory } from "../components/ChatPane";
import type { QueuedOperatorPrompt } from "../types/api";

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
    ...overrides,
  };
}

describe("held operator prompts (#496 defect 1)", () => {
  beforeEach(() => {
    sessionDraftMemory.clear();
    window.localStorage.clear();
  });

  it("detects composer-held prompts by their hold reason", () => {
    expect(isComposerHeldPrompt(heldItem())).toBe(true);
    expect(isComposerHeldPrompt(heldItem({ delivery_error: "receiver unavailable" }))).toBe(false);
    expect(isComposerHeldPrompt(heldItem({ delivery_error: null }))).toBe(false);
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
    expect(holdLine).toHaveTextContent(HOLD_REASON);
    // Separate line: the hold reason is not inside the message body element.
    expect(body.contains(holdLine)).toBe(false);
    expect(holdLine.contains(body)).toBe(false);
    expect(within(item).getByText(HOLD_REASON)).toBe(holdLine);
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
