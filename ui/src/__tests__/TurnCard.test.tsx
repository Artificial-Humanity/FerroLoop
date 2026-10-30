import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import {
  stripAgyUserRequestWrapper,
  stripSubagentHandbackWrapper,
  TurnCard,
} from "../components/TurnCard";
import type { TranscriptTurn } from "../types/api";

function turn(partial: Partial<TranscriptTurn> & { index: number }): TranscriptTurn {
  return {
    role: "assistant",
    content: "",
    thinking: null,
    tool_calls: [],
    ...partial,
  };
}

describe("TurnCard directed assistant highlight", () => {
  it("highlights an assistant turn with prose directed at the operator", () => {
    render(
      <TurnCard
        agentDisplayName="Ferris"
        turn={turn({
          index: 2,
          content: "The hub restart finished. FerroDeck is live on the new bundle.",
        })}
      />,
    );

    const card = screen.getByTestId("turn-card-2");
    expect(card).toHaveAttribute("data-directed-assistant", "true");
    expect(card.style.borderColor).toContain("var(--card-assistant-border)");
    expect(card.style.boxShadow).toContain("inset");
  });

  it("keeps the amber Operator highlight distinct from directed replies", () => {
    render(
      <TurnCard
        turn={turn({ index: 0, role: "user", content: "Please ship it." })}
      />,
    );

    const card = screen.getByTestId("turn-card-0");
    expect(card).not.toHaveAttribute("data-directed-assistant", "true");
    expect(card.style.boxShadow).toContain("inset");
    expect(screen.getByText("Operator")).toBeInTheDocument();
  });

  it("does not highlight thinking-only or tool-only assistant turns", () => {
    render(
      <>
        <TurnCard
          turn={turn({
            index: 1,
            content: "",
            thinking: "Planning the change quietly.",
            tool_calls: [{ name: "run_command", arguments: { command: "fl status" } }],
          })}
        />
        <TurnCard
          turn={turn({
            index: 2,
            role: "tool",
            content: "Created At: t\nCommand output: done",
          })}
        />
      </>,
    );

    expect(screen.getByTestId("turn-card-1")).not.toHaveAttribute(
      "data-directed-assistant",
      "true",
    );
    expect(screen.getByTestId("turn-card-2")).not.toHaveAttribute(
      "data-directed-assistant",
      "true",
    );
  });

  it("does not highlight wire turns as directed replies and applies subdued brown/bronze wire outline", () => {
    render(
      <TurnCard
        turn={turn({
          index: 4,
          role: "wire:jarvis",
          content: "Subject: Review ready\nBody for the operator to read.",
        })}
      />,
    );

    const card = screen.getByTestId("turn-card-4");
    expect(card).not.toHaveAttribute("data-directed-assistant", "true");
    expect(card).toHaveAttribute("data-wire-card", "true");
    expect(card.style.borderColor).toContain("--wire-card-border");
    expect(card.style.boxShadow).toContain("--wire-card-stripe");
    expect(card.style.backgroundColor).toContain("--wire-card-bg");
  });

  it("renders system cards without highlight and collapsed by default until clicked", () => {
    render(
      <TurnCard
        turn={turn({
          index: 5,
          role: "system",
          content: "System instruction line 1\nSystem instruction line 2",
        })}
      />,
    );

    const card = screen.getByTestId("turn-card-5");
    expect(within(card).getByText("System")).toBeInTheDocument();
    expect(within(card).queryByText("Operator")).toBeNull();
    expect(card).not.toHaveAttribute("data-directed-assistant", "true");
    expect(card.style.borderColor).toBe("var(--border-subtle)");
    expect(card.style.backgroundColor).toBe("var(--bg-surface)");
    expect(card.style.boxShadow).toBe("");

    const toggle = within(card).getByTestId("turn-system-output-toggle-5");
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(toggle).toHaveTextContent("System (2 lines)");
    expect(toggle).toHaveTextContent("Show output");
    expect(within(card).queryByText(/System instruction line 1/)).toBeNull();

    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(toggle).toHaveTextContent("Hide output");
    expect(within(card).getByText(/System instruction line 1/)).toBeInTheDocument();
  });

  it("defensively classifies raw code-snippet user dumps as collapsed System turns", () => {
    render(
      <TurnCard
        turn={turn({
          index: 6,
          role: "user",
          content: [
            '")',
            '729: && !inner.starts_with("<local-command-stdout>")',
            '730: && !inner.starts_with("<bash-notification>")',
          ].join("\n"),
        })}
      />,
    );

    const card = screen.getByTestId("turn-card-6");
    expect(within(card).getByText("System")).toBeInTheDocument();
    expect(within(card).queryByText("Operator")).toBeNull();
    expect(card).not.toHaveAttribute("data-directed-assistant", "true");
    expect(card.style.borderColor).toBe("var(--border-subtle)");
    expect(card.style.backgroundColor).toBe("var(--bg-surface)");
    expect(card.style.boxShadow).toBe("");

    const toggle = within(card).getByTestId("turn-system-output-toggle-6");
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(toggle).toHaveTextContent("System (3 lines)");
    expect(within(card).queryByText(/729: && !inner\.starts_with/)).toBeNull();

    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(within(card).getByText(/729: && !inner\.starts_with/)).toBeInTheDocument();
  });

  it("strips Antigravity <USER_REQUEST> and <ADDITIONAL_METADATA> wrappers from operator turns", () => {
    const raw = [
      "<USER_REQUEST>",
      "I'm sorry, Ferris, I accidentally clicked the stop execution button. Please continue",
      "</USER_REQUEST>",
      "<ADDITIONAL_METADATA>",
      "The current local time is: 2026-10-29T18:44:28Z.",
      "</ADDITIONAL_METADATA>",
    ].join("\n");

    expect(stripAgyUserRequestWrapper(raw)).toBe(
      "I'm sorry, Ferris, I accidentally clicked the stop execution button. Please continue",
    );

    render(
      <TurnCard
        turn={turn({
          index: 7,
          role: "user",
          content: raw,
        })}
      />,
    );

    const card = screen.getByTestId("turn-card-7");
    expect(within(card).getByText("Operator")).toBeInTheDocument();
    expect(
      within(card).getByText(
        /I'm sorry, Ferris, I accidentally clicked the stop execution button\. Please continue/,
      ),
    ).toBeInTheDocument();
    expect(within(card).queryByText(/USER_REQUEST/)).toBeNull();
    expect(within(card).queryByText(/ADDITIONAL_METADATA/)).toBeNull();
    expect(within(card).queryByText(/The current local time is/)).toBeNull();
  });

  it("classifies [Notice] and <EPHEMERAL_MESSAGE> user turns as collapsed System turns instead of Operator", () => {
    render(
      <>
        <TurnCard
          turn={turn({
            index: 8,
            role: "user",
            content:
              "[Notice] All your subagents and background tasks have been stopped due to user force cancellation.",
          })}
        />
        <TurnCard
          turn={turn({
            index: 9,
            role: "user",
            content:
              "<EPHEMERAL_MESSAGE>\nSubagent wakeup notification.\n</EPHEMERAL_MESSAGE>",
          })}
        />
      </>,
    );

    const noticeCard = screen.getByTestId("turn-card-8");
    expect(within(noticeCard).getByText("System")).toBeInTheDocument();
    expect(within(noticeCard).queryByText("Operator")).toBeNull();
    const noticeToggle = within(noticeCard).getByTestId("turn-system-output-toggle-8");
    expect(noticeToggle).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(noticeToggle);
    expect(
      within(noticeCard).getByText(/All your subagents and background tasks have been stopped/),
    ).toBeInTheDocument();

    const ephemeralCard = screen.getByTestId("turn-card-9");
    expect(within(ephemeralCard).getByText("System")).toBeInTheDocument();
    expect(within(ephemeralCard).queryByText("Operator")).toBeNull();
    const ephemeralToggle = within(ephemeralCard).getByTestId(
      "turn-system-output-toggle-9",
    );
    fireEvent.click(ephemeralToggle);
    expect(
      within(ephemeralCard).getByText(/Subagent wakeup notification\./),
    ).toBeInTheDocument();
    expect(within(ephemeralCard).queryByText(/EPHEMERAL_MESSAGE/)).toBeNull();
  });

  it("preserves <USER_REQUEST> inside assistant or tool turns and classifies [Notice] wrapped in <USER_REQUEST> as System", () => {
    const assistantCodeSnippet = [
      "<USER_REQUEST>",
      "diff --git a/ui/src/components/TurnCard.tsx b/ui/src/components/TurnCard.tsx",
      "</USER_REQUEST>",
    ].join("\n");

    const wrappedNotice = [
      "<USER_REQUEST>",
      "[Notice] All your subagents and background tasks have been stopped due to user force cancellation.",
      "</USER_REQUEST>",
      "<ADDITIONAL_METADATA>",
      "The current local time is: 2026-10-30T16:00:00Z.",
      "</ADDITIONAL_METADATA>",
    ].join("\n");

    expect(
      stripAgyUserRequestWrapper("Here is a snippet:\n<USER_REQUEST>\nhello\n</USER_REQUEST>"),
    ).toBe("Here is a snippet:\n<USER_REQUEST>\nhello\n</USER_REQUEST>");

    render(
      <>
        <TurnCard
          agentDisplayName="Ferris"
          turn={turn({
            index: 10,
            role: "assistant",
            content: assistantCodeSnippet,
          })}
        />
        <TurnCard
          turn={turn({
            index: 11,
            role: "user",
            content: wrappedNotice,
          })}
        />
      </>,
    );

    const assistantCard = screen.getByTestId("turn-card-10");
    expect(within(assistantCard).getByText("Ferris")).toBeInTheDocument();
    expect(within(assistantCard).getByText(/<USER_REQUEST>/)).toBeInTheDocument();
    expect(within(assistantCard).getByText(/<\/USER_REQUEST>/)).toBeInTheDocument();

    const wrappedNoticeCard = screen.getByTestId("turn-card-11");
    expect(within(wrappedNoticeCard).getByText("System")).toBeInTheDocument();
    expect(within(wrappedNoticeCard).queryByText("Operator")).toBeNull();
    const toggle = within(wrappedNoticeCard).getByTestId("turn-system-output-toggle-11");
    fireEvent.click(toggle);
    expect(
      within(wrappedNoticeCard).getByText(
        /All your subagents and background tasks have been stopped/,
      ),
    ).toBeInTheDocument();
    expect(within(wrappedNoticeCard).queryByText(/USER_REQUEST/)).toBeNull();
  });

  it("classifies subagent hand-back user turns (with or without 'Another Claude session sent a message:') as collapsed Wire · subagent cards and strips wrapper boilerplate", () => {
    const directHandback = [
      '<agent-message from="acd87c17952de4c0a">',
      "[Subagent hand-back] Subagent acd87c17952de4c0a completed its task. The report follows:",
      "### Findings",
      "All checks passed cleanly.",
      "</agent-message>",
    ].join("\n");

    const claudeForwardedHandback = [
      "Another Claude session sent a message:",
      '<agent-message from="acd87c17952de4c0a">',
      "[Subagent hand-back] Subagent acd87c17952de4c0a completed its task. The report follows:",
      "### Findings",
      "Forwarded subagent report details.",
      "</agent-message>",
      "",
      'This came from another Claude session (ID: acd87c17952de4c0a). Reply using SendMessage with to: "acd87c17952de4c0a".',
    ].join("\n");

    expect(stripSubagentHandbackWrapper(directHandback)).toBe(
      "### Findings\nAll checks passed cleanly.",
    );
    expect(stripSubagentHandbackWrapper(claudeForwardedHandback)).toBe(
      "### Findings\nForwarded subagent report details.",
    );

    render(
      <>
        <TurnCard
          turn={turn({
            index: 12,
            role: "user",
            content: directHandback,
          })}
        />
        <TurnCard
          turn={turn({
            index: 13,
            role: "user",
            content: claudeForwardedHandback,
          })}
        />
      </>,
    );

    for (const [idx, bodySnippet] of [
      [12, /All checks passed cleanly\./],
      [13, /Forwarded subagent report details\./],
    ] as const) {
      const card = screen.getByTestId(`turn-card-${idx}`);
      expect(within(card).getByText("Wire · subagent")).toBeInTheDocument();
      expect(within(card).queryByText("Operator")).toBeNull();
      expect(card).toHaveAttribute("data-wire-card", "true");
      expect(card).not.toHaveAttribute("data-directed-assistant", "true");

      const wireToggle = within(card).getByTestId("wire-output-toggle");
      expect(wireToggle).toHaveAttribute("aria-expanded", "false");
      expect(wireToggle).toHaveTextContent("Wire · subagent — ### Findings (2 lines)");
      expect(within(card).queryByText(bodySnippet)).toBeNull();

      fireEvent.click(wireToggle);
      expect(wireToggle).toHaveAttribute("aria-expanded", "true");
      expect(within(card).getByRole("heading", { name: "Findings" })).toBeInTheDocument();
      expect(within(card).getByText(bodySnippet)).toBeInTheDocument();
      expect(within(card).queryByText(/agent-message/)).toBeNull();
      expect(within(card).queryByText(/Subagent hand-back/)).toBeNull();
      expect(within(card).queryByText(/Another Claude session sent a message/)).toBeNull();
    }
  });

  it("classifies Claude internal notification and command tags (<task-notification>, <local-command-caveat>, <local-command-stdout>, [SYSTEM NOTIFICATION - NOT USER INPUT]) as collapsed System turns", () => {
    const internalTurns = [
      {
        index: 14,
        content: "<task-notification>\nBackground task b91 completed.\n</task-notification>",
        expectedText: /Background task b91 completed\./,
      },
      {
        index: 15,
        content:
          "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>",
        expectedText: /Caveat: The messages below were generated by the user/,
      },
      {
        index: 16,
        content: "<local-command-stdout>On branch main\nnothing to commit</local-command-stdout>",
        expectedText: /On branch main/,
      },
      {
        index: 17,
        content:
          "[SYSTEM NOTIFICATION - NOT USER INPUT]\nScheduled wakeup check triggered.",
        expectedText: /Scheduled wakeup check triggered\./,
      },
    ];

    render(
      <>
        {internalTurns.map((item) => (
          <TurnCard
            key={item.index}
            turn={turn({
              index: item.index,
              role: "user",
              content: item.content,
            })}
          />
        ))}
      </>,
    );

    for (const item of internalTurns) {
      const card = screen.getByTestId(`turn-card-${item.index}`);
      expect(within(card).getByText("System")).toBeInTheDocument();
      expect(within(card).queryByText("Operator")).toBeNull();
      expect(card).not.toHaveAttribute("data-directed-assistant", "true");

      const sysToggle = within(card).getByTestId(`turn-system-output-toggle-${item.index}`);
      expect(sysToggle).toHaveAttribute("aria-expanded", "false");
      expect(within(card).queryByText(item.expectedText)).toBeNull();

      fireEvent.click(sysToggle);
      expect(sysToggle).toHaveAttribute("aria-expanded", "true");
      expect(within(card).getByText(item.expectedText)).toBeInTheDocument();
    }
  });
});
