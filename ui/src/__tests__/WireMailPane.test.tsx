import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { HubApiClient } from "../api/client";
import { WireMailPane } from "../components/WireMailPane";
import { TEST_WORKSPACE_AGENTS as DEFAULT_WORKSPACE_AGENTS } from "./fixtures/testAgents";
import {
  type WireInboxItem,
  type WireStatusResponse,
} from "../types/api";

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

  it("renders wire-diagnostics-unavailable for inactive daemon with pending recipients and never renders '0 holds'", () => {
    const wireStatus: WireStatusResponse = {
      daemon: "inactive",
      dispatch_diagnostics: null,
      pending_recipients: [
        {
          recipient: "ferris",
          pending_count: 3,
          oldest_pending_at: "2026-11-06T18:20:31.000Z",
          oldest_pending_age_ms: 125_000,
        },
      ],
    };

    render(
      <WireMailPane
        messages={SAMPLE_MESSAGES}
        selectedAgentName="ferris"
        agents={DEFAULT_WORKSPACE_AGENTS}
        wireStatus={wireStatus}
      />,
    );

    const strip = screen.getByTestId("wire-status-strip");
    const unavailable = screen.getByTestId("wire-diagnostics-unavailable");
    expect(unavailable).toHaveTextContent(
      "Auto-push diagnostics: unavailable (wire daemon inactive)",
    );
    expect(strip).not.toHaveTextContent("0 holds");

    const pendingBadge = screen.getByTestId("wire-pending-recipient-ferris");
    expect(pendingBadge).toHaveTextContent("ferris: 3 pending (oldest 2m 5s)");
  });

  it("renders wire-diagnostics-unavailable for unreachable daemon with status query failed and never 'inactive' or '0 holds'", () => {
    const wireStatus: WireStatusResponse = {
      daemon: "unreachable",
      dispatch_diagnostics: null,
      pending_recipients: [],
    };

    render(
      <WireMailPane
        messages={SAMPLE_MESSAGES}
        selectedAgentName="ferris"
        agents={DEFAULT_WORKSPACE_AGENTS}
        wireStatus={wireStatus}
      />,
    );

    const strip = screen.getByTestId("wire-status-strip");
    const unavailable = screen.getByTestId("wire-diagnostics-unavailable");
    expect(unavailable).toHaveTextContent(
      "Auto-push diagnostics: unavailable (status query failed)",
    );
    expect(strip).not.toHaveTextContent("inactive");
    expect(strip).not.toHaveTextContent("0 holds");
  });

  it("renders wire-diagnostics-unavailable for active older daemon with null dispatch_diagnostics and never '0 holds'", () => {
    const wireStatus: WireStatusResponse = {
      daemon: "active",
      dispatch_diagnostics: null,
      pending_recipients: [],
    };

    render(
      <WireMailPane
        messages={SAMPLE_MESSAGES}
        selectedAgentName="ferris"
        agents={DEFAULT_WORKSPACE_AGENTS}
        wireStatus={wireStatus}
      />,
    );

    const strip = screen.getByTestId("wire-status-strip");
    const unavailable = screen.getByTestId("wire-diagnostics-unavailable");
    expect(unavailable).toHaveTextContent(
      "Auto-push diagnostics: unavailable (unsupported daemon)",
    );
    expect(strip).not.toHaveTextContent("0 holds");
  });

  it("renders wire-diagnostics-available with '0 holds (daemon_lifetime)' when dispatch_diagnostics is supported and empty", () => {
    const wireStatus: WireStatusResponse = {
      daemon: "active",
      dispatch_diagnostics: {
        schema_version: 1,
        instance_nonce: "cf54f7ef527a41fe018e7dab492dcfcb",
        instance_started_at: "2026-11-09T07:48:57.578Z",
        scope: "daemon_lifetime",
        records: [],
        global_capacity_exhausted: false,
        diagnostics_partial: false,
        omitted_evidence_total: 0,
        omitted_evidence_total_saturated: false,
      },
      pending_recipients: [],
    };

    render(
      <WireMailPane
        messages={SAMPLE_MESSAGES}
        selectedAgentName="ferris"
        agents={DEFAULT_WORKSPACE_AGENTS}
        wireStatus={wireStatus}
      />,
    );

    const available = screen.getByTestId("wire-diagnostics-available");
    expect(available).toHaveTextContent(
      "Auto-push diagnostics: 0 holds (daemon_lifetime)",
    );
  });

  it("renders held recipient FenceDiagnostic row with registration_generation=unknown, decisive reason, stages, and zero recovery controls", () => {
    const wireStatus: WireStatusResponse = {
      daemon: "active",
      dispatch_diagnostics: {
        schema_version: 1,
        instance_nonce: "cf54f7ef527a41fe018e7dab492dcfcb",
        instance_started_at: "2026-11-09T07:48:57.578Z",
        scope: "daemon_lifetime",
        records: [
          {
            recipient: "kayla",
            recipient_digest: null,
            recipient_invalid: false,
            trigger_message_id: "urn:uuid:01a1b217-3cdb-7769-9027-93b8f3963dbe",
            trigger_message_id_digest: null,
            trigger_message_id_omitted: false,
            hold_observed_at: "2026-11-06T18:20:31.000Z",
            phase: "retained_fence",
            first_reason: "transport_error",
            decisive_reason: "unresolved_receipt_status",
            failure_stages: ["transport_error", "unresolved_receipt_status"],
            omitted_failure_stages: 0,
            omitted_failure_stages_saturated: false,
            route_observations: [
              {
                adapter: "claude",
                identity_digest: "a1b2c3d4e5f6",
                route_updated_at: "2026-11-06T18:00:00.000Z",
                explicit: true,
                outcome: "returned_ok",
              },
            ],
            omitted_route_observations: 0,
            omitted_route_observations_saturated: false,
            route_dispatch_calls: 1,
            route_dispatch_calls_saturated: false,
            registration_generation: null,
          },
        ],
        global_capacity_exhausted: true,
        diagnostics_partial: true,
        omitted_evidence_total: 4,
        omitted_evidence_total_saturated: true,
      },
      pending_recipients: [
        {
          recipient: "kayla",
          pending_count: 2,
          oldest_pending_at: "2026-11-06T18:20:31.000Z",
          oldest_pending_age_ms: 45_000,
        },
      ],
    };

    render(
      <WireMailPane
        messages={SAMPLE_MESSAGES}
        selectedAgentName="kayla"
        agents={DEFAULT_WORKSPACE_AGENTS}
        wireStatus={wireStatus}
      />,
    );

    const strip = screen.getByTestId("wire-status-strip");
    expect(screen.getByTestId("wire-diagnostics-holds")).toBeInTheDocument();
    expect(screen.getByTestId("wire-diagnostics-hold-count")).toHaveTextContent("holds=1");
    expect(screen.getByTestId("wire-diagnostics-global-cap")).toHaveTextContent(
      "global_capacity_exhausted",
    );
    expect(screen.getByTestId("wire-diagnostics-partial")).toHaveTextContent(
      "diagnostics_partial (omitted=4+)",
    );

    const holdRow = screen.getByTestId("wire-hold-row-kayla");
    expect(holdRow).toHaveTextContent("kayla");
    expect(holdRow).toHaveTextContent("retained_fence");
    expect(holdRow).toHaveTextContent("decisive_reason=unresolved_receipt_status");
    expect(holdRow).toHaveTextContent("first_reason=transport_error");
    expect(holdRow).toHaveTextContent("stages=transport_error -> unresolved_receipt_status");
    expect(holdRow).toHaveTextContent("calls=1");
    expect(holdRow).toHaveTextContent("claude:returned_ok");
    expect(holdRow).toHaveTextContent("registration_generation=unknown");
    expect(holdRow).not.toHaveTextContent("registration_generation=0");

    expect(strip).toHaveTextContent(
      "Daemon-lifetime hold suppresses automatic pushes; ACK and route registration do not clear it.",
    );

    const pendingKayla = screen.getByTestId("wire-pending-recipient-kayla");
    expect(pendingKayla).toHaveTextContent("kayla: 2 pending (oldest 45s)");

    // Condition 5: Zero recovery, clear, unfence, retry, or replay buttons in status strip
    expect(within(strip).queryAllByRole("button")).toHaveLength(0);
    expect(screen.queryByRole("button", { name: /clear|unfence|retry|replay/i })).toBeNull();
  });

  it("renders wire-pending-unavailable when pending_recipients_available is false and omits it when true with empty pending_recipients", () => {
    const unavailableStatus: WireStatusResponse = {
      daemon: "inactive",
      dispatch_diagnostics: null,
      pending_recipients_available: false,
      pending_recipients: [],
    };

    const { rerender } = render(
      <WireMailPane
        messages={SAMPLE_MESSAGES}
        selectedAgentName="ferris"
        agents={DEFAULT_WORKSPACE_AGENTS}
        wireStatus={unavailableStatus}
      />,
    );

    const pendingUnavailable = screen.getByTestId("wire-pending-unavailable");
    expect(pendingUnavailable).toHaveTextContent(/^pending: unavailable$/);

    const availableEmptyStatus: WireStatusResponse = {
      daemon: "inactive",
      dispatch_diagnostics: null,
      pending_recipients_available: true,
      pending_recipients: [],
    };

    rerender(
      <WireMailPane
        messages={SAMPLE_MESSAGES}
        selectedAgentName="ferris"
        agents={DEFAULT_WORKSPACE_AGENTS}
        wireStatus={availableEmptyStatus}
      />,
    );

    expect(screen.queryByTestId("wire-pending-unavailable")).toBeNull();
  });

  it("fetches wireStatus via apiClient.getWireStatus on mount and on Refresh click, surfacing HTTP errors as unreachable", async () => {
    const getWireStatus = vi
      .fn<() => Promise<WireStatusResponse>>()
      .mockResolvedValueOnce({
        daemon: "active",
        dispatch_diagnostics: {
          schema_version: 1,
          instance_nonce: "nonce-1",
          instance_started_at: "2026-11-09T07:48:57.578Z",
          scope: "daemon_lifetime",
          records: [],
          global_capacity_exhausted: false,
          diagnostics_partial: false,
          omitted_evidence_total: 0,
          omitted_evidence_total_saturated: false,
        },
        pending_recipients_available: true,
        pending_recipients: [],
      })
      .mockRejectedValueOnce(new Error("HTTP 502"));

    const mockApiClient = {
      getWireStatus,
    } as unknown as HubApiClient;

    const onRefresh = vi.fn();

    render(
      <WireMailPane
        messages={SAMPLE_MESSAGES}
        selectedAgentName="ferris"
        agents={DEFAULT_WORKSPACE_AGENTS}
        apiClient={mockApiClient}
        onRefresh={onRefresh}
      />,
    );

    await waitFor(() => {
      expect(getWireStatus).toHaveBeenCalledTimes(1);
    });
    expect(await screen.findByTestId("wire-diagnostics-available")).toHaveTextContent(
      "Auto-push diagnostics: 0 holds (daemon_lifetime)",
    );
    expect(screen.queryByTestId("wire-pending-unavailable")).toBeNull();

    const refreshBtn = screen.getByTestId("wire-refresh-btn");
    fireEvent.click(refreshBtn);

    await waitFor(() => {
      expect(getWireStatus).toHaveBeenCalledTimes(2);
    });
    expect(onRefresh).toHaveBeenCalledTimes(1);
    expect(await screen.findByTestId("wire-diagnostics-unavailable")).toHaveTextContent(
      "Auto-push diagnostics: unavailable (status query failed)",
    );
    expect(screen.getByTestId("wire-pending-unavailable")).toHaveTextContent(
      /^pending: unavailable$/,
    );
  });
});
