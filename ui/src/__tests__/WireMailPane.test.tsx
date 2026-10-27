import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { WireMailPane } from "../components/WireMailPane";
import { DEFAULT_WORKSPACE_AGENTS, type WireInboxItem } from "../types/api";

const SAMPLE_MESSAGES: WireInboxItem[] = [
  {
    message: {
      id: "msg-001",
      thread_id: "thr-001",
      sender_name: "jarvis",
      to: "ferris",
      kind: "mail",
      urgency: "urgent",
      durable: true,
      subject: "GPU arbitration window starting",
      body: "Please pause heavy verification jobs on ai-lab-0 for 20 minutes.",
      created_at: "2026-10-27T04:15:00Z",
    },
    receipt: {
      message_id: "msg-001",
      recipient_id: "urn:fl:agent:ferris",
      recipient_name: "ferris",
      status: "delivered",
      attempts: 1,
      updated_at: "2026-10-27T04:15:01Z",
      acked_at: null,
    },
  },
  {
    message: {
      id: "msg-002",
      thread_id: "thr-002",
      sender_name: "wesley",
      to: "ferris",
      kind: "mail",
      urgency: "normal",
      durable: true,
      subject: "FerroDeck Task 4 ready",
      body: "DiffViewer and GitPrPane tests are green.",
      created_at: "2026-10-27T04:20:00Z",
    },
    receipt: {
      message_id: "msg-002",
      recipient_id: "urn:fl:agent:ferris",
      recipient_name: "ferris",
      status: "acked",
      attempts: 1,
      updated_at: "2026-10-27T04:21:00Z",
      acked_at: "2026-10-27T04:21:00Z",
    },
  },
  {
    message: {
      id: "msg-003",
      thread_id: "thr-003",
      sender_name: "jarvis",
      to: "all",
      kind: "system",
      urgency: "normal",
      durable: true,
      subject: "Nightly Redb compaction complete",
      body: "Workspace stores compacted cleanly.",
      created_at: "2026-10-27T04:25:00Z",
    },
    receipt: {
      message_id: "msg-003",
      recipient_id: "urn:fl:agent:kayla",
      recipient_name: "kayla",
      status: "pending",
      attempts: 0,
      updated_at: "2026-10-27T04:25:00Z",
      acked_at: null,
    },
  },
];

describe("WireMailPane", () => {
  it("renders messages with urgency pills, delivery status, filters by recipient scope and Unread Only, and acknowledges messages", async () => {
    const onAckMessage = vi.fn(async () => {});
    const onRefresh = vi.fn();

    render(
      <WireMailPane
        messages={SAMPLE_MESSAGES}
        selectedAgentName="ferris"
        agents={DEFAULT_WORKSPACE_AGENTS}
        onAckMessage={onAckMessage}
        onSendMessage={vi.fn()}
        onRefresh={onRefresh}
      />,
    );

    // Selected Agent scope ("ferris") shows messages addressed to ferris or "all"
    expect(screen.getByText("GPU arbitration window starting")).toBeInTheDocument();
    expect(screen.getByText("FerroDeck Task 4 ready")).toBeInTheDocument();
    expect(screen.getByText("Nightly Redb compaction complete")).toBeInTheDocument();

    // Urgency and status badges
    expect(screen.getByTestId("mail-urgency-msg-001")).toHaveTextContent(/urgent/i);
    expect(screen.getByTestId("mail-status-msg-001")).toHaveTextContent(/delivered/i);
    expect(screen.getByTestId("mail-status-msg-002")).toHaveTextContent(/acked/i);

    // Unread Only toggle filters out acked msg-002
    const unreadToggle = screen.getByRole("button", { name: /Unread Only/i });
    fireEvent.click(unreadToggle);
    expect(screen.getByText("GPU arbitration window starting")).toBeInTheDocument();
    expect(screen.queryByText("FerroDeck Task 4 ready")).toBeNull();

    // Turn off Unread Only
    fireEvent.click(unreadToggle);
    expect(screen.getByText("FerroDeck Task 4 ready")).toBeInTheDocument();

    // Acknowledge msg-001
    const ackBtn = screen.getByTestId("ack-btn-msg-001");
    fireEvent.click(ackBtn);

    await waitFor(() => {
      expect(onAckMessage).toHaveBeenCalledWith("msg-001", "ferris");
    });

    // Already acked message should not render an active Acknowledge button
    expect(screen.queryByTestId("ack-btn-msg-002")).toBeNull();
  });

  it("composes and dispatches a FerroWire message via onSendMessage", async () => {
    const onSendMessage = vi.fn(async () => {});

    render(
      <WireMailPane
        messages={SAMPLE_MESSAGES}
        selectedAgentName="ferris"
        agents={DEFAULT_WORKSPACE_AGENTS}
        onAckMessage={vi.fn()}
        onSendMessage={onSendMessage}
      />,
    );

    const toSelect = screen.getByLabelText(/Recipient \(To\)/i) as HTMLSelectElement;
    const urgencySelect = screen.getByLabelText(/Message Urgency/i) as HTMLSelectElement;
    const subjectInput = screen.getByLabelText(/Message Subject/i) as HTMLInputElement;
    const bodyInput = screen.getByLabelText(/Message Body/i) as HTMLTextAreaElement;

    fireEvent.change(toSelect, { target: { value: "jarvis" } });
    fireEvent.change(urgencySelect, { target: { value: "urgent" } });
    fireEvent.change(subjectInput, { target: { value: "Ready for PR review" } });
    fireEvent.change(bodyInput, {
      target: { value: "PR #55 is green and ready for Lab Manager review." },
    });

    const sendBtn = screen.getByRole("button", { name: /Send Message/i });
    fireEvent.click(sendBtn);

    await waitFor(() => {
      expect(onSendMessage).toHaveBeenCalledWith(
        expect.objectContaining({
          to: "jarvis",
          urgency: "urgent",
          subject: "Ready for PR review",
          body: "PR #55 is green and ready for Lab Manager review.",
          from: "ferris",
        }),
      );
    });
  });
});
