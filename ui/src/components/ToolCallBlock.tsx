import React from "react";
import type { TranscriptToolCall } from "../types/api";

export interface ToolCallBlockProps {
  tool: TranscriptToolCall;
  index?: number;
  defaultExpanded?: boolean;
}

export function formatToolArguments(args: unknown): string {
  if (args === undefined || args === null) {
    return "{}";
  }
  if (typeof args === "string") {
    try {
      const parsed = JSON.parse(args);
      return JSON.stringify(parsed, null, 2);
    } catch {
      return args;
    }
  }
  try {
    return JSON.stringify(args, null, 2);
  } catch {
    return String(args);
  }
}

export const ToolCallBlock: React.FC<ToolCallBlockProps> = ({
  tool,
  index,
  defaultExpanded = false,
}) => {
  const [expanded, setExpanded] = React.useState<boolean>(defaultExpanded);
  const formattedArgs = React.useMemo(() => formatToolArguments(tool.arguments), [tool.arguments]);

  const argKeysCount = React.useMemo(() => {
    if (tool.arguments && typeof tool.arguments === "object" && !Array.isArray(tool.arguments)) {
      return Object.keys(tool.arguments as Record<string, unknown>).length;
    }
    return 0;
  }, [tool.arguments]);

  return (
    <div
      className="fd-card"
      data-testid={`tool-call-block-${tool.name}`}
      style={{
        padding: "8px 10px",
        backgroundColor: "var(--subcard-bg, var(--bg-elevated))",
        borderColor: "var(--border-subtle)",
        marginBottom: "6px",
      }}
    >
      <button
        type="button"
        aria-expanded={expanded}
        onClick={() => setExpanded((prev) => !prev)}
        style={{
          width: "100%",
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: "8px",
          background: "transparent",
          border: "none",
          color: "var(--text-primary)",
          padding: 0,
          cursor: "pointer",
          fontFamily: "var(--font-mono)",
          fontSize: "12px",
          fontWeight: 600,
          textAlign: "left",
        }}
      >
        <span style={{ display: "inline-flex", alignItems: "center", gap: "8px" }}>
          <span style={{ color: "var(--status-info)", fontSize: "11px" }}>
            {expanded ? "▼" : "▶"}
          </span>
          <span className="fd-badge" style={{ color: "var(--status-info)" }}>
            TOOL
          </span>
          <span>{tool.name}</span>
        </span>
        <span className="fd-badge" style={{ fontSize: "10px", color: "var(--text-muted)" }}>
          {index !== undefined ? `#${index + 1} · ` : ""}
          {argKeysCount} {argKeysCount === 1 ? "arg" : "args"}
        </span>
      </button>

      {expanded && (
        <pre
          className="fd-mono"
          data-testid={`tool-call-args-${tool.name}`}
          style={{
            margin: "8px 0 0 0",
            padding: "8px 10px",
            borderRadius: "var(--radius-sm)",
            backgroundColor: "var(--bg-input)",
            border: "1px solid var(--border-subtle)",
            color: "var(--text-secondary)",
            fontSize: "12px",
            lineHeight: 1.45,
            whiteSpace: "pre-wrap",
            wordBreak: "break-word",
            overflowX: "auto",
          }}
        >
          {formattedArgs}
        </pre>
      )}
    </div>
  );
};
