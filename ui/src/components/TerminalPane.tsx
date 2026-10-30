import React, { useCallback, useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import type { HubWsClient } from "../api/ws";
import type {
  HubAuthority,
  HubWsFrame,
  TerminalClosePayload,
  TerminalExitedPayload,
  TerminalInputPayload,
  TerminalOpenedPayload,
  TerminalOpenPayload,
  TerminalOutputPayload,
  TerminalResizePayload,
} from "../types/api";

export interface TerminalPaneProps {
  authority?: HubAuthority | null;
  sessionId?: string | null;
  cwd?: string | null;
  wsClient?: HubWsClient | null;
  terminalFrames?: HubWsFrame[];
  onSendTerminalFrame?: (event: string, data: unknown) => void;
  onOpenPairing?: () => void;
}

const ANSI_ESCAPE_REGEX =
  // eslint-disable-next-line no-control-regex
  /\u001b\[[0-?]*[ -/]*[@-~]|\u001b\][^\u0007\u001b]*(?:\u0007|\u001b\\)|\u001b[@-Z\\-_]/g;

export function formatTerminalOutputLog(raw: string): string {
  if (!raw) {
    return "";
  }
  const stripped = raw
    .replace(ANSI_ESCAPE_REGEX, "")
    .replace(/\r\n/g, "\n")
    .replace(/\r/g, "\n");
  const lines = stripped.split("\n").filter((line) => {
    const lower = line.toLowerCase();
    if (lower.includes("cannot set terminal process group")) {
      return false;
    }
    if (lower.includes("no job control in this shell")) {
      return false;
    }
    return true;
  });
  return lines.join("\n").trimStart();
}

export const TerminalPane: React.FC<TerminalPaneProps> = ({
  authority,
  sessionId,
  cwd,
  wsClient,
  terminalFrames,
  onSendTerminalFrame,
  onOpenPairing,
}) => {
  const isOwner = authority?.tier === "owner";
  const effectiveSessionId = sessionId || "default-shell";
  const effectiveCwd = cwd || "/home/lmcfarlin/Projects/Artificial-Humanity";

  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);

  const [outputLog, setOutputLog] = useState<string>("");
  const [commandInput, setCommandInput] = useState<string>("");
  const [shellStatus, setShellStatus] = useState<string>("idle");
  const [colsRows, setColsRows] = useState<{ cols: number; rows: number }>({
    cols: 100,
    rows: 28,
  });

  const emitTerminalEvent = useCallback(
    (event: string, data: unknown) => {
      if (onSendTerminalFrame) {
        onSendTerminalFrame(event, data);
        return;
      }
      if (wsClient) {
        wsClient.sendFrame("terminal", event, data);
      }
    },
    [onSendTerminalFrame, wsClient],
  );

  const handleIncomingFrame = useCallback(
    (frame: HubWsFrame) => {
      if (frame.channel !== "terminal") {
        return;
      }
      if (frame.event === "opened") {
        const payload = frame.data as TerminalOpenedPayload;
        if (!payload.session_id || payload.session_id === effectiveSessionId) {
          setShellStatus(`connected${payload.pid ? ` (pid ${payload.pid})` : ""}`);
          if (payload.cols && payload.rows) {
            setColsRows({ cols: payload.cols, rows: payload.rows });
          }
        }
        return;
      }

      if (frame.event === "output") {
        const payload = frame.data as TerminalOutputPayload;
        if (payload.session_id && payload.session_id !== effectiveSessionId) {
          return;
        }
        const chunk = payload.data ?? payload.output ?? "";
        if (chunk) {
          setOutputLog((prev) => prev + chunk);
          try {
            termRef.current?.write(chunk);
          } catch {
            // Ignore xterm write errors in headless/jsdom environments.
          }
        }
        return;
      }

      if (frame.event === "exited") {
        const payload = frame.data as TerminalExitedPayload;
        if (!payload.session_id || payload.session_id === effectiveSessionId) {
          setShellStatus(`exited (${payload.exit_code})`);
        }
      }
    },
    [effectiveSessionId],
  );

  // Subscribe to wsClient terminal channel
  useEffect(() => {
    if (!isOwner || !wsClient) {
      return;
    }
    return wsClient.subscribe("terminal", handleIncomingFrame);
  }, [isOwner, wsClient, handleIncomingFrame]);

  // Process prop-supplied terminalFrames if provided
  useEffect(() => {
    if (!isOwner || !terminalFrames || terminalFrames.length === 0) {
      return;
    }
    const latest = terminalFrames[terminalFrames.length - 1];
    if (latest) {
      handleIncomingFrame(latest);
    }
  }, [isOwner, terminalFrames, handleIncomingFrame]);

  // Mount xterm.js and open shell session when Owner
  useEffect(() => {
    if (!isOwner) {
      return;
    }

    let term: Terminal | null = null;
    let fitAddon: FitAddon | null = null;
    let dataDisposable: { dispose: () => void } | null = null;
    let resizeDisposable: { dispose: () => void } | null = null;

    const isJsdom =
      typeof navigator !== "undefined" && /jsdom/i.test(navigator.userAgent ?? "");

    if (!isJsdom) {
      try {
        term = new Terminal({
          convertEol: true,
          cursorBlink: true,
          fontFamily:
            '"JetBrains Mono", "SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace',
          fontSize: 13,
          cols: 100,
          rows: 28,
          theme: {
            background: "#0A0D12",
            foreground: "#F1F5F9",
            cursor: "#F59E0B",
          },
        });
        fitAddon = new FitAddon();
        term.loadAddon(fitAddon);
        termRef.current = term;
        fitAddonRef.current = fitAddon;

        if (containerRef.current) {
          term.open(containerRef.current);
          fitAddon.fit();
        }

        dataDisposable = term.onData((data: string) => {
          const payload: TerminalInputPayload = {
            session_id: effectiveSessionId,
            data,
          };
          emitTerminalEvent("input", payload);
        });

        resizeDisposable = term.onResize(({ cols, rows }) => {
          setColsRows({ cols, rows });
          const payload: TerminalResizePayload = {
            session_id: effectiveSessionId,
            cols,
            rows,
          };
          emitTerminalEvent("resize", payload);
        });
      } catch {
        // Degrade gracefully in headless environments where canvas / layout metrics are unavailable.
      }
    }

    const openPayload: TerminalOpenPayload = {
      session_id: effectiveSessionId,
      cwd: effectiveCwd,
      cols: term?.cols ?? 100,
      rows: term?.rows ?? 28,
      command: ["/bin/bash", "-i"],
    };
    setShellStatus("connecting");
    emitTerminalEvent("open", openPayload);

    const handleWindowResize = () => {
      try {
        fitAddonRef.current?.fit();
      } catch {
        // Ignore in jsdom.
      }
      const cols = termRef.current?.cols ?? 100;
      const rows = termRef.current?.rows ?? 28;
      setColsRows({ cols, rows });
      const resizePayload: TerminalResizePayload = {
        session_id: effectiveSessionId,
        cols,
        rows,
      };
      emitTerminalEvent("resize", resizePayload);
    };

    window.addEventListener("resize", handleWindowResize);

    return () => {
      window.removeEventListener("resize", handleWindowResize);
      const closePayload: TerminalClosePayload = {
        session_id: effectiveSessionId,
      };
      emitTerminalEvent("close", closePayload);
      try {
        dataDisposable?.dispose();
        resizeDisposable?.dispose();
        term?.dispose();
      } catch {
        // Ignore disposal errors in jsdom.
      }
      termRef.current = null;
      fitAddonRef.current = null;
    };
  }, [isOwner, effectiveSessionId, effectiveCwd, emitTerminalEvent]);

  const handleReconnect = () => {
    if (!isOwner) {
      return;
    }
    const closePayload: TerminalClosePayload = {
      session_id: effectiveSessionId,
    };
    emitTerminalEvent("close", closePayload);
    const openPayload: TerminalOpenPayload = {
      session_id: effectiveSessionId,
      cwd: effectiveCwd,
      cols: colsRows.cols,
      rows: colsRows.rows,
      command: ["/bin/bash", "-i"],
    };
    setShellStatus("reconnecting");
    emitTerminalEvent("open", openPayload);
  };

  const handleResizeTrigger = () => {
    if (!isOwner) {
      return;
    }
    try {
      fitAddonRef.current?.fit();
    } catch {
      // Ignore in jsdom.
    }
    const cols = termRef.current?.cols ?? colsRows.cols;
    const rows = termRef.current?.rows ?? colsRows.rows;
    const resizePayload: TerminalResizePayload = {
      session_id: effectiveSessionId,
      cols,
      rows,
    };
    emitTerminalEvent("resize", resizePayload);
  };

  const handleQuickCommandSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!isOwner || !commandInput.trim()) {
      return;
    }
    const payload: TerminalInputPayload = {
      session_id: effectiveSessionId,
      data: `${commandInput}\n`,
    };
    emitTerminalEvent("input", payload);
    setCommandInput("");
  };

  if (!isOwner) {
    return (
      <section className="fd-pane" data-testid="terminal-pane">
        <div
          className="fd-scroll"
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
          }}
        >
          <div
            className="fd-card"
            data-testid="terminal-owner-required"
            style={{
              maxWidth: "480px",
              textAlign: "center",
              padding: "28px 24px",
              display: "flex",
              flexDirection: "column",
              gap: "12px",
              alignItems: "center",
            }}
          >
            <span
              className="fd-badge"
              style={{
                color: "#FCA5A5",
                borderColor: "rgba(239, 68, 68, 0.45)",
                backgroundColor: "rgba(239, 68, 68, 0.14)",
              }}
            >
              Owner Tier Only
            </span>
            <h3 style={{ margin: 0, fontSize: "15px", fontWeight: 700 }}>
              Owner authority required for interactive terminal sessions
            </h3>
            <p style={{ margin: 0, fontSize: "12px", color: "var(--text-secondary)" }}>
              Interactive PTY shell multiplexer sessions on AI-Lab-0 require authenticated Owner
              authority. Pair with a 10-minute code from <code className="fd-mono">fl hub pair</code>{" "}
              or enter your Owner bearer token.
            </p>
            {onOpenPairing && (
              <button
                type="button"
                className="fd-btn fd-btn-primary"
                onClick={onOpenPairing}
              >
                Authenticate as Owner
              </button>
            )}
          </div>
        </div>
      </section>
    );
  }

  return (
    <section className="fd-pane" data-testid="terminal-pane">
      {/* Terminal Toolbar */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: "8px",
          padding: "8px 16px",
          borderBottom: "1px solid var(--border-subtle)",
          backgroundColor: "var(--bg-surface)",
          flexWrap: "wrap",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
          <span className="fd-badge fd-mono">{effectiveSessionId}</span>
          <span className="fd-badge fd-mono" style={{ color: "var(--text-muted)" }}>
            {effectiveCwd}
          </span>
          <span className="fd-badge" data-testid="terminal-status-badge">
            {shellStatus}
          </span>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
          <button type="button" className="fd-btn" onClick={handleResizeTrigger}>
            Fit ({colsRows.cols}×{colsRows.rows})
          </button>
          <button
            type="button"
            className="fd-btn"
            onClick={() => {
              setOutputLog("");
              try {
                termRef.current?.clear();
              } catch {
                // Ignore
              }
            }}
          >
            Clear
          </button>
          <button
            type="button"
            className="fd-btn fd-btn-primary"
            onClick={handleReconnect}
          >
            Reconnect / New Shell
          </button>
        </div>
      </div>

      {/* Terminal Viewport + Accessible Output Buffer */}
      <div
        className="fd-scroll"
        style={{
          display: "flex",
          flexDirection: "column",
          gap: "12px",
          backgroundColor: "#0A0D12",
        }}
      >
        <div
          ref={containerRef}
          className="fd-terminal-viewport"
          data-testid="xterm-container"
          style={{
            flex: 1,
            minHeight: "260px",
            borderRadius: "var(--radius-md)",
            border: "1px solid var(--border-subtle)",
            padding: "6px",
            backgroundColor: "#0A0D12",
            textAlign: "left",
          }}
        />

        <pre
          data-testid="terminal-output-log"
          aria-live="polite"
          className="fd-mono"
          style={{
            margin: 0,
            padding: "10px 12px",
            borderRadius: "var(--radius-md)",
            border: "1px solid var(--border-subtle)",
            backgroundColor: "var(--bg-surface)",
            color: "var(--text-secondary)",
            fontSize: "12px",
            maxHeight: "180px",
            overflowY: "auto",
            whiteSpace: "pre-wrap",
            wordBreak: "break-word",
            textAlign: "left",
          }}
        >
          {formatTerminalOutputLog(outputLog) || "Ready for interactive PTY output..."}
        </pre>
      </div>

      {/* Quick Command Input Bar */}
      <form
        onSubmit={handleQuickCommandSubmit}
        style={{
          borderTop: "1px solid var(--border-subtle)",
          backgroundColor: "var(--bg-surface)",
          padding: "10px 16px",
          display: "flex",
          gap: "8px",
          alignItems: "center",
        }}
      >
        <input
          type="text"
          aria-label="Shell Command"
          placeholder="Enter shell command (e.g. fl status)..."
          className="fd-input fd-mono"
          value={commandInput}
          onChange={(e) => setCommandInput(e.target.value)}
          style={{ flex: 1 }}
        />
        <button
          type="submit"
          className="fd-btn fd-btn-primary"
          disabled={!commandInput.trim()}
        >
          Send Command
        </button>
      </form>
    </section>
  );
};
