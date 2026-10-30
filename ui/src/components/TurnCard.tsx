import React from "react";
import type { TranscriptTurn } from "../types/api";
import { dispatchOpenArtifact, MermaidDiagram, RichMarkdown } from "./RichMarkdown";
import { ThinkingBlock } from "./ThinkingBlock";
import { ToolCallBlock } from "./ToolCallBlock";

export interface TurnCardProps {
  turn: TranscriptTurn;
  agentDisplayName?: string;
  /** True for the latest turn while the session is busy (live thinking). */
  isActive?: boolean;
  onOpenArtifact?: (pathOrUrl: string) => void;
}

/** One-line collapsed summary for a Wire message body. */
export function summarizeWireContent(
  content: string,
  wireSender?: string | null,
): string {
  const raw = content || "";
  const lines = raw.split("\n");
  const lineLabel = `${lines.length} ${lines.length === 1 ? "line" : "lines"}`;
  const label = wireSender ? `Wire · ${wireSender}` : "Wire";
  const subject = raw.match(/^\s*Subject:\s*(.+)$/m)?.[1]?.trim();
  if (subject) {
    const preview = subject.length > 80 ? `${subject.slice(0, 80)}…` : subject;
    return `${label} — ${preview} (${lineLabel})`;
  }
  const firstLine = lines.find((line) => line.trim().length > 0)?.trim() ?? "";
  if (!firstLine) {
    return `${label} (${lineLabel})`;
  }
  const preview = firstLine.length > 80 ? `${firstLine.slice(0, 80)}…` : firstLine;
  return `${label} — ${preview} (${lineLabel})`;
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

export interface ParsedTurnAttachment {
  path: string;
  url?: string;
  name: string;
  isImage: boolean;
}

/**
 * Strip Antigravity web/mobile `<USER_REQUEST>` and `<ADDITIONAL_METADATA>` wrappers
 * (and `<EPHEMERAL_MESSAGE>` outer tags) so operator and notice turns render cleanly.
 */
export function stripAgyUserRequestWrapper(content: string): string {
  if (!content) {
    return "";
  }
  if (!/^\s*<(?:USER_REQUEST|ADDITIONAL_METADATA|EPHEMERAL_MESSAGE)>/i.test(content)) {
    return content;
  }
  return content
    .replace(/<ADDITIONAL_METADATA>[\s\S]*?<\/ADDITIONAL_METADATA>/gi, "")
    .replace(/<USER_REQUEST>([\s\S]*?)<\/USER_REQUEST>/gi, (_full, inner: string) =>
      inner.trim(),
    )
    .replace(
      /<EPHEMERAL_MESSAGE>([\s\S]*?)<\/EPHEMERAL_MESSAGE>/gi,
      (_full, inner: string) => inner.trim(),
    )
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

export function extractTurnAttachments(
  content: string,
  options?: { stripAgyWrapper?: boolean },
): {
  cleanedContent: string;
  attachments: ParsedTurnAttachment[];
} {
  if (!content) {
    return { cleanedContent: "", attachments: [] };
  }
  const shouldStrip = options?.stripAgyWrapper ?? true;
  const stripped = shouldStrip ? stripAgyUserRequestWrapper(content) : content;
  const attachments: ParsedTurnAttachment[] = [];
  const attachmentRegex = /\[Attached file:\s*([^\])\n]+?)(?:\s+\(([^)\n]+)\))?\]/g;
  const cleanedContent = stripped
    .replace(attachmentRegex, (_full, rawPath: string, rawUrl?: string) => {
      const path = rawPath.trim();
      const url = rawUrl ? rawUrl.trim() : undefined;
      const parts = path.split("/");
      const name = parts[parts.length - 1] || path;
      const target = (url || path).toLowerCase();
      const isImage = /\.(png|jpe?g|webp|gif|svg)(\?.*)?$/i.test(target);
      attachments.push({ path, url, name, isImage });
      return "";
    })
    .replace(/\n{3,}/g, "\n\n")
    .trim();

  return { cleanedContent, attachments };
}

