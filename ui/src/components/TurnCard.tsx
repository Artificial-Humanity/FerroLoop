import { formatTimestamp, useActiveTimeZone } from "../lib/datetime";
import { isSafeInlineImageUrl, isSafeLinkUrl } from "../lib/safeUrl";
import React from "react";
import type { TranscriptTurn } from "../types/api";
import { dispatchOpenArtifact, MermaidDiagram, RichMarkdown } from "./RichMarkdown";
import { ThinkingBlock } from "./ThinkingBlock";
import { ToolCallBlock } from "./ToolCallBlock";

export const EXPAND_TURN_EVENT = "ferrodeck:expand-turn";

export interface TurnCardProps {
  turn: TranscriptTurn;
  agentDisplayName?: string;
  /** True for the latest turn while the session is busy (live thinking). */
  isActive?: boolean;
  onOpenArtifact?: (pathOrUrl: string) => void;
  /** Consecutive tool-output turns attached to this assistant tool-call turn. */
  attachedToolOutputs?: TranscriptTurn[];
  /**
   * When true (default), internal work (Thinking + ToolCalls + attachedToolOutputs)
   * on a directed-assistant prose turn is collapsed into a single "Internal work" row.
   */
  encapsulateInternalWork?: boolean;
}

export function summarizeToolOutputContent(raw: string): string {
  const lines = raw.split("\n");
  const exitMatch = raw.match(/Command exited with code\s+\d+/i);
  const suffix = exitMatch ? ` · ${exitMatch[0]}` : "";
  return `Tool / Terminal Output (${lines.length} ${lines.length === 1 ? "line" : "lines"})${suffix}`;
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

export function isSubagentHandbackContent(content: string): boolean {
  if (!content) {
    return false;
  }
  const withoutClaudeForward = content
    .trimStart()
    .replace(/^Another Claude session sent a message:\s*/i, "")
    .trimStart();
  return (
    /^<agent-message\b/i.test(withoutClaudeForward) ||
    /^\[Subagent\s/i.test(withoutClaudeForward) ||
    withoutClaudeForward.includes("[Subagent hand-back]")
  );
}

/**
 * Strip Claude / subagent hand-back wrapper tags and boilerplate header so expanded
 * `Wire · subagent` turns cleanly render the subagent's Markdown report.
 */
export function stripSubagentHandbackWrapper(content: string): string {
  if (!content) {
    return "";
  }
  return content
    .replace(/^\s*Another Claude session sent a message:\s*/i, "")
    .replace(/^\s*<agent-message[^>]*>\s*/i, "")
    .replace(
      /\s*<\/agent-message>(?:\s*This came from another Claude session[\s\S]*)?\s*$/i,
      "",
    )
    .replace(
      /^\s*(?:\[Subagent hand-back\][\s\S]*?The report follows:\s*|\[Subagent [^\]]+\]\s*)/i,
      "",
    )
    .trim();
}

function hasSystemNoticePrefix(text: string): boolean {
  return (
    /^\[Notice\]\s/i.test(text) ||
    /^<EPHEMERAL_MESSAGE>/i.test(text) ||
    /^<task-notification[\s>]/i.test(text) ||
    /^<local-command-caveat[\s>]/i.test(text) ||
    /^<local-command-stdout[\s>]/i.test(text) ||
    /^\[SYSTEM NOTIFICATION - NOT USER INPUT\]/i.test(text) ||
    (/^<SYSTEM_MESSAGE>/i.test(text) &&
      !text.includes("[Operator Prompt via FerroDeck"))
  );
}

export function extractTurnAttachments(
  content: string,
  options?: { stripAgyWrapper?: boolean; stripSubagentWrapper?: boolean },
): {
  cleanedContent: string;
  attachments: ParsedTurnAttachment[];
} {
  if (!content) {
    return { cleanedContent: "", attachments: [] };
  }
  const shouldStrip = options?.stripAgyWrapper ?? true;
  const agyStripped = shouldStrip ? stripAgyUserRequestWrapper(content) : content;
  const stripped = options?.stripSubagentWrapper
    ? stripSubagentHandbackWrapper(agyStripped)
    : agyStripped;
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

export function isPureToolOutputTurn(turn: TranscriptTurn): boolean {
  const roleNormalized = (turn.role || "assistant").toLowerCase();
  if (roleNormalized === "tool") {
    return true;
  }
  const trimmedStart = (turn.content || "").trimStart();
  if (/^Task id "[^"]+" finished with result:/i.test(trimmedStart)) {
    return true;
  }
  const shouldStripAgyWrapper =
    roleNormalized === "user" || roleNormalized === "system";
  const strippedForNotice = shouldStripAgyWrapper
    ? stripAgyUserRequestWrapper(turn.content || "").trimStart()
    : trimmedStart;
  const looksLikeSystemCodeDump = /^"\)\r?\n\d+:\s/.test(trimmedStart);
  const looksLikeSystemNotice =
    hasSystemNoticePrefix(trimmedStart) || hasSystemNoticePrefix(strippedForNotice);
  const looksLikeSubagentHandback =
    (roleNormalized === "user" || roleNormalized === "wire:subagent") &&
    (isSubagentHandbackContent(trimmedStart) ||
      isSubagentHandbackContent(strippedForNotice));
  const isUser =
    roleNormalized === "user" &&
    !looksLikeSystemCodeDump &&
    !looksLikeSystemNotice &&
    !looksLikeSubagentHandback;
  const isWire = roleNormalized.startsWith("wire:") || looksLikeSubagentHandback;
  const isSystem =
    roleNormalized === "system" ||
    (roleNormalized === "user" &&
      !looksLikeSubagentHandback &&
      (looksLikeSystemCodeDump || looksLikeSystemNotice));
  return (
    !isUser &&
    !isSystem &&
    !isWire &&
    trimmedStart.startsWith("Created At:") &&
    (!turn.tool_calls || turn.tool_calls.length === 0)
  );
}

export function isOperatorUserTurn(turn: TranscriptTurn): boolean {
  const roleNormalized = (turn.role || "assistant").toLowerCase();
  if (roleNormalized !== "user") {
    return false;
  }
  const trimmedStart = (turn.content || "").trimStart();
  if (/^Task id "[^"]+" finished with result:/i.test(trimmedStart)) {
    return false;
  }
  const strippedForNotice = stripAgyUserRequestWrapper(turn.content || "").trimStart();
  const looksLikeSystemCodeDump = /^"\)\r?\n\d+:\s/.test(trimmedStart);
  const looksLikeSystemNotice =
    hasSystemNoticePrefix(trimmedStart) || hasSystemNoticePrefix(strippedForNotice);
  const looksLikeSubagentHandback =
    isSubagentHandbackContent(trimmedStart) || isSubagentHandbackContent(strippedForNotice);
  return (
    !looksLikeSystemCodeDump &&
    !looksLikeSystemNotice &&
    !looksLikeSubagentHandback
  );
}

