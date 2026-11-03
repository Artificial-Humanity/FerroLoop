import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { TurnCard } from "../components/TurnCard";
import {
  WireInboxPane,
  formatWireIntentBody,
  parseCommitteeVotesSpec,
  parseWireIntentMetadata,
} from "../components/WireInboxPane";
import type { WireSendRequest } from "../types/api";

describe("Record #101 — Typed Delegate and Committee FerroWire Intents", () => {
  it("parses and formats structured delegate and committee intents with graceful fallback", () => {
    const delegateBody = formatWireIntentBody(
      "Please implement the new UI pane.",
      "delegate",
      {
        target_record: "#101",
        deliverable: "Typed Wire intents UI",
        branch_or_pr: "felix/record-101-wire-intents",
        resume_on_reply: true,
      },
    );

    const parsedDelegate = parseWireIntentMetadata({
      kind: "mail",
      subject: "Delegate #101",
      body: delegateBody,
    });
    expect(parsedDelegate.intent).toBe("delegate");
    expect(parsedDelegate.delegate).toEqual({
      target_record: "#101",
      deliverable: "Typed Wire intents UI",
      branch_or_pr: "felix/record-101-wire-intents",
      resume_on_reply: true,
    });

    const committeeBody = formatWireIntentBody(
      "Deliberate on Strategy Council #2 wire intents.",
      "committee",
      undefined,
      {
        council_track: "Strategy Council #2",
        quorum: "3/3 unanimous",
        artifact_path: "docs/specs/wire-intents.md",
        artifact_sha256: "sha256:9f86d08",
        votes: parseCommitteeVotesSpec(
          "krono:APPROVE, kendi:APPROVE WITH AMENDMENTS, kora:BLOCK, jarvis:PENDING",
        ),
      },
    );

    const parsedCommittee = parseWireIntentMetadata({
      kind: "mail",
      subject: "Council Round",
      body: committeeBody,
    });
    expect(parsedCommittee.intent).toBe("committee");
    expect(parsedCommittee.committee).toEqual({
      council_track: "Strategy Council #2",
      quorum: "3/3 unanimous",
      artifact_path: "docs/specs/wire-intents.md",
      artifact_sha256: "sha256:9f86d08",
      votes: {
        krono: "APPROVE",
        kendi: "APPROVE WITH AMENDMENTS",
        kora: "BLOCK",
        jarvis: "PENDING",
      },
    });

    // Graceful fallback when hub only stores standard mail
    const parsedMail = parseWireIntentMetadata({
      kind: "mail",
      subject: "Hello",
      body: "Standard message body",
    });
    expect(parsedMail.intent).toBe("mail");
  });

  it("composes and renders delegate and committee cards in WireInboxPane", async () => {
    const sentPayloads: WireSendRequest[] = [];
    render(
      <WireInboxPane
        selectedAgentName="felix"
        onSendMessage={(payload) => {
          sentPayloads.push(payload);
        }}
        messages={[
          {
            message: {
              id: "msg-del-1",
              thread_id: "t-1",
              sender_name: "ferris",
              to: "felix",
              kind: "delegate",
              urgency: "normal",
              durable: true,
              subject: "Delegate Record #101",
              body: formatWireIntentBody(
                "Build typed Wire intents.",
                "delegate",
                {
                  target_record: "#101",
                  deliverable: "WireInboxPane + TurnCard support",
                  branch_or_pr: "felix/record-101-wire-intents",
                  resume_on_reply: true,
                },
              ),
              created_at: "2026-11-03T05:00:00Z",
            },
          },
          {
            message: {
              id: "msg-com-1",
              thread_id: "t-2",
              sender_name: "ferris",
              to: "felix",
              kind: "committee",
              urgency: "normal",
              durable: true,
              subject: "Strategy Council #2 Vote",
              body: formatWireIntentBody(
                "Please record your vote.",
                "committee",
                undefined,
                {
                  council_track: "Strategy Council #2",
                  quorum: "3/3 unanimous",
                  artifact_path: "docs/specs/council-2.md",
                  artifact_sha256: "sha256:abcdef01",
                  votes: {
                    krono: "APPROVE",
                    kendi: "APPROVE WITH AMENDMENTS",
                    kora: "BLOCK",
                    felix: "PENDING",
                  },
                },
              ),
              created_at: "2026-11-03T05:01:00Z",
            },
          },
        ]}
      />,
    );

    // Verify rendered delegate inbox card
    expect(screen.getByTestId("mail-delegate-card-msg-del-1")).toHaveTextContent(
      "Record: #101",
    );
    expect(
      screen.getByTestId("mail-delegate-resume-badge-msg-del-1"),
    ).toHaveTextContent("Non-Blocking · Resume on Reply");

    // Verify rendered committee inbox card & 4 vote pills
    expect(screen.getByTestId("mail-committee-card-msg-com-1")).toHaveTextContent(
      "Strategy Council #2",
    );
    expect(
      screen.getByTestId("committee-vote-pill-msg-com-1-krono"),
    ).toHaveTextContent("krono: APPROVE");
    expect(
      screen.getByTestId("committee-vote-pill-msg-com-1-kendi"),
    ).toHaveTextContent("kendi: APPROVE WITH AMENDMENTS");
    expect(
      screen.getByTestId("committee-vote-pill-msg-com-1-kora"),
    ).toHaveTextContent("kora: BLOCK");
    expect(
      screen.getByTestId("committee-vote-pill-msg-com-1-felix"),
    ).toHaveTextContent("felix: PENDING");

    // Switch composer to 'delegate' mode and send
    fireEvent.change(screen.getByTestId("wire-compose-intent-select"), {
      target: { value: "delegate" },
    });
    expect(screen.getByTestId("wire-delegate-fields")).toBeInTheDocument();
    expect(
      screen.getByTestId("delegate-composer-status-badge"),
    ).toHaveTextContent("Non-Blocking · Resume on Reply");
    fireEvent.change(screen.getByTestId("delegate-target-record-input"), {
      target: { value: "#101" },
    });
    fireEvent.change(screen.getByTestId("delegate-deliverable-input"), {
      target: { value: "Typed intents UI" },
    });
    fireEvent.change(screen.getByTestId("delegate-branch-pr-input"), {
      target: { value: "PR #232" },
    });
    fireEvent.change(screen.getByLabelText("Message Subject"), {
      target: { value: "Delegate #101 to Felix" },
    });
    fireEvent.change(screen.getByLabelText("Message Body"), {
      target: { value: "Execute Record #101." },
    });
    fireEvent.click(screen.getByText("Send Message"));

    await waitFor(() => {
      expect(sentPayloads).toHaveLength(1);
    });
    expect(sentPayloads[0]?.kind).toBe("delegate");
    expect(sentPayloads[0]?.delegate_meta).toEqual({
      target_record: "#101",
      deliverable: "Typed intents UI",
      branch_or_pr: "PR #232",
      resume_on_reply: true,
    });
  });

  it("renders typed delegate and committee Wire turns in TurnCard", () => {
    const { unmount } = render(
      <TurnCard
        turn={{
          index: 7,
          role: "wire:ferris",
          content: formatWireIntentBody(
            "Assigned Record #101.",
            "delegate",
            {
              target_record: "#101",
              deliverable: "WireInboxPane + TurnCard",
              branch_or_pr: "felix/record-101",
              resume_on_reply: true,
            },
          ),
          tool_calls: [],
          created_at: "2026-11-03T05:02:00Z",
        }}
      />,
    );

    expect(screen.getByTestId("turn-wire-intent-badge-7")).toHaveTextContent(
      "delegate",
    );
    expect(screen.getByTestId("turn-wire-delegate-card-7")).toHaveTextContent(
      "Record: #101",
    );
    expect(screen.getByTestId("turn-wire-delegate-resume-7")).toHaveTextContent(
      "Non-Blocking · Resume on Reply",
    );
    unmount();

    render(
      <TurnCard
        turn={{
          index: 8,
          role: "wire:ferris",
          content: formatWireIntentBody(
            "Deliberation round open.",
            "committee",
            undefined,
            {
              council_track: "Strategy Council #2",
              quorum: "3/3 unanimous",
              artifact_path: "docs/specs/wire.md",
              artifact_sha256: "sha256:1234",
              votes: {
                krono: "APPROVE",
                kendi: "APPROVE WITH AMENDMENTS",
                kora: "PENDING",
              },
            },
          ),
          tool_calls: [],
          created_at: "2026-11-03T05:03:00Z",
        }}
      />,
    );

    expect(screen.getByTestId("turn-wire-intent-badge-8")).toHaveTextContent(
      "committee",
    );
    expect(screen.getByTestId("turn-wire-committee-card-8")).toHaveTextContent(
      "Strategy Council #2",
    );
    expect(
      screen.getByTestId("turn-committee-vote-pill-8-kendi"),
    ).toHaveTextContent("kendi: APPROVE WITH AMENDMENTS");
  });
});
