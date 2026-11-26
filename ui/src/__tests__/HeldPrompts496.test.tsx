import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { HubApiClient, HubApiError } from "../api/client";
import { ChatPane, isComposerHeldPrompt, sessionDraftMemory } from "../components/ChatPane";
import { parkedPromptDraftScope, readParkedPromptDrafts, writeParkedPromptDraft, type ParkedPromptDraft } from "../lib/parkedPromptDrafts";
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

  it.each([
    ["session_busy", "the session is busy"],
    ["composer_unlocated", "the session composer could not be located"],
  ])("shows the specific %s reason on the removable queue item", (hold_reason, detail) => {
    render(<ChatPane sessionId="claude-619-reason" turns={[]} queue={[heldItem({ hold_reason })]} />);
    expect(screen.getByTestId("queued-hold-reason-held-1")).toHaveTextContent(detail);
    expect(screen.getByTestId("queued-cancel-held-1")).toBeEnabled();
  });

  it.each(["hub restart", "conversation replacement"] as const)(
    "preserves a parked draft after a %s without sending it again", async (change) => {
      const target = `claude-619-${change}`;
      const nextTarget = change === "hub restart" ? target : `${target}-new`;
      const initial = "Draft parked before the change";
      const savedFile = { name: "saved.txt", media_type: "text/plain",
        url: "/api/v1/attachments/saved.txt", path: "/data/attachments/saved.txt" };
      const item = heldItem({ id: target, session_id: target, prompt: initial,
        hold_reason: "session_busy", claude_terminal: true, attachments: [savedFile] });
      let epoch = "original-hub";
      let queue: QueuedOperatorPrompt[] = [];
      const client = new HubApiClient({ fetchImpl: async () => new Response("{}") });
      vi.spyOn(client, "getSessionTranscript").mockImplementation(async (session_id) => ({
        session_id, turns: [], queue_epoch: epoch, queue: session_id === target ? queue : [],
      }));
      const readQueue = vi.spyOn(client, "getSessionQueue").mockImplementation(async (session_id) => ({
        session_id, is_busy: false, queue_epoch: epoch, queue: session_id === target ? queue : [],
      }));
      const send = vi.spyOn(client, "sendSessionPrompt").mockImplementation(async () => {
        queue = [item];
        return { session_id: target, queue_epoch: epoch, queued: true, status: "queued",
          delivery_stage: "held", hold_reason: "session_busy", queued_prompt: item, queue };
      });
      const cancel = vi.spyOn(client, "cancelQueuedPrompt").mockImplementation(async (session_id, prompt_id) => {
        queue = [];
        return { session_id, prompt_id, status: "cancelled", queue };
      });
      const readFile = vi.spyOn(client, "getQueuedPromptAttachment").mockResolvedValue(new Blob(["saved"]));
      const props = { agentName: target, adapter: "claude", sessionId: target, apiClient: client,
        authority: { tier: "owner", login: "owner" } as const };
      const { rerender } = render(<ChatPane {...props} wsConnected />);
      const composer = screen.getByLabelText("Message");
      fireEvent.change(composer, { target: { value: initial } });
      fireEvent.change(screen.getByTestId("prompt-file-input"), {
        target: { files: [new File(["original"], savedFile.name, { type: savedFile.media_type })] },
      });
      expect(await screen.findByTestId("attachment-chip-list")).toHaveTextContent(savedFile.name);
      fireEvent.click(screen.getByRole("button", { name: "Send Message" }));
      expect(await screen.findByTestId(`queued-hold-reason-${target}`)).toHaveTextContent("the session is busy");
      expect(composer).toHaveValue("");
      expect(screen.queryByTestId("optimistic-user-turn")).not.toBeInTheDocument();
      await waitFor(() => expect(readQueue).toHaveBeenCalledWith(target));

      const edited = "Edited text retained by the original queue";
      if (change === "hub restart") {
        epoch = "replacement-hub";
        queue = [];
      } else {
        queue = [{ ...item, prompt: edited, hold_reason: null, delivery_state: "refused", refusal_reason: "session_replaced" }];
      }
      rerender(<ChatPane {...props} sessionId={nextTarget} wsConnected={false} />);
      const restore = await screen.findByRole("button", { name: "Restore draft" });
      expect(screen.getByText(change === "hub restart" ? /Hub restarted/ : /original conversation changed/)).toBeVisible();
      fireEvent.change(composer, { target: { value: "A new draft" } });
      expect(restore).toBeDisabled();
      expect(composer).toHaveValue("A new draft");
      fireEvent.change(composer, { target: { value: "" } });
      fireEvent.click(restore);
      await waitFor(() => expect(composer).toHaveValue(change === "hub restart" ? initial : edited));
      expect(readFile).toHaveBeenCalledWith(savedFile.url);
      expect(screen.getByTestId("attachment-chip-list")).toHaveTextContent(savedFile.name);
      expect(send).toHaveBeenCalledTimes(1);
      if (change === "hub restart") expect(cancel).not.toHaveBeenCalled();
      else expect(cancel).toHaveBeenCalledWith(target, item.id);
    },
  );

  it("keeps the draft when the queue is full", async () => {
    const target = "claude-619-full";
    const client = new HubApiClient({ fetchImpl: async () => new Response("{}") });
    vi.spyOn(client, "getSessionTranscript").mockResolvedValue({ session_id: target, turns: [] });
    vi.spyOn(client, "sendSessionPrompt").mockRejectedValue(new HubApiError(409,
      "The prompt queue is full. Remove an item or wait for delivery.", undefined, undefined, { refusal_reason: "queue_full" }));
    render(<ChatPane sessionId={target} adapter="claude" apiClient={client} />);
    const composer = screen.getByLabelText("Message");
    fireEvent.change(composer, { target: { value: "Keep this draft" } });
    fireEvent.click(screen.getByRole("button", { name: "Send Message" }));
    expect(await screen.findByText(/prompt queue is full/)).toBeVisible();
    expect(composer).toHaveValue("Keep this draft");
  });

  it.each(["", "New draft"])("keeps an unconfirmed Claude POST out of the composer when it contains '%s'", async (newDraft) => {
    const target = `claude-619-lost-post-${newDraft.length}`;
    const client = new HubApiClient({ fetchImpl: async () => new Response("{}") });
    vi.spyOn(client, "getSessionTranscript").mockResolvedValue({ session_id: target, turns: [] });
    let loseReply!: (error: TypeError) => void;
    const send = vi.spyOn(client, "sendSessionPrompt").mockReturnValue(
      new Promise((_, reject) => { loseReply = reject; }),
    );
    render(<ChatPane sessionId={target} adapter="claude" apiClient={client} authority={{ tier: "owner", login: "owner" }} />);
    const composer = screen.getByLabelText("Message");
    fireEvent.change(composer, { target: { value: "Original submission" } });
    fireEvent.click(screen.getByRole("button", { name: "Send Message" }));
    if (newDraft) fireEvent.change(composer, { target: { value: newDraft } });
    await act(async () => loseReply(new TypeError("Lost HTTP reply")));

    expect(await screen.findByText(/Check the session and queue before resending/)).toBeVisible();
    expect(composer).toHaveValue(newDraft);
    const restore = screen.getByRole("button", { name: "Restore draft" });
    if (newDraft) {
      expect(restore).toBeDisabled();
      fireEvent.change(composer, { target: { value: "" } });
    }
    fireEvent.click(restore);
    await waitFor(() => expect(composer).toHaveValue("Original submission"));
    expect(send).toHaveBeenCalledTimes(1);
  });

  it("keeps a refused submission and its file when another draft is already open", async () => {
    const target = "claude-619-delayed-refusal";
    const client = new HubApiClient({ fetchImpl: async () => new Response("{}") });
    vi.spyOn(client, "getSessionTranscript").mockResolvedValue({ session_id: target, turns: [] });
    let refuse!: (error: HubApiError) => void;
    const send = vi.spyOn(client, "sendSessionPrompt").mockReturnValue(
      new Promise((_, reject) => { refuse = reject; }),
    );
    render(<ChatPane sessionId={target} adapter="claude" apiClient={client} authority={{ tier: "owner", login: "owner" }} />);
    const composer = screen.getByLabelText("Message");
    fireEvent.change(composer, { target: { value: "First draft" } });
    fireEvent.change(screen.getByTestId("prompt-file-input"), {
      target: { files: [new File(["original"], "original.txt", { type: "text/plain" })] },
    });
    expect(await screen.findByTestId("attachment-chip-list")).toHaveTextContent("original.txt");
    fireEvent.click(screen.getByRole("button", { name: "Send Message" }));
    fireEvent.change(composer, { target: { value: "Second draft" } });
    await act(async () => refuse(new HubApiError(409, "The prompt queue is full.")));

    const restore = await screen.findByRole("button", { name: "Restore draft" });
    expect(restore).toBeDisabled();
    expect(composer).toHaveValue("Second draft");
    fireEvent.change(composer, { target: { value: "" } });
    fireEvent.click(restore);
    await waitFor(() => expect(composer).toHaveValue("First draft"));
    expect(screen.getByTestId("attachment-chip-list")).toHaveTextContent("original.txt");
    expect(send).toHaveBeenCalledTimes(1);
  });

  it("keeps a restored draft private across principal and Hub switches at the same target", async () => {
    const target = "claude-619-private-restore";
    const scope = parkedPromptDraftScope("", "owner", target);
    window.localStorage.setItem(`${scope}old.private`, JSON.stringify({
      id: "private", sessionId: target, epoch: "old", text: "Owner draft", queuedText: "Owner draft",
      files: [{ name: "owner.txt", media_type: "text/plain", url: "/api/v1/attachments/owner.txt" }],
      recovery: "hub_restarted",
    }));
    const owner = { tier: "owner", login: "owner" } as const;
    const client = new HubApiClient({ fetchImpl: async () => new Response("{}") });
    const otherHub = new HubApiClient({ baseUrl: "https://other.example", fetchImpl: async () => new Response("{}") });
    const readFile = vi.spyOn(client, "getQueuedPromptAttachment").mockResolvedValue(new Blob(["private"]));
    const props = { sessionId: target, adapter: "claude", apiClient: client, turns: [] };
    const { rerender } = render(<ChatPane {...props} authority={owner} />);

    fireEvent.click(await screen.findByRole("button", { name: "Restore draft" }));
    const composer = screen.getByLabelText("Message");
    await waitFor(() => expect(composer).toHaveValue("Owner draft"));
    expect(screen.getByTestId("attachment-chip-list")).toHaveTextContent("owner.txt");
    expect(readFile).toHaveBeenCalledWith("/api/v1/attachments/owner.txt");

    rerender(<ChatPane {...props} authority={{ tier: "peer", agent: "peer" }} />);
    expect(composer).toHaveValue("");
    expect(screen.queryByTestId("attachment-chip-list")).not.toBeInTheDocument();
    rerender(<ChatPane {...props} apiClient={otherHub} authority={owner} />);
    expect(composer).toHaveValue("");
    expect(screen.queryByTestId("attachment-chip-list")).not.toBeInTheDocument();
    rerender(<ChatPane {...props} authority={owner} />);
    expect(composer).toHaveValue("Owner draft");
    expect(screen.getByTestId("attachment-chip-list")).toHaveTextContent("owner.txt");
  });

  it("bounds attachment bytes across locally refused submissions while retaining their draft text", () => {
    const scope = parkedPromptDraftScope("", "owner", "claude-619-capped-drafts");
    const data_base64 = "A".repeat(12 * 1024 * 1024 + 1);
    const first: ParkedPromptDraft = {
      id: "first", sessionId: "target", epoch: "local", text: "First", queuedText: "First",
      files: [], recovery: "submission_refused", attachmentCount: 1,
      attachments: [{ name: "first.txt", media_type: "text/plain", data_base64 }],
    };
    const second = { ...first, id: "second", text: "Second" };
    try {
      writeParkedPromptDraft(scope, first);
      writeParkedPromptDraft(scope, second);
      const drafts = readParkedPromptDrafts(scope);
      expect(drafts.find((draft) => draft.id === "first")?.attachments).toHaveLength(1);
      expect(drafts.find((draft) => draft.id === "second")).toMatchObject({ text: "Second", attachmentCount: 1 });
      expect(drafts.find((draft) => draft.id === "second")?.attachments).toBeUndefined();
    } finally {
      writeParkedPromptDraft(scope, first, true);
      writeParkedPromptDraft(scope, second, true);
    }
  });

  it("restores persisted draft files where available and identifies that files need reattaching", async () => {
    const target = "claude-619-reopened";
    const files = ["saved.txt", "expired.txt"].map((name) => ({
      name, media_type: "text/plain", url: `/api/v1/attachments/${name}`, path: name,
    }));
    const scope = parkedPromptDraftScope("", "owner", target);
    window.localStorage.setItem(`${scope}old.reopened`, JSON.stringify({ id: "reopened", sessionId: target,
      epoch: "old", text: "A draft from a previous page", queuedText: "A draft from a previous page", files }));
    const client = new HubApiClient({ fetchImpl: async () => new Response("{}") });
    vi.spyOn(client, "getSessionTranscript").mockResolvedValue({ session_id: target, turns: [] });
    vi.spyOn(client, "getSessionQueue").mockResolvedValue({ session_id: target, is_busy: false, queue: [], queue_epoch: "new" });
    const readFile = vi.spyOn(client, "getQueuedPromptAttachment")
      .mockResolvedValueOnce(new Blob(["contents"], { type: "text/plain" }))
      .mockRejectedValueOnce(new HubApiError(404, "Attachment expired"));
    render(<ChatPane agentName={target} sessionId={target} apiClient={client} authority={{ tier: "owner", login: "owner" }} />);
    fireEvent.click(await screen.findByRole("button", { name: "Restore draft" }));
    expect(await screen.findByText(/Reattach any unavailable files/)).toBeVisible();
    expect(screen.getByLabelText("Message")).toHaveValue("A draft from a previous page");
    expect(screen.getByTestId("attachment-chip-list")).toHaveTextContent("saved.txt");
    expect(readFile.mock.calls).toEqual(files.map((file) => [file.url]));
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