export function isWireTurn(turn: TranscriptTurn): boolean {
  const roleNormalized = (turn.role || "assistant").toLowerCase();
  if (roleNormalized.startsWith("wire:")) {
    return true;
  }
  if (roleNormalized === "user") {
    const trimmedStart = (turn.content || "").trimStart();
    const strippedForNotice = stripAgyUserRequestWrapper(turn.content || "").trimStart();
    return (
      isSubagentHandbackContent(trimmedStart) ||
      isSubagentHandbackContent(strippedForNotice)
    );
  }
  return false;
}

export const TurnCard: React.FC<TurnCardProps> = ({
  turn,
  agentDisplayName,
  isActive = false,
  onOpenArtifact,
  attachedToolOutputs,
  encapsulateInternalWork = true,
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
  // Defensive: Antigravity [Notice], <EPHEMERAL_MESSAGE>, non-operator <SYSTEM_MESSAGE>,
  // and Claude internal notification/command tags arrive with role === "user" but represent system notices.
  const looksLikeSystemNotice =
    hasSystemNoticePrefix(trimmedStart) || hasSystemNoticePrefix(strippedForNotice);
  // Defensive: Subagent hand-back messages can arrive as role === "user" when forwarded
  // across sessions; always render them as collapsed `Wire · subagent` cards.
  const looksLikeSubagentHandback =
    (roleNormalized === "user" || roleNormalized === "wire:subagent") &&
    (isSubagentHandbackContent(trimmedStart) || isSubagentHandbackContent(strippedForNotice));
  const isUser =
    roleNormalized === "user" &&
    !looksLikeBackgroundTaskOutput &&
    !looksLikeSystemCodeDump &&
    !looksLikeSystemNotice &&
    !looksLikeSubagentHandback;
  const isWire = roleNormalized.startsWith("wire:") || looksLikeSubagentHandback;
  const wireSender = roleNormalized.startsWith("wire:")
    ? turn.role.slice(5)
    : looksLikeSubagentHandback
      ? "subagent"
      : null;
  const isSystem =
    roleNormalized === "system" ||
    (roleNormalized === "user" &&
      !looksLikeSubagentHandback &&
      (looksLikeSystemCodeDump || looksLikeSystemNotice));
  const isToolOutput =
    roleNormalized === "tool" ||
    looksLikeBackgroundTaskOutput ||
    (!isUser &&
      !isSystem &&
      !isWire &&
      trimmedStart.startsWith("Created At:") &&
      (!turn.tool_calls || turn.tool_calls.length === 0));

  const [toolOutputExpanded, setToolOutputExpanded] = React.useState(false);
  const [expandedAttachedToolOutputs, setExpandedAttachedToolOutputs] = React.useState<
    Record<number, boolean>
  >({});
  const [internalWorkExpanded, setInternalWorkExpanded] = React.useState(false);
  const [wireExpanded, setWireExpanded] = React.useState(false);
  const activeTimeZone = useActiveTimeZone();
  const [systemExpanded, setSystemExpanded] = React.useState(false);
  const [expandedCodeBlocks, setExpandedCodeBlocks] = React.useState<Record<number, boolean>>({});

  React.useEffect(() => {
    if (typeof window === "undefined") return;
    const handler = (event: Event) => {
      const custom = event as CustomEvent<{ turnIndex?: number }>;
      const targetIdx = custom.detail?.turnIndex;
      if (targetIdx === undefined) return;
      if (
        targetIdx === turn.index ||
        (attachedToolOutputs ?? []).some((o) => o.index === targetIdx)
      ) {
        setInternalWorkExpanded(true);
      }
    };
    window.addEventListener(EXPAND_TURN_EVENT, handler);
    return () => window.removeEventListener(EXPAND_TURN_EVENT, handler);
  }, [turn.index, attachedToolOutputs]);

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
        stripSubagentWrapper: looksLikeSubagentHandback,
      }),
    [turn.content, shouldStripAgyWrapper, looksLikeSubagentHandback],
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
  const attachedOutputsCount = attachedToolOutputs?.length ?? 0;
  const hasInternalWork =
    hasThinking || toolCalls.length > 0 || attachedOutputsCount > 0;
  const shouldEncapsulateInternalWork =
    encapsulateInternalWork && isDirectedAssistant && hasInternalWork;

  const internalWorkSummary = React.useMemo(() => {
    const parts: string[] = [];
    if (hasThinking) {
      parts.push("1 thinking");
    }
    if (toolCalls.length > 0) {
      parts.push(
        `${toolCalls.length} ${toolCalls.length === 1 ? "tool call" : "tool calls"}`,
      );
    }
    if (attachedOutputsCount > 0) {
      parts.push(
        `${attachedOutputsCount} ${attachedOutputsCount === 1 ? "output" : "outputs"}`,
      );
    }
    return `Internal work · ${parts.join(" · ")}`;
  }, [hasThinking, toolCalls.length, attachedOutputsCount]);

  const toolOutputSummary = React.useMemo(() => {
    if (!isToolOutput) return "";
    const raw = cleanedContent || turn.content || "";
    return summarizeToolOutputContent(raw);
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
            title={turn.created_at}
            className="fd-mono"
            style={{ fontSize: "11px", color: "var(--text-dim)" }}
          >
            {formatTimestamp(turn.created_at, activeTimeZone)}
          </time>
        )}
      </header>

      {shouldEncapsulateInternalWork ? (
        <div
          data-testid={`turn-internal-work-${turn.index}`}
          style={{
            borderRadius: "var(--radius-sm)",
            border: "1px solid var(--border-subtle)",
            backgroundColor: "var(--subcard-bg, var(--bg-elevated))",
            overflow: "hidden",
          }}
        >
          <button
            type="button"
            data-testid={`turn-internal-work-toggle-${turn.index}`}
            aria-expanded={internalWorkExpanded}
            onClick={() => setInternalWorkExpanded((prev) => !prev)}
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
            <span
              className="fd-mono"
              style={{ display: "flex", alignItems: "center", gap: "6px", minWidth: 0 }}
            >
              <span style={{ color: "var(--accent-primary)" }}>
                {internalWorkExpanded ? "▾" : "▸"}
              </span>
              {isActive && hasThinking && !internalWorkExpanded && (
                <span
                  className="fd-thinking-dot"
                  data-testid="thinking-activity-dot"
                  aria-hidden="true"
                />
              )}
              <span
                style={{
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  whiteSpace: "nowrap",
                }}
              >
                {internalWorkSummary}
              </span>
            </span>
            <span style={{ color: "var(--text-dim)", fontSize: "11px", flexShrink: 0 }}>
              {internalWorkExpanded ? "Hide steps" : "Show steps"}
            </span>
          </button>

          {internalWorkExpanded && (
            <div
              data-testid={`turn-internal-work-body-${turn.index}`}
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "6px",
                padding: "8px 10px",
                borderTop: "1px solid var(--border-subtle)",
              }}
            >
              {hasThinking && turn.thinking && (
                <ThinkingBlock thinking={turn.thinking} isActive={isActive} />
              )}

              {(toolCalls.length > 0 ||
                (attachedToolOutputs && attachedToolOutputs.length > 0)) && (
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

                  {attachedToolOutputs && attachedToolOutputs.length > 0 && (
                    <div
                      data-testid={`turn-attached-tool-outputs-${turn.index}`}
                      style={{ display: "flex", flexDirection: "column", gap: "4px" }}
                    >
                      {attachedToolOutputs.map((outTurn) => {
                        const { cleanedContent: outCleaned } = extractTurnAttachments(
                          outTurn.content,
                          { stripAgyWrapper: false },
                        );
                        const outText = outCleaned || outTurn.content || "";
                        const outSummary = summarizeToolOutputContent(outText);
                        const outExpanded = Boolean(expandedAttachedToolOutputs[outTurn.index]);
                        return (
                          <div
                            key={`attached-tool-output-${outTurn.index}`}
                            data-testid={`turn-content-${outTurn.index}`}
                            style={{
                              borderRadius: "var(--radius-sm)",
                              border: "1px solid var(--border-subtle)",
                              backgroundColor: "var(--subcard-bg, var(--bg-elevated))",
                              overflow: "hidden",
                            }}
                          >
                            <button
                              type="button"
                              data-testid={`turn-tool-output-toggle-${outTurn.index}`}
                              aria-expanded={outExpanded}
                              onClick={() =>
                                setExpandedAttachedToolOutputs((prev) => ({
                                  ...prev,
                                  [outTurn.index]: !prev[outTurn.index],
                                }))
                              }
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
                              <span
                                className="fd-mono"
                                style={{ display: "flex", alignItems: "center", gap: "6px" }}
                              >
                                <span
                                  data-testid={`turn-tool-output-arrow-${outTurn.index}`}
                                  style={{ color: "var(--status-warn, #F59E0B)" }}
                                >
                                  {outExpanded ? "▼" : "▶"}
                                </span>
                                <span>{outSummary}</span>
                              </span>
                              <span style={{ color: "var(--text-dim)", fontSize: "11px" }}>
                                {outExpanded ? "Hide output" : "Show output"}
                              </span>
                            </button>

                            {outExpanded && (
                              <pre
                                className="fd-mono"
                                style={{
                                  margin: 0,
                                  padding: "10px 12px",
                                  borderTop: "1px solid var(--border-subtle)",
                                  backgroundColor: "var(--bg-input)",
                                  fontSize: "11.5px",
                                  lineHeight: 1.45,
                                  color: "var(--text-primary)",
                                  whiteSpace: "pre-wrap",
                                  wordBreak: "break-word",
                                  maxHeight: "340px",
                                  overflowY: "auto",
                                }}
                              >
                                {outText}
                              </pre>
                            )}
                          </div>
                        );
                      })}
                    </div>
                  )}
                </div>
              )}
            </div>
          )}
        </div>
      ) : (
        <>
          {hasThinking && turn.thinking && (
            <ThinkingBlock thinking={turn.thinking} isActive={isActive} />
          )}

          {(toolCalls.length > 0 ||
            (attachedToolOutputs && attachedToolOutputs.length > 0)) && (
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

              {attachedToolOutputs && attachedToolOutputs.length > 0 && (
                <div
                  data-testid={`turn-attached-tool-outputs-${turn.index}`}
                  style={{ display: "flex", flexDirection: "column", gap: "4px" }}
                >
                  {attachedToolOutputs.map((outTurn) => {
                    const { cleanedContent: outCleaned } = extractTurnAttachments(
                      outTurn.content,
                      { stripAgyWrapper: false },
                    );
                    const outText = outCleaned || outTurn.content || "";
                    const outSummary = summarizeToolOutputContent(outText);
                    const outExpanded = Boolean(expandedAttachedToolOutputs[outTurn.index]);
                    return (
                      <div
                        key={`attached-tool-output-${outTurn.index}`}
                        data-testid={`turn-content-${outTurn.index}`}
                        style={{
                          borderRadius: "var(--radius-sm)",
                          border: "1px solid var(--border-subtle)",
                          backgroundColor: "var(--subcard-bg, var(--bg-elevated))",
                          overflow: "hidden",
                        }}
                      >
                        <button
                          type="button"
                          data-testid={`turn-tool-output-toggle-${outTurn.index}`}
                          aria-expanded={outExpanded}
                          onClick={() =>
                            setExpandedAttachedToolOutputs((prev) => ({
                              ...prev,
                              [outTurn.index]: !prev[outTurn.index],
                            }))
                          }
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
                          <span
                            className="fd-mono"
                            style={{ display: "flex", alignItems: "center", gap: "6px" }}
                          >
                            <span
                              data-testid={`turn-tool-output-arrow-${outTurn.index}`}
                              style={{ color: "var(--status-warn, #F59E0B)" }}
                            >
                              {outExpanded ? "▼" : "▶"}
                            </span>
                            <span>{outSummary}</span>
                          </span>
                          <span style={{ color: "var(--text-dim)", fontSize: "11px" }}>
                            {outExpanded ? "Hide output" : "Show output"}
                          </span>
                        </button>

                        {outExpanded && (
                          <pre
                            className="fd-mono"
                            style={{
                              margin: 0,
                              padding: "10px 12px",
                              borderTop: "1px solid var(--border-subtle)",
                              backgroundColor: "var(--bg-input)",
                              fontSize: "11.5px",
                              lineHeight: 1.45,
                              color: "var(--text-primary)",
                              whiteSpace: "pre-wrap",
                              wordBreak: "break-word",
                              maxHeight: "340px",
                              overflowY: "auto",
                            }}
                          >
                            {outText}
                          </pre>
                        )}
                      </div>
                    );
                  })}
                </div>
              )}
            </div>
          )}
        </>
      )}

      {isWire && segments.length > 0 && (
        <div
          style={{
            borderRadius: "var(--radius-sm)",
            border: "1px solid var(--border-subtle)",
            backgroundColor: "var(--subcard-bg, var(--bg-elevated))",
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
            backgroundColor: "var(--subcard-bg, var(--bg-elevated))",
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
            backgroundColor: "var(--subcard-bg, var(--bg-elevated))",
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
              <span
                data-testid={`turn-tool-output-arrow-${turn.index}`}
                style={{ color: "var(--status-warn, #F59E0B)" }}
              >
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
                backgroundColor: "var(--bg-input)",
                fontSize: "11.5px",
                lineHeight: 1.45,
                color: "var(--text-primary)",
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
                          backgroundColor: "var(--bg-elevated)",
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
                            backgroundColor: "var(--bg-elevated)",
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
                {att.url && isSafeLinkUrl(att.url) && (
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
              {att.isImage && isSafeInlineImageUrl(att.url || att.path) && (
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
