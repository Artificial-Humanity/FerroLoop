import React from "react";
import type { TranscriptTurn } from "../types/api";
import { ThinkingBlock } from "./ThinkingBlock";
import { ToolCallBlock } from "./ToolCallBlock";

export interface TurnCardProps {
  turn: TranscriptTurn;
  agentDisplayName?: string;
}

interface ContentSegment {
  kind: "prose" | "code";
  language?: string;
  text: string;
}

export function splitTurnContent(content: string): ContentSegment[] {
  if (!content) {
    return [];
  }
  const segments: ContentSegment[] = [];
  const fenceRegex = /```([a-zA-Z0-9_-]*)\n?([\s\S]*?)```/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null = fenceRegex.exec(content);

  while (match !== null) {
    if (match.index > lastIndex) {
      const prose = content.slice(lastIndex, match.index);
      if (prose.trim().length > 0) {
        segments.push({ kind: "prose", text: prose });
      }
    }
    segments.push({
      kind: "code",
      language: match[1] ? match[1].trim() : undefined,
      text: match[2].replace(/\n$/, ""),
    });
    lastIndex = match.index + match[0].length;
    match = fenceRegex.exec(content);
  }

  if (lastIndex < content.length) {
    const trailing = content.slice(lastIndex);
    if (trailing.trim().length > 0) {
      segments.push({ kind: "prose", text: trailing });
    }
  }

  return segments;
}

export const TurnCard: React.FC<TurnCardProps> = ({ turn, agentDisplayName }) => {
  const roleNormalized = (turn.role || "assistant").toLowerCase();
  const isUser = roleNormalized === "user";
  const isSystem = roleNormalized === "system";

  const roleLabel = isUser
    ? "Operator"
    : isSystem
      ? "System"
      : agentDisplayName || "Assistant";

  const roleBorderColor = isUser
    ? "var(--border-accent)"
    : isSystem
      ? "rgba(56, 189, 248, 0.35)"
      : "var(--border-subtle)";

  const roleBadgeStyle: React.CSSProperties = isUser
    ? {
        backgroundColor: "var(--accent-subtle)",
        borderColor: "var(--border-accent)",
        color: "var(--accent-hover)",
      }
    : isSystem
      ? {
          backgroundColor: "rgba(56, 189, 248, 0.12)",
          borderColor: "rgba(56, 189, 248, 0.35)",
          color: "var(--status-info)",
        }
      : {
          backgroundColor: "var(--bg-elevated)",
          borderColor: "var(--border-subtle)",
          color: "var(--text-primary)",
        };

  const segments = React.useMemo(() => splitTurnContent(turn.content), [turn.content]);
  const hasThinking = Boolean(turn.thinking && turn.thinking.trim().length > 0);
  const toolCalls = turn.tool_calls ?? [];

  return (
    <article
      className="fd-card"
      data-testid={`turn-card-${turn.index}`}
      style={{
        borderColor: roleBorderColor,
        backgroundColor: isUser ? "rgba(23, 30, 41, 0.55)" : "var(--bg-surface)",
        display: "flex",
        flexDirection: "column",
        gap: "8px",
      }}
    >
      <header
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: "8px",
          flexWrap: "wrap",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
          <span className="fd-badge" style={roleBadgeStyle}>
            {roleLabel}
          </span>
          <span className="fd-mono" style={{ fontSize: "11px", color: "var(--text-dim)" }}>
            #{turn.index}
          </span>
        </div>

        {turn.created_at && (
          <time
            dateTime={turn.created_at}
            className="fd-mono"
            style={{ fontSize: "11px", color: "var(--text-dim)" }}
          >
            {turn.created_at}
          </time>
        )}
      </header>

      {hasThinking && turn.thinking && <ThinkingBlock thinking={turn.thinking} />}

      {toolCalls.length > 0 && (
        <div
          data-testid={`turn-tools-${turn.index}`}
          style={{ display: "flex", flexDirection: "column", gap: "4px" }}
        >
          {toolCalls.map((tool, idx) => (
            <ToolCallBlock
              key={`${tool.name}-${idx}`}
              tool={tool}
              index={idx}
            />
          ))}
        </div>
      )}

      {segments.length > 0 && (
        <div
          data-testid={`turn-content-${turn.index}`}
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "8px",
            color: "var(--text-primary)",
            fontSize: "13.5px",
            lineHeight: 1.55,
          }}
        >
          {segments.map((seg, idx) =>
            seg.kind === "code" ? (
              <div
                key={idx}
                style={{
                  borderRadius: "var(--radius-sm)",
                  border: "1px solid var(--border-subtle)",
                  backgroundColor: "var(--bg-input)",
                  overflow: "hidden",
                }}
              >
                {seg.language && (
                  <div
                    className="fd-mono"
                    style={{
                      padding: "4px 10px",
                      fontSize: "10px",
                      color: "var(--text-muted)",
                      borderBottom: "1px solid var(--border-subtle)",
                      backgroundColor: "rgba(23, 30, 41, 0.7)",
                    }}
                  >
                    {seg.language}
                  </div>
                )}
                <pre
                  className="fd-mono"
                  style={{
                    margin: 0,
                    padding: "10px 12px",
                    fontSize: "12px",
                    lineHeight: 1.45,
                    color: "var(--text-secondary)",
                    whiteSpace: "pre-wrap",
                    wordBreak: "break-word",
                    overflowX: "auto",
                  }}
                >
                  <code>{seg.text}</code>
                </pre>
              </div>
            ) : (
              <div
                key={idx}
                style={{
                  whiteSpace: "pre-wrap",
                  wordBreak: "break-word",
                }}
              >
                {seg.text.trim()}
              </div>
            ),
          )}
        </div>
      )}
    </article>
  );
};
