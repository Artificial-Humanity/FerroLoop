import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { TerminalPane } from "../components/TerminalPane";
import type { HubWsFrame } from "../types/api";

describe("TerminalPane", () => {
  it("renders non-Owner authority refusal banner and triggers onOpenPairing when not Owner", () => {
    const onOpenPairing = vi.fn();
    const onSendTerminalFrame = vi.fn();

    render(
      <TerminalPane
        authority={{ tier: "peer", agent: "wesley" }}
        sessionId="term-sess-01"
        cwd="/home/lmcfarlin/worktrees/fl-ferrodeck-ui"
        onOpenPairing={onOpenPairing}
        onSendTerminalFrame={onSendTerminalFrame}
      />,
    );

    expect(
      screen.getByText(/Owner authority required for interactive terminal sessions/i),
    ).toBeInTheDocument();
    expect(onSendTerminalFrame).not.toHaveBeenCalled();

    const pairBtn = screen.getByRole("button", { name: /Authenticate as Owner|Open Pairing/i });
    fireEvent.click(pairBtn);
    expect(onOpenPairing).toHaveBeenCalledTimes(1);
  });

  it("manages Owner terminal open, output streaming, input dispatch, and close lifecycle", async () => {
    const onSendTerminalFrame = vi.fn();
    let subscribedHandler: ((frame: HubWsFrame) => void) | null = null;

    const mockWsClient = {
      subscribe: vi.fn((channel: string, cb: (frame: HubWsFrame) => void) => {
        if (channel === "terminal") {
          subscribedHandler = cb;
        }
        return () => {
          subscribedHandler = null;
        };
      }),
      sendFrame: vi.fn((channel: string, event: string, data: unknown) => {
        onSendTerminalFrame(event, data);
        return true;
      }),
    };

    const { unmount } = render(
      <TerminalPane
        authority={{ tier: "owner", login: "lmcfarlin" }}
        sessionId="term-owner-01"
        cwd="/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop"
        wsClient={mockWsClient as never}
        onSendTerminalFrame={onSendTerminalFrame}
      />,
    );

    // 1. Sends "open" event on mount
    await waitFor(() => {
      expect(onSendTerminalFrame).toHaveBeenCalledWith(
        "open",
        expect.objectContaining({
          session_id: "term-owner-01",
          cwd: "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop",
          command: ["/bin/bash", "-i"],
        }),
      );
    });

    // 2. Streams "output" into accessible terminal-output-log
    expect(subscribedHandler).not.toBeNull();
    act(() => {
      subscribedHandler?.({
        channel: "terminal",
        event: "output",
        data: {
          session_id: "term-owner-01",
          stream: "stdout",
          data: "lmcfarlin@ai-lab-0:~/FerroLoop$ fl status\r\nAll systems operational\r\n",
        },
      });
    });

    const outputLog = screen.getByTestId("terminal-output-log");
    expect(outputLog).toHaveTextContent(/All systems operational/i);

    // 3. Dispatches command input via quick-command bar
    const cmdInput = screen.getByPlaceholderText(/Enter shell command/i) as HTMLInputElement;
    fireEvent.change(cmdInput, { target: { value: "cargo test --workspace" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Command/i }));

    await waitFor(() => {
      expect(onSendTerminalFrame).toHaveBeenCalledWith(
        "input",
        expect.objectContaining({
          session_id: "term-owner-01",
          data: "cargo test --workspace\n",
        }),
      );
    });

    // 4. Clicking Reconnect / New Shell emits "close" followed by "open"
    onSendTerminalFrame.mockClear();
    fireEvent.click(screen.getByRole("button", { name: /Reconnect \/ New Shell/i }));
    expect(onSendTerminalFrame).toHaveBeenNthCalledWith(
      1,
      "close",
      expect.objectContaining({
        session_id: "term-owner-01",
      }),
    );
    expect(onSendTerminalFrame).toHaveBeenNthCalledWith(
      2,
      "open",
      expect.objectContaining({
        session_id: "term-owner-01",
        cwd: "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop",
        command: ["/bin/bash", "-i"],
      }),
    );

    // 5. Sends "close" event on unmount
    onSendTerminalFrame.mockClear();
    unmount();
    expect(onSendTerminalFrame).toHaveBeenCalledWith(
      "close",
      expect.objectContaining({
        session_id: "term-owner-01",
      }),
    );
  });
});
