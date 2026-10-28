import React from "react";
import { RichMarkdown } from "./RichMarkdown";

export interface ThinkingBlockProps {
  thinking: string;
  defaultExpanded?: boolean;
  /** True while this block belongs to the turn the agent is still producing. */
  isActive?: boolean;
}

export const ThinkingBlock: React.FC<ThinkingBlockProps> = ({
  thinking,
  defaultExpanded = false,
  isActive = false,
}) => {
  const [expanded, setExpanded] = React.useState<boolean>(defaultExpanded);

  const trimmed = thinking.trim();
  if (!trimmed) {
    return null;
  }

  const lineCount = trimmed.split(/\r?\n/).length;
  const charCount = trimmed.length;

  return (
    <div
      className="fd-card"
      data-testid="thinking-block"
      data-active={isActive ? "true" : "false"}
      style={{
        padding: "8px 10px",
        backgroundColor: "rgba(13, 17, 24, 0.72)",
        borderColor: "var(--border-subtle)",
        marginBottom: "8px",
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
          color: "var(--text-secondary)",
          padding: 0,
          cursor: "pointer",
          fontFamily: "var(--font-sans)",
          fontSize: "12px",
          fontWeight: 600,
          textAlign: "left",
        }}
      >
        <span style={{ display: "inline-flex", alignItems: "center", gap: "6px" }}>
          <span
            className="fd-mono"
            style={{ color: "var(--accent-primary)", fontSize: "11px" }}
          >
            {expanded ? "▼" : "▶"}
          </span>
          <span>Thinking...</span>
          {isActive && (
            <span
              className="fd-thinking-dot"
              data-testid="thinking-activity-dot"
              aria-hidden="true"
            />
          )}
        </span>
        <span className="fd-badge" style={{ fontSize: "10px", color: "var(--text-muted)" }}>
          {lineCount} {lineCount === 1 ? "line" : "lines"} · {charCount} chars
        </span>
      </button>

      {expanded && (
        <div
          data-testid="thinking-content"
          style={{
            margin: "8px 0 0 0",
            padding: "8px 10px",
            borderRadius: "var(--radius-sm)",
            backgroundColor: "var(--bg-input)",
            border: "1px solid var(--border-subtle)",
            color: "var(--text-muted)",
            fontSize: "12px",
            lineHeight: 1.5,
            wordBreak: "break-word",
            overflowX: "auto",
          }}
        >
          <RichMarkdown text={trimmed} />
        </div>
      )}
    </div>
  );
};
