import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { TurnCard } from "../components/TurnCard";
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
});