export const TurnCard: React.FC<TurnCardProps> = ({
  turn,
  agentDisplayName,
  isActive = false,
  onOpenArtifact,
}) => {
  const roleNormalized = (turn.role || "assistant").toLowerCase();
  const trimmedStart = turn.content.trimStart();
  const shouldStripAgyWrapper =
    roleNormalized === "user" || roleNormalized === "system";
  const strippedForNotice = shouldStripAgyWrapper
    ? stripAgyUserRequestWrapper(turn.content).trimStart()
    : trimmedStart;
  // Defensive: hub normalization can mislabel background task completions
  // as operator turns; the content shape is unambiguous tool output.
  const looksLikeBackgroundTaskOutput = /^Task id "[^"]+" finished with result:/i.test(
    trimmedStart,
  );
  // Defensive: raw system/code snippet dumps can be mislabeled as user turns.
  const looksLikeSystemCodeDump = /^"\)\r?\n\d+:\s/.test(trimmedStart);
  // Defensive: Antigravity [Notice], <EPHEMERAL_MESSAGE>, and non-operator <SYSTEM_MESSAGE>
  // blocks arrive with role === "user" but represent system notices.
  const looksLikeSystemNotice =
    /^\[Notice\]\s/i.test(trimmedStart) ||
    /^\[Notice\]\s/i.test(strippedForNotice) ||
    /^<EPHEMERAL_MESSAGE>/i.test(trimmedStart) ||
    (/^<SYSTEM_MESSAGE>/i.test(trimmedStart) &&
      !trimmedStart.includes("[Operator Prompt via FerroDeck"));
  const isUser =
    roleNormalized === "user" &&
    !looksLikeBackgroundTaskOutput &&
    !looksLikeSystemCodeDump &&
    !looksLikeSystemNotice;
  const isWire = roleNormalized.startsWith("wire:");
  const wireSender = isWire ? turn.role.slice(5) : null;
  const isSystem =
    roleNormalized === "system" ||
    (roleNormalized === "user" && (looksLikeSystemCodeDump || looksLikeSystemNotice));
  const isToolOutput =
    roleNormalized === "tool" ||
    looksLikeBackgroundTaskOutput ||
    (!isUser &&
      !isSystem &&
      !isWire &&
      trimmedStart.startsWith("Created At:") &&
      (!turn.tool_calls || turn.tool_calls.length === 0));

  const [toolOutputExpanded, setToolOutputExpanded] = React.useState(false);
  const [wireExpanded, setWireExpanded] = React.useState(false);
  const [systemExpanded, setSystemExpanded] = React.useState(false);
  const [expandedCodeBlocks, setExpandedCodeBlocks] = React.useState<Record<number, boolean>>({});

  const roleLabel = isUser
    ? "Operator"
    : isWire
      ? wireSender
        ? `Wire · ${wireSender}`
        : "Wire"
      : isSystem
        ? "System"
        : isToolOutput
          ? "Tool Output"
          : agentDisplayName || "Assistant";

  const { cleanedContent, attachments } = React.useMemo(
    () =>
      extractTurnAttachments(turn.content, {
        stripAgyWrapper: shouldStripAgyWrapper,
      }),
    [turn.content, shouldStripAgyWrapper],
  );

  // Prose agent output addressed to the operator is the conversation's
  // other first-class voice; thinking/tool-only turns stay subdued.
  const isDirectedAssistant =
    !isUser &&
    !isWire &&
    !isSystem &&
    !isToolOutput &&
    cleanedContent.trim().length > 0;

  const roleBorderColor = isUser
    ? "var(--border-accent)"
    : isDirectedAssistant
      ? "var(--card-assistant-border)"
      : isWire
        ? "var(--wire-card-border, rgba(180, 120, 70, 0.42))"
        : "var(--border-subtle)";

  const roleBadgeStyle: React.CSSProperties = isUser
    ? {
        backgroundColor: "var(--accent-subtle)",
        borderColor: "var(--border-accent)",
        color: "var(--accent-hover)",
      }
    : isDirectedAssistant
      ? {
          backgroundColor: "var(--badge-assistant-bg)",
          borderColor: "var(--badge-assistant-border)",
          color: "var(--status-info)",
        }
      : isWire
        ? {
            backgroundColor: "rgba(180, 120, 70, 0.12)",
            borderColor: "var(--wire-card-border, rgba(180, 120, 70, 0.42))",
            color: "var(--wire-badge-text, #D99B66)",
          }
        : {
            backgroundColor: "var(--bg-elevated)",
            borderColor: "var(--border-subtle)",
            color: isSystem ? "var(--text-secondary)" : "var(--text-primary)",
          };
  const segments = React.useMemo(() => splitTurnContent(cleanedContent), [cleanedContent]);
  const hasThinking = Boolean(turn.thinking && turn.thinking.trim().length > 0);
  const toolCalls = turn.tool_calls ?? [];

  const toolOutputSummary = React.useMemo(() => {
    if (!isToolOutput) return "";
    const raw = cleanedContent || turn.content || "";
    const lines = raw.split("\n");
    const exitMatch = raw.match(/Command exited with code\s+\d+/i);
    const suffix = exitMatch ? ` · ${exitMatch[0]}` : "";
    return `Tool / Terminal Output (${lines.length} ${lines.length === 1 ? "line" : "lines"})${suffix}`;
  }, [isToolOutput, cleanedContent, turn.content]);

  const wireSummary = React.useMemo(() => {
    if (!isWire) return "";
    return summarizeWireContent(cleanedContent || turn.content || "", wireSender);
  }, [isWire, cleanedContent, turn.content, wireSender]);

  const systemOutputSummary = React.useMemo(() => {
    if (!isSystem) return "";
    const raw = cleanedContent || turn.content || "";
    const lines = raw.split("\n");
    return `System (${lines.length} ${lines.length === 1 ? "line" : "lines"})`;
  }, [isSystem, cleanedContent, turn.content]);

  return (
    <article
      className="fd-card"
      data-testid={`turn-card-${turn.index}`}
      data-directed-assistant={isDirectedAssistant ? "true" : undefined}
      data-wire-card={isWire ? "true" : undefined}
      style={{
        borderColor: roleBorderColor,
        backgroundColor: isUser
          ? "var(--card-user-bg)"
          : isDirectedAssistant
            ? "var(--card-assistant-bg)"
            : isWire
              ? "var(--wire-card-bg, rgba(180, 120, 70, 0.06))"
              : "var(--bg-surface)",
        boxShadow: isUser
          ? "inset 3px 0 0 var(--accent-primary)"
          : isDirectedAssistant
            ? "inset 3px 0 0 var(--status-info)"
            : isWire
              ? "inset 3px 0 0 0 var(--wire-card-stripe, rgba(180, 120, 70, 0.72))"
              : undefined,
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

      {hasThinking && turn.thinking && (
        <ThinkingBlock thinking={turn.thinking} isActive={isActive} />
      )}

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

      {isWire && segments.length > 0 && (
        <div
          style={{
            borderRadius: "var(--radius-sm)",
            border: "1px solid var(--border-subtle)",
            backgroundColor: "rgba(13, 17, 24, 0.65)",
            overflow: "hidden",
          }}
        >
          <button
            type="button"
            data-testid="wire-output-toggle"
            aria-expanded={wireExpanded}
            onClick={() => setWireExpanded((prev) => !prev)}
            style={{
              width: "100%",
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "8px",
              padding: "6px 10px",
              background: "transparent",
              border: "none",
              color: "var(--text-secondary)",
              cursor: "pointer",
              fontSize: "11px",
              textAlign: "left",
            }}
          >
            <span className="fd-mono" style={{ display: "flex", alignItems: "center", gap: "6px", minWidth: 0 }}>
              <span style={{ color: "var(--accent-primary)" }}>
                {wireExpanded ? "▼" : "▶"}
              </span>
              <span
                style={{
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  whiteSpace: "nowrap",
                }}
              >
                {wireSummary}
              </span>
            </span>
            <span style={{ color: "var(--text-dim)", fontSize: "11px", flexShrink: 0 }}>
              {wireExpanded ? "Hide message" : "Show message"}
            </span>
          </button>
        </div>
      )}

      {isSystem && (cleanedContent || turn.content) && (
        <div
          style={{
            borderRadius: "var(--radius-sm)",
            border: "1px solid var(--border-subtle)",
            backgroundColor: "rgba(13, 17, 24, 0.65)",
            overflow: "hidden",
          }}
        >
          <button
            type="button"
            data-testid={`turn-system-output-toggle-${turn.index}`}
            aria-expanded={systemExpanded}
            onClick={() => setSystemExpanded((prev) => !prev)}
            style={{
              width: "100%",
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "8px",
              padding: "6px 10px",
              background: "transparent",
              border: "none",
              color: "var(--text-secondary)",
              cursor: "pointer",
              fontSize: "11px",
              textAlign: "left",
            }}
          >
            <span className="fd-mono" style={{ display: "flex", alignItems: "center", gap: "6px" }}>
              <span style={{ color: "var(--accent-primary)" }}>
                {systemExpanded ? "▼" : "▶"}
              </span>
              <span>{systemOutputSummary}</span>
            </span>
            <span style={{ color: "var(--text-dim)", fontSize: "11px" }}>
              {systemExpanded ? "Hide output" : "Show output"}
            </span>
          </button>
        </div>
      )}

      {(!isWire || wireExpanded) && (!isSystem || systemExpanded) && (
        <>
          {isToolOutput && (cleanedContent || turn.content) ? (
        <div
          data-testid={`turn-content-${turn.index}`}
          style={{
            borderRadius: "var(--radius-sm)",
            border: "1px solid var(--border-subtle)",
            backgroundColor: "rgba(13, 17, 24, 0.65)",
            overflow: "hidden",
          }}
        >
          <button
            type="button"
            data-testid={`turn-tool-output-toggle-${turn.index}`}
            aria-expanded={toolOutputExpanded}
            onClick={() => setToolOutputExpanded((prev) => !prev)}
            style={{
              width: "100%",
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "8px",
              padding: "6px 10px",
              background: "transparent",
              border: "none",
              color: "var(--text-secondary)",
              cursor: "pointer",
              fontSize: "11px",
              textAlign: "left",
            }}
          >
            <span className="fd-mono" style={{ display: "flex", alignItems: "center", gap: "6px" }}>
              <span style={{ color: "var(--accent-primary)" }}>
                {toolOutputExpanded ? "▼" : "▶"}
              </span>
              <span>{toolOutputSummary}</span>
            </span>
            <span style={{ color: "var(--text-dim)", fontSize: "11px" }}>
              {toolOutputExpanded ? "Hide output" : "Show output"}
            </span>
          </button>

          {toolOutputExpanded && (
            <pre
              className="fd-mono"
              style={{
                margin: 0,
                padding: "10px 12px",
                borderTop: "1px solid var(--border-subtle)",
                fontSize: "11.5px",
                lineHeight: 1.45,
                color: "var(--text-secondary)",
                whiteSpace: "pre-wrap",
                wordBreak: "break-word",
                maxHeight: "340px",
                overflowY: "auto",
              }}
            >
              {cleanedContent || turn.content}
            </pre>
          )}
        </div>
      ) : (
        segments.length > 0 && (
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
            {segments.map((seg, idx) => {
              if (seg.kind === "code") {
                if ((seg.language || "").toLowerCase() === "mermaid") {
                  return (
                    <MermaidDiagram
                      key={idx}
                      code={seg.text}
                      onOpenArtifact={onOpenArtifact}
                    />
                  );
                }
                const codeLines = seg.text.split("\n");
                const isLargeCode = codeLines.length > 10;
                const isExpanded = isLargeCode ? Boolean(expandedCodeBlocks[idx]) : true;

                return (
                  <div
                    key={idx}
                    style={{
                      borderRadius: "var(--radius-sm)",
                      border: "1px solid var(--border-subtle)",
                      backgroundColor: "var(--bg-input)",
                      overflow: "hidden",
                    }}
                  >
                    {isLargeCode ? (
                      <button
                        type="button"
                        data-testid={`code-block-toggle-${turn.index}-${idx}`}
                        aria-expanded={isExpanded}
                        onClick={() =>
                          setExpandedCodeBlocks((prev) => ({
                            ...prev,
                            [idx]: !prev[idx],
                          }))
                        }
                        className="fd-mono"
                        style={{
                          width: "100%",
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "space-between",
                          padding: "5px 10px",
                          fontSize: "10.5px",
                          color: "var(--text-muted)",
                          border: "none",
                          borderBottom: isExpanded ? "1px solid var(--border-subtle)" : "none",
                          backgroundColor: "rgba(23, 30, 41, 0.7)",
                          cursor: "pointer",
                          textAlign: "left",
                        }}
                      >
                        <span>
                          {isExpanded ? "▼" : "▶"} {seg.language || "output"} ({codeLines.length}{" "}
                          lines)
                        </span>
                        <span>{isExpanded ? "Collapse" : "Expand"}</span>
                      </button>
                    ) : (
                      seg.language && (
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
                      )
                    )}
                    {isExpanded && (
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
                    )}
                  </div>
                );
              }

              return (
                <div
                  key={idx}
                  style={{
                    wordBreak: "break-word",
                  }}
                >
                  <RichMarkdown text={seg.text.trim()} onOpenArtifact={onOpenArtifact} />
                </div>
              );
            })}
          </div>
          )
          )}
        </>
      )}

      {attachments.length > 0 && (
        <div
          data-testid={`turn-attachments-${turn.index}`}
          style={{
            display: "flex",
            flexWrap: "wrap",
            gap: "8px",
            marginTop: "4px",
          }}
        >
          {attachments.map((att, idx) => (
            <div
              key={`${att.path}-${idx}`}
              data-testid={`turn-attachment-item-${turn.index}-${idx}`}
              onClick={() => dispatchOpenArtifact(att.path || att.url || "", onOpenArtifact)}
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "6px",
                padding: "6px 8px",
                borderRadius: "var(--radius-sm)",
                border: "1px solid var(--border-subtle)",
                backgroundColor: "var(--bg-elevated)",
                maxWidth: "280px",
                cursor: "pointer",
              }}
            >
              <div style={{ display: "flex", alignItems: "center", gap: "6px", fontSize: "11px" }}>
                <span className="fd-mono" style={{ color: "var(--text-secondary)" }}>
                  {att.name}
                </span>
                {att.url && (
                  <a
                    href={att.url}
                    target="_blank"
                    rel="noreferrer"
                    className="fd-mono"
                    onClick={(e) => {
                      e.stopPropagation();
                      e.preventDefault();
                      dispatchOpenArtifact(att.path || att.url || "", onOpenArtifact);
                    }}
                    style={{ color: "var(--accent-hover)", fontSize: "11px" }}
                  >
                    open
                  </a>
                )}
              </div>
              {att.isImage && (
                <img
                  src={att.url || att.path}
                  alt={att.name}
                  style={{
                    maxWidth: "260px",
                    maxHeight: "180px",
                    borderRadius: "var(--radius-sm)",
                    objectFit: "contain",
                    border: "1px solid var(--border-subtle)",
                  }}
                />
              )}
            </div>
          ))}
        </div>
      )}
    </article>
  );
};
