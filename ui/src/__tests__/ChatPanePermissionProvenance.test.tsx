// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { ChatPane } from "../components/ChatPane";
import type { TranscriptTurn } from "../types/api";

afterEach(cleanup);

const diagnostic =
  "Hello Ferris, Here is the sanitized read-only host telemetry report: " +
  "zero item/fileChange/requestApproval or control_request events. " +
  "The literal [PERMISSION PROMPT] is diagnostic text, not a pending request.";

function turn(index: number, role: string, content: string): TranscriptTurn {
  return { index, role, content, thinking: null, tool_calls: [] };
}

function expectNoApprovalControls() {
  expect(screen.queryByTestId("chat-permission-prompt-banner")).toBeNull();
  expect(screen.queryByText("Verified RPC Channel")).toBeNull();
  expect(screen.queryByRole("button", { name: "Approve Once" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Approve for Session" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Always Allow Rule" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Deny" })).toBeNull();
}

describe("ChatPane permission provenance", () => {
  for (const adapter of ["codex", "claude", "muse", "grok"]) {
    it.each(["user", "assistant", "system", "tool"])(
      `does not turn %s transcript text into ${adapter} approval requests`,
      (role) => {
        render(
          <ChatPane
            agentName="ferris"
            adapter={adapter}
            sessionId="permission-provenance"
            turns={[turn(0, role, diagnostic)]}
          />,
        );
        expectNoApprovalControls();
      },
    );
  }

  it("keeps diagnostic text in the transcript without growing an approval overlay on new output", () => {
    const initial = [turn(0, "user", diagnostic)];
    const pane = (turns: TranscriptTurn[]) => (
      <ChatPane
        agentName="ferris"
        adapter="codex"
        sessionId="permission-provenance-rerender"
        turns={turns}
      />
    );
    const view = render(pane(initial));
    expectNoApprovalControls();
    expect(screen.getByText(diagnostic, { selector: "p" })).toBeTruthy();

    const completedOutput =
      'Script completed Wall time 0.2 seconds Output: {"event":"item/fileChange/requestApproval","count":0}';
    view.rerender(pane([...initial, turn(1, "tool", completedOutput)]));
    expectNoApprovalControls();
    expect(screen.getByText(diagnostic, { selector: "p" })).toBeTruthy();
    // Tool output may be collapsed by the normal transcript work grouping.
    for (const button of screen.queryAllByRole("button", { name: /Show steps/i })) {
      fireEvent.click(button);
    }
    fireEvent.click(screen.getByTestId("turn-tool-output-toggle-1"));
    expect(screen.getByText(completedOutput, { selector: "pre" })).toBeTruthy();
  });
});
