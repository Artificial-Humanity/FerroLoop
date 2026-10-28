import { render, screen } from "@testing-library/react";
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

  it("does not highlight wire turns as directed replies", () => {
    render(
      <TurnCard
        turn={turn({
          index: 4,
          role: "wire:jarvis",
          content: "Subject: Review ready\nBody for the operator to read.",
        })}
      />,
    );

    expect(screen.getByTestId("turn-card-4")).not.toHaveAttribute(
      "data-directed-assistant",
      "true",
    );
  });
});
