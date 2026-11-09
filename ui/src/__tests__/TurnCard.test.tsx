import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import {
  isOperatorUserTurn,
  normalizeOperatorPromptForMatch,
  stripAgyUserRequestWrapper,
  stripOperatorPromptHeader,
  stripOuterPastedContentWrapper,
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

  it("strips <USER_SETTINGS_CHANGE> and <CONTEXT_SUMMARY> from operator turns", () => {
    const withWrapperAndSettings = [
      "<USER_REQUEST>",
      "Please review the latest PR.",
      "</USER_REQUEST>",
      "<USER_SETTINGS_CHANGE>",
      "The user changed setting Model Selection to Gemini 2.5 Pro.",
      "</USER_SETTINGS_CHANGE>",
      "<ADDITIONAL_METADATA>",
      "The current local time is: 2026-11-02T16:00:00Z.",
      "</ADDITIONAL_METADATA>",
    ].join("\n");

    const postBackendUnwrappedWithSettings = [
      "Please review the latest PR.",
      "<USER_SETTINGS_CHANGE>",
      "The user changed setting Model Selection to Gemini 2.5 Pro.",
      "</USER_SETTINGS_CHANGE>",
    ].join("\n");

    const withContextSummaryAndRequest = [
      "<CONTEXT_SUMMARY>",
      "Previous session summary details.",
      "</CONTEXT_SUMMARY>",
      "<USER_REQUEST>",
      "You are Felix.",
      "</USER_REQUEST>",
    ].join("\n");

    expect(stripAgyUserRequestWrapper(withWrapperAndSettings)).toBe(
      "Please review the latest PR.",
    );
    expect(stripAgyUserRequestWrapper(postBackendUnwrappedWithSettings)).toBe(
      "Please review the latest PR.",
    );
    expect(stripAgyUserRequestWrapper(withContextSummaryAndRequest)).toBe("You are Felix.");

    render(
      <>
        <TurnCard
          turn={turn({
            index: 18,
            role: "user",
            content: withWrapperAndSettings,
          })}
        />
        <TurnCard
          turn={turn({
            index: 19,
            role: "user",
            content: postBackendUnwrappedWithSettings,
          })}
        />
        <TurnCard
          turn={turn({
            index: 20,
            role: "user",
            content: withContextSummaryAndRequest,
          })}
        />
      </>,
    );

    for (const [idx, expected] of [
      [18, /Please review the latest PR\./],
      [19, /Please review the latest PR\./],
      [20, /You are Felix\./],
    ] as const) {
      const card = screen.getByTestId(`turn-card-${idx}`);
      expect(within(card).getByText("Operator")).toBeInTheDocument();
      expect(within(card).getByText(expected)).toBeInTheDocument();
      expect(within(card).queryByText(/USER_SETTINGS_CHANGE/)).toBeNull();
      expect(within(card).queryByText(/Model Selection/)).toBeNull();
      expect(within(card).queryByText(/CONTEXT_SUMMARY/)).toBeNull();
      expect(within(card).queryByText(/Previous session summary/)).toBeNull();
    }
  });

  it("classifies Grok <user_info>/<rules> turns and standalone <CONTEXT_SUMMARY>/<USER_SETTINGS_CHANGE> turns as collapsed System turns", () => {
    const grokSystemTurn = [
      "<user_info>",
      "OS Version: linux",
      "Workspace: /home/lmcfarlin/Projects/Artificial-Humanity",
      "</user_info>",
      "",
      "<rules>",
      "Follow workspace instructions in AGENTS.md.",
      "</rules>",
    ].join("\n");

    const standaloneSettingsTurn = [
      "<USER_SETTINGS_CHANGE>",
      "The user changed setting Model Selection to Gemini 2.5 Pro.",
      "</USER_SETTINGS_CHANGE>",
    ].join("\n");

    const standaloneContextSummaryTurn = [
      "<CONTEXT_SUMMARY>",
      "Compacted conversation summary.",
      "</CONTEXT_SUMMARY>",
    ].join("\n");

    expect(
      isOperatorUserTurn(turn({ index: 21, role: "user", content: grokSystemTurn })),
    ).toBe(false);
    expect(
      isOperatorUserTurn(turn({ index: 22, role: "user", content: standaloneSettingsTurn })),
    ).toBe(false);
    expect(
      isOperatorUserTurn(
        turn({ index: 23, role: "user", content: standaloneContextSummaryTurn }),
      ),
    ).toBe(false);

    render(
      <>
        <TurnCard
          turn={turn({
            index: 21,
            role: "user",
            content: grokSystemTurn,
          })}
        />
        <TurnCard
          turn={turn({
            index: 22,
            role: "user",
            content: standaloneSettingsTurn,
          })}
        />
        <TurnCard
          turn={turn({
            index: 23,
            role: "user",
            content: standaloneContextSummaryTurn,
          })}
        />
      </>,
    );

    for (const [idx, expectedSnippet] of [
      [21, /Follow workspace instructions in AGENTS\.md\./],
      [22, /The user changed setting Model Selection/],
      [23, /Compacted conversation summary\./],
    ] as const) {
      const card = screen.getByTestId(`turn-card-${idx}`);
      expect(within(card).getByText("System")).toBeInTheDocument();
      expect(within(card).queryByText("Operator")).toBeNull();
      expect(card).not.toHaveAttribute("data-directed-assistant", "true");

      const sysToggle = within(card).getByTestId(`turn-system-output-toggle-${idx}`);
      expect(sysToggle).toHaveAttribute("aria-expanded", "false");
      expect(within(card).queryByText(expectedSnippet)).toBeNull();

      fireEvent.click(sysToggle);
      expect(sysToggle).toHaveAttribute("aria-expanded", "true");
      expect(within(card).getByText(expectedSnippet)).toBeInTheDocument();
    }
  });

  it("hides outer <pasted_content id=\"X\"> wrapper only when opening and closing ids match and leaves mismatched or prose tags untouched", () => {
    const matchingWrapper = [
      '<pasted_content id="1234">',
      "First line of operator prompt",
      "Second line of operator prompt",
      '</pasted_content id="1234">',
    ].join("\n");

    const mismatchedWrapper = [
      '<pasted_content id="1234">',
      "Mismatched closing tag should stay visible",
      '</pasted_content id="5678">',
    ].join("\n");

    const literalInProse = [
      "Look at the wrapper tag in this snippet:",
      '<pasted_content id="1234">inner literal</pasted_content id="1234">',
    ].join("\n");

    const turn304Shape = [
      '<pasted_content id="9999">',
      "[Operator Prompt via FerroDeck — from lmcfarlin]",
      "",
      "Why does my message have:",
      '<\\pasted_content id="1234">literal inner tag</\\pasted_content id="1234">',
      '</pasted_content id="9999">',
    ].join("\n");

    const plainCodexTurn = "Plain Codex user turn without any paste wrapper.";

    expect(stripOuterPastedContentWrapper(matchingWrapper)).toEqual({
      text: "First line of operator prompt\nSecond line of operator prompt",
      unwrapped: true,
    });
    expect(stripOuterPastedContentWrapper(mismatchedWrapper)).toEqual({
      text: mismatchedWrapper,
      unwrapped: false,
    });
    expect(stripOuterPastedContentWrapper(literalInProse)).toEqual({
      text: literalInProse,
      unwrapped: false,
    });
    expect(stripAgyUserRequestWrapper(turn304Shape)).toBe(
      'Why does my message have:\n<pasted_content id="1234">literal inner tag</pasted_content id="1234">',
    );
    expect(stripAgyUserRequestWrapper(plainCodexTurn)).toBe(plainCodexTurn);

    render(
      <>
        <TurnCard
          turn={turn({
            index: 24,
            role: "user",
            content: matchingWrapper,
          })}
        />
        <TurnCard
          turn={turn({
            index: 25,
            role: "user",
            content: mismatchedWrapper,
          })}
        />
        <TurnCard
          turn={turn({
            index: 26,
            role: "user",
            content: literalInProse,
          })}
        />
        <TurnCard
          turn={turn({
            index: 27,
            role: "user",
            content: turn304Shape,
          })}
        />
        <TurnCard
          turn={turn({
            index: 28,
            role: "user",
            content: plainCodexTurn,
          })}
        />
      </>,
    );

    const matchedCard = screen.getByTestId("turn-card-24");
    expect(within(matchedCard).getByText(/First line of operator prompt/)).toBeInTheDocument();
    expect(within(matchedCard).queryByText(/pasted_content/)).toBeNull();

    const mismatchedCard = screen.getByTestId("turn-card-25");
    expect(within(mismatchedCard).getByText(/pasted_content id="1234"/)).toBeInTheDocument();
    expect(within(mismatchedCard).getByText(/pasted_content id="5678"/)).toBeInTheDocument();

    const proseCard = screen.getByTestId("turn-card-26");
    expect(
      within(proseCard).getByText(/<pasted_content id="1234">inner literal<\/pasted_content id="1234">/),
    ).toBeInTheDocument();

    const turn304Card = screen.getByTestId("turn-card-27");
    expect(within(turn304Card).queryByText(/id="9999"/)).toBeNull();
    expect(within(turn304Card).queryByText(/Operator Prompt via FerroDeck/)).toBeNull();
    expect(
      within(turn304Card).getByText(
        /<pasted_content id="1234">literal inner tag<\/pasted_content id="1234">/,
      ),
    ).toBeInTheDocument();

    const codexCard = screen.getByTestId("turn-card-28");
    expect(
      within(codexCard).getByText("Plain Codex user turn without any paste wrapper."),
    ).toBeInTheDocument();
  });

  it("hides [Operator Prompt via FerroDeck — from <name>] when sender is owner/lmcfarlin and preserves it for other senders", () => {
    const fromOwner = "[Operator Prompt via FerroDeck — from owner]\n\nShip the fix.";
    const fromLmcfarlin = "[Operator Prompt via FerroDeck — from lmcfarlin]\n\nCheck the logs.";
    const bareHeader = "[Operator Prompt via FerroDeck]\n\nRun tests.";
    const fromFerris = "[Operator Prompt via FerroDeck — from ferris]\n\nCoordinating across agents.";

    expect(stripOperatorPromptHeader(fromOwner)).toBe("Ship the fix.");
    expect(stripOperatorPromptHeader(fromLmcfarlin)).toBe("Check the logs.");
    expect(stripOperatorPromptHeader(bareHeader)).toBe("Run tests.");
    expect(stripOperatorPromptHeader(fromFerris)).toBe(fromFerris);
    expect(stripOperatorPromptHeader("[Operator Prompt via FerroDeck — from owner]")).toBe(
      "[Operator Prompt via FerroDeck — from owner]",
    );
    expect(stripOperatorPromptHeader("[Operator Prompt via FerroDeck — from owner]\n   ")).toBe(
      "[Operator Prompt via FerroDeck — from owner]",
    );

    render(
      <>
        <TurnCard turn={turn({ index: 29, role: "user", content: fromLmcfarlin })} />
        <TurnCard turn={turn({ index: 30, role: "user", content: fromFerris })} />
      </>,
    );

    const ownerCard = screen.getByTestId("turn-card-29");
    expect(within(ownerCard).getByText("Check the logs.")).toBeInTheDocument();
    expect(within(ownerCard).queryByText(/Operator Prompt via FerroDeck/)).toBeNull();

    const ferrisCard = screen.getByTestId("turn-card-30");
    expect(
      within(ferrisCard).getByText(/\[Operator Prompt via FerroDeck — from ferris\]/),
    ).toBeInTheDocument();
    expect(within(ferrisCard).getByText(/Coordinating across agents\./)).toBeInTheDocument();
  });

  it("copies cleaned turn text (without outer paste wrapper or owner header) when clicking Copy on a TurnCard", () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });

    const wrappedTurn = [
      '<pasted_content id="4321">',
      "[Operator Prompt via FerroDeck — from lmcfarlin]",
      "",
      "Cleaned copy payload line 1",
      '<\\pasted_content id="1111">kept inner tag</\\pasted_content id="1111">',
      '</pasted_content id="4321">',
    ].join("\n");

    render(
      <TurnCard
        turn={turn({
          index: 31,
          role: "user",
          content: wrappedTurn,
        })}
      />,
    );

    const copyBtn = screen.getByTestId("turn-copy-btn-31");
    expect(copyBtn).toHaveTextContent("Copy");
    fireEvent.click(copyBtn);

    expect(writeText).toHaveBeenCalledWith(
      'Cleaned copy payload line 1\n<pasted_content id="1111">kept inner tag</pasted_content id="1111">',
    );
    expect(copyBtn).toHaveTextContent("Copied");
    expect(
      normalizeOperatorPromptForMatch(wrappedTurn),
    ).toBe(
      'Cleaned copy payload line 1 <pasted_content id="1111">kept inner tag</pasted_content id="1111">',
    );

    const soleNestedAfterHeader = [
      '<pasted_content id="9999">',
      "[Operator Prompt via FerroDeck — from owner]",
      "",
      '<\\pasted_content id="1234">literal</\\pasted_content id="1234">',
      '</pasted_content id="9999">',
    ].join("\n");
    expect(normalizeOperatorPromptForMatch(soleNestedAfterHeader)).toBe(
      '<pasted_content id="1234">literal</pasted_content id="1234">',
    );

    const attrlessNested = [
      '<pasted_content id="9999">',
      "<\\pasted_content>body<\\/pasted_content> and </\\pasted_content>",
      '</pasted_content id="9999">',
    ].join("\n");
    expect(stripOuterPastedContentWrapper(attrlessNested)).toEqual({
      text: "<pasted_content>body</pasted_content> and </pasted_content>",
      unwrapped: true,
    });
  });
});
