import React from "react";
import type { HubApiClient } from "../api/client";
import {
  DEFAULT_SLASH_COMMANDS,
  type ClearSessionMode,
  type DecommissionMode,
  type DecommissionSessionResponse,
  type InterruptSessionResponse,
  type PromptAttachment,
  type PromptRequest,
  type PromptResponse,
  type QueuedOperatorPrompt,
  type QuotaWindowUsage,
  type SessionContextUsage,
  type SessionQuotaUsage,
  type SetSessionModelResponse,
  type SlashCommandEntry,
  type TranscriptTurn,
} from "../types/api";
import { formatTimestamp, useActiveTimeZone } from "../lib/datetime";
import { AgentSettingsModal } from "./AgentSettingsModal";
import { dispatchOpenHelp, type HelpTopicId } from "./HelpDrawer";
import { ThinkingBlock } from "./ThinkingBlock";
import {
  EXPAND_TURN_EVENT,
  extractTurnAttachments,
  isOperatorUserTurn,
  isPureToolOutputTurn,
  isWireTurn,
  summarizeWireContent,
  TurnCard,
} from "./TurnCard";

export interface SinceLastPromptStep {
  id: string;
  turnIndex: number;
  kind: "reply" | "tool" | "file" | "wire";
  label: string;
}

export interface SinceLastPromptSummary {
  promptTurnIndex: number;
  promptCreatedAt?: string | null;
  promptText: string;
  assistantRepliesCount: number;
  toolCallsCount: number;
  toolSummaries: Array<{ name: string; count: number; turnIndex: number }>;
  wireMessagesCount: number;
  touchedFiles: Array<{ path: string; basename: string; turnIndex: number }>;
  steps: SinceLastPromptStep[];
}

const FILE_MODIFYING_TOOL_REGEX =
  /^(?:edit_file|write_file|replace_in_file|create_file|multi_edit|patch|notebook_edit)$/i;

export function computeSinceLastPromptSummary(
  turns: TranscriptTurn[],
): SinceLastPromptSummary | null {
  let lastUserPos = -1;
  for (let i = turns.length - 1; i >= 0; i--) {
    if (isOperatorUserTurn(turns[i])) {
      lastUserPos = i;
      break;
    }
  }
  if (lastUserPos < 0) {
    return null;
  }

  const lastUserTurn = turns[lastUserPos];
  const { cleanedContent } = extractTurnAttachments(lastUserTurn.content || "", {
    stripAgyWrapper: true,
  });
  const promptText =
    (cleanedContent || lastUserTurn.content || "").trim() || "(empty prompt)";

  let assistantRepliesCount = 0;
  let toolCallsCount = 0;
  let wireMessagesCount = 0;
  const toolMap = new Map<string, { count: number; turnIndex: number }>();
  const fileMap = new Map<string, { path: string; basename: string; turnIndex: number }>();
  const steps: SinceLastPromptStep[] = [];

  const subsequentTurns = turns.slice(lastUserPos + 1);
  for (const t of subsequentTurns) {
    if (isWireTurn(t)) {
      wireMessagesCount++;
      const roleNormalized = (t.role || "").toLowerCase();
      const sender = roleNormalized.startsWith("wire:")
        ? t.role.slice(5)
        : "subagent";
      steps.push({
        id: `wire-${t.index}`,
        turnIndex: t.index,
        kind: "wire",
        label: summarizeWireContent(t.content || "", sender),
      });
      continue;
    }

    const roleNormalized = (t.role || "assistant").toLowerCase();
    if (roleNormalized === "assistant" && !isPureToolOutputTurn(t)) {
      const toolCalls = t.tool_calls ?? [];
      toolCalls.forEach((tc, idx) => {
        toolCallsCount++;
        const name = tc.name || "tool";
        const existingTool = toolMap.get(name);
        if (existingTool) {
          existingTool.count += 1;
          existingTool.turnIndex = t.index;
        } else {
          toolMap.set(name, { count: 1, turnIndex: t.index });
        }

        const args =
          tc.arguments && typeof tc.arguments === "object"
            ? (tc.arguments as Record<string, unknown>)
            : {};
        const rawPath =
          args.Path ?? args.path ?? args.file_path ?? args.TargetFile;
        if (
          typeof rawPath === "string" &&
          rawPath.trim().length > 0 &&
          FILE_MODIFYING_TOOL_REGEX.test(name)
        ) {
          const cleanPath = rawPath.trim();
          const basename = cleanPath.split("/").pop() || cleanPath;
          fileMap.set(cleanPath, {
            path: cleanPath,
            basename,
            turnIndex: t.index,
          });
          steps.push({
            id: `file-${t.index}-${idx}`,
            turnIndex: t.index,
            kind: "file",
            label: `${name}: ${basename}`,
          });
        } else {
          steps.push({
            id: `tool-${t.index}-${idx}`,
            turnIndex: t.index,
            kind: "tool",
            label: `Tool: ${name}`,
          });
        }
      });

      const { cleanedContent: replyClean } = extractTurnAttachments(
        t.content || "",
        { stripAgyWrapper: false },
      );
      const replyTrimmed = replyClean.trim();
      if (replyTrimmed.length > 0) {
        assistantRepliesCount++;
        const firstLine =
          replyTrimmed
            .split("\n")
            .find((line) => line.trim().length > 0)
            ?.trim() ?? replyTrimmed;
        const snippet =
          firstLine.length > 76 ? `${firstLine.slice(0, 76)}…` : firstLine;
        steps.push({
          id: `reply-${t.index}`,
          turnIndex: t.index,
          kind: "reply",
          label: `Reply (#${t.index}): ${snippet}`,
        });
      }
    }
  }

  return {
    promptTurnIndex: lastUserTurn.index,
    promptCreatedAt: lastUserTurn.created_at ?? null,
    promptText,
    assistantRepliesCount,
    toolCallsCount,
    toolSummaries: Array.from(toolMap.entries()).map(([name, info]) => ({
      name,
      count: info.count,
      turnIndex: info.turnIndex,
    })),
    wireMessagesCount,
    touchedFiles: Array.from(fileMap.values()),
    steps,
  };
}

export interface CoalescedTurnGroup {
  turn: TranscriptTurn;
  attachedToolOutputs: TranscriptTurn[];
}

export function coalesceTurnsWithToolOutputs(
  turns: TranscriptTurn[],
): CoalescedTurnGroup[] {
  const groups: CoalescedTurnGroup[] = [];
  for (const turn of turns) {
    if (isPureToolOutputTurn(turn) && groups.length > 0) {
      const prev = groups[groups.length - 1];
      const prevRole = (prev.turn.role || "assistant").toLowerCase();
      const prevHasToolCalls = Boolean(
        prev.turn.tool_calls && prev.turn.tool_calls.length > 0,
      );
      if (
        prevRole === "assistant" &&
        !isPureToolOutputTurn(prev.turn) &&
        (prevHasToolCalls || prev.attachedToolOutputs.length > 0)
      ) {
        prev.attachedToolOutputs.push(turn);
        continue;
      }
    }
    groups.push({ turn, attachedToolOutputs: [] });
  }
  return groups;
}

export function isIntermediateAssistantWorkGroup(
  group: CoalescedTurnGroup,
): boolean {
  const roleNormalized = (group.turn.role || "assistant").toLowerCase();
  if (roleNormalized !== "assistant") {
    return false;
  }
  if (isPureToolOutputTurn(group.turn) || isWireTurn(group.turn)) {
    return false;
  }
  const { cleanedContent, attachments } = extractTurnAttachments(
    group.turn.content || "",
    { stripAgyWrapper: false },
  );
  if (cleanedContent.trim().length > 0 || attachments.length > 0) {
    return false;
  }
  const hasThinking = Boolean(
    group.turn.thinking && group.turn.thinking.trim().length > 0,
  );
  const hasToolCalls = (group.turn.tool_calls?.length ?? 0) > 0;
  const hasAttachedOutputs = group.attachedToolOutputs.length > 0;
  return hasThinking || hasToolCalls || hasAttachedOutputs;
}

export type StreamRenderItem =
  | { kind: "single"; group: CoalescedTurnGroup }
  | { kind: "work-run"; groups: CoalescedTurnGroup[] };

export function groupConsecutiveWorkRuns(
  groups: CoalescedTurnGroup[],
): StreamRenderItem[] {
  const items: StreamRenderItem[] = [];
  for (const group of groups) {
    if (isIntermediateAssistantWorkGroup(group)) {
      const prev = items[items.length - 1];
      if (prev && prev.kind === "work-run") {
        prev.groups.push(group);
      } else {
        items.push({ kind: "work-run", groups: [group] });
      }
    } else {
      items.push({ kind: "single", group });
    }
  }
  return items;
}

export interface EncapsulatedWorkBlockProps {
  groups: CoalescedTurnGroup[];
  agentDisplayName?: string;
  isLastActive?: boolean;
  onOpenArtifact?: (pathOrUrl: string) => void;
}

export const EncapsulatedWorkBlock: React.FC<EncapsulatedWorkBlockProps> = ({
  groups,
  agentDisplayName,
  isLastActive = false,
  onOpenArtifact,
}) => {
  const [expanded, setExpanded] = React.useState(false);
  const activeTimeZone = useActiveTimeZone();

  const allTurnIndices = React.useMemo(() => {
    const indices: number[] = [];
    for (const g of groups) {
      indices.push(g.turn.index);
      for (const out of g.attachedToolOutputs) {
        indices.push(out.index);
      }
    }
    return indices;
  }, [groups]);

  React.useEffect(() => {
    if (typeof window === "undefined") return;
    const handler = (event: Event) => {
      const custom = event as CustomEvent<{ turnIndex?: number }>;
      const targetIdx = custom.detail?.turnIndex;
      if (targetIdx !== undefined && allTurnIndices.includes(targetIdx)) {
        setExpanded(true);
      }
    };
    window.addEventListener(EXPAND_TURN_EVENT, handler);
    return () => window.removeEventListener(EXPAND_TURN_EVENT, handler);
  }, [allTurnIndices]);

  const firstIndex = groups[0]?.turn.index ?? 0;
  const lastIndex = groups[groups.length - 1]?.turn.index ?? firstIndex;
  const rangeLabel =
    firstIndex === lastIndex ? `#${firstIndex}` : `#${firstIndex}–#${lastIndex}`;

  const latestCreatedAt = React.useMemo(() => {
    for (let i = groups.length - 1; i >= 0; i--) {
      const g = groups[i];
      for (let j = g.attachedToolOutputs.length - 1; j >= 0; j--) {
        if (g.attachedToolOutputs[j].created_at) {
          return g.attachedToolOutputs[j].created_at;
        }
      }
      if (g.turn.created_at) {
        return g.turn.created_at;
      }
    }
    return undefined;
  }, [groups]);

  const {
    stepCount,
    thinkingCount,
    totalToolCalls,
    uniqueToolNames,
    totalOutputs,
    summaryText,
  } = React.useMemo(() => {
    const stepCnt = groups.length;
    let thinkCnt = 0;
    let toolCnt = 0;
    let outCnt = 0;
    const names: string[] = [];
    for (const g of groups) {
      if (g.turn.thinking && g.turn.thinking.trim().length > 0) {
        thinkCnt += 1;
      }
      for (const tc of g.turn.tool_calls ?? []) {
        toolCnt += 1;
        const name = tc.name || "tool";
        if (!names.includes(name)) {
          names.push(name);
        }
      }
      outCnt += g.attachedToolOutputs.length;
    }

    const parts: string[] = [
      `${stepCnt} ${stepCnt === 1 ? "step" : "steps"}`,
    ];
    if (thinkCnt > 0 && toolCnt === 0 && outCnt === 0) {
      parts.push(`${thinkCnt} thinking`);
    }
    if (toolCnt > 0) {
      const shown = names.slice(0, 3).join(", ");
      const extra = names.length > 3 ? `, +${names.length - 3}` : "";
      parts.push(
        `${toolCnt} ${toolCnt === 1 ? "tool call" : "tool calls"}${
          shown ? ` (${shown}${extra})` : ""
        }`,
      );
    }
    if (outCnt > 0) {
      parts.push(`${outCnt} ${outCnt === 1 ? "output" : "outputs"}`);
    }
    return {
      stepCount: stepCnt,
      thinkingCount: thinkCnt,
      totalToolCalls: toolCnt,
      uniqueToolNames: names,
      totalOutputs: outCnt,
      summaryText: parts.join(" · "),
    };
  }, [groups]);

  const lastGroupHasThinking = Boolean(
    groups[groups.length - 1]?.turn.thinking?.trim(),
  );

  return (
    <article
      className="fd-card"
      data-testid="encapsulated-work-block"
      data-turn-indices={allTurnIndices.join(",")}
      style={{
        borderColor: "var(--border-subtle)",
        backgroundColor: "var(--bg-surface)",
        display: "flex",
        flexDirection: "column",
        gap: expanded ? "8px" : "0",
        padding: expanded ? undefined : "6px 12px",
      }}
    >
      <button
        type="button"
        data-testid="encapsulated-work-toggle"
        aria-expanded={expanded}
        onClick={() => setExpanded((prev) => !prev)}
        style={{
          width: "100%",
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: "8px",
          padding: "2px 0",
          background: "transparent",
          border: "none",
          color: "var(--text-secondary)",
          cursor: "pointer",
          fontSize: "11.5px",
          textAlign: "left",
          flexWrap: "wrap",
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "8px",
            minWidth: 0,
            flex: 1,
          }}
        >
          <span
            className="fd-mono"
            style={{ color: "var(--accent-primary)", flexShrink: 0 }}
          >
            {expanded ? "▾" : "▸"}
          </span>
          {isLastActive && lastGroupHasThinking && !expanded && (
            <span
              className="fd-thinking-dot"
              data-testid="thinking-activity-dot"
              aria-hidden="true"
            />
          )}
          <span
            className="fd-badge"
            style={{
              backgroundColor: "var(--bg-elevated)",
              borderColor: "var(--border-subtle)",
              color: "var(--text-primary)",
              flexShrink: 0,
            }}
          >
            {agentDisplayName || "Assistant"}
          </span>
          <span
            className="fd-mono"
            data-testid="encapsulated-work-range"
            style={{ fontSize: "11px", color: "var(--text-dim)", flexShrink: 0 }}
          >
            {rangeLabel}
          </span>
          <span
            className="fd-mono"
            data-testid="encapsulated-work-summary"
            style={{
              fontSize: "11px",
              color: "var(--text-secondary)",
              overflow: "hidden",
              textOverflow: "ellipsis",
              whiteSpace: "nowrap",
            }}
            title={
              uniqueToolNames.length > 0
                ? `${stepCount} steps · tools: ${uniqueToolNames.join(", ")}`
                : undefined
            }
          >
            {summaryText}
          </span>
        </div>

        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "6px",
            flexShrink: 0,
          }}
        >
          {thinkingCount > 0 && (
            <span
              className="fd-badge fd-mono"
              style={{ fontSize: "10px", padding: "1px 6px" }}
            >
              {thinkingCount} thinking
            </span>
          )}
          {totalToolCalls > 0 && (
            <span
              className="fd-badge fd-mono"
              style={{
                fontSize: "10px",
                padding: "1px 6px",
                color: "var(--status-warn, #F59E0B)",
              }}
            >
              {totalToolCalls} {totalToolCalls === 1 ? "tool" : "tools"}
            </span>
          )}
          {totalOutputs > 0 && (
            <span
              className="fd-badge fd-mono"
              style={{ fontSize: "10px", padding: "1px 6px" }}
            >
              {totalOutputs} {totalOutputs === 1 ? "output" : "outputs"}
            </span>
          )}
          {latestCreatedAt && (
            <time
              dateTime={latestCreatedAt}
              title={latestCreatedAt}
              className="fd-mono"
              style={{ fontSize: "11px", color: "var(--text-dim)" }}
            >
              {formatTimestamp(latestCreatedAt, activeTimeZone)}
            </time>
          )}
          <span style={{ color: "var(--text-dim)", fontSize: "11px" }}>
            {expanded ? "Hide steps" : "Show steps"}
          </span>
        </div>
      </button>

      {expanded && (
        <div
          data-testid="encapsulated-work-body"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "8px",
            paddingTop: "6px",
            borderTop: "1px solid var(--border-subtle)",
          }}
        >
          {groups.map((group, gIdx) => (
            <TurnCard
              key={`${group.turn.index}-${group.turn.created_at ?? ""}`}
              turn={group.turn}
              attachedToolOutputs={group.attachedToolOutputs}
              agentDisplayName={agentDisplayName}
              encapsulateInternalWork={false}
              isActive={
                isLastActive &&
                gIdx === groups.length - 1 &&
                group.attachedToolOutputs.length === 0
              }
              onOpenArtifact={onOpenArtifact}
            />
          ))}
        </div>
      )}
    </article>
  );
};

export function getVendorModelOptions(
  adapter?: string | null,
  currentModel?: string | null,
  availableModels?: string[] | null,
): string[] {
  const normalized = (adapter ?? "").trim().toLowerCase();
  let base: string[];
  if (Array.isArray(availableModels) && availableModels.length > 0) {
    base = [...availableModels];
  } else if (
    normalized === "antigravity" ||
    normalized === "agy" ||
    normalized === "argon" ||
    normalized === "gemini"
  ) {
    base = [
      "gemini-4-argon",
      "gemini-3.8-flash",
      "claude-opus-5-5",
      "claude-sonnet-5-5",
      "gpt-oss-120b",
    ];
  } else if (normalized === "claude" || normalized.startsWith("claude-")) {
    base = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-4-5"];
  } else if (normalized === "codex" || normalized.startsWith("codex-")) {
    base = [
      "gpt-6.1-terra",
      "gpt-6.1-sol",
      "gpt-6-astra",
      "gpt-6-sol",
      "gpt-6-luna",
    ];
  } else if (normalized === "grok" || normalized.startsWith("grok-")) {
    base = ["grok-4.7", "grok-4.7-build-fast", "grok-4.6", "grok-4.5"];
  } else if (normalized === "muse" || normalized.startsWith("muse-")) {
    base = ["muse-spark"];
  } else {
    base = [
      "gemini-4-argon",
      "gemini-3.8-flash",
      "claude-opus-5-5",
      "claude-sonnet-5-5",
      "gpt-6.1-terra",
      "grok-4.7",
      "muse-spark",
    ];
  }
  const trimmedCurrent = (currentModel ?? "").trim();
  if (trimmedCurrent && !base.includes(trimmedCurrent)) {
    return [trimmedCurrent, ...base];
  }
  return base;
}

/** Rough fixed cost of system prompt plus tool definitions per session. */
export const BASE_SYSTEM_OVERHEAD_TOKENS = 12_000;

export interface EstimatedContextUsage {
  usedTokens: number;
  maxTokens: number;
  percent: number;
}

function maxTokensForAdapter(adapter?: string | null): number {
  const normalized = (adapter ?? "").trim().toLowerCase();
  if (
    normalized === "antigravity" ||
    normalized === "agy" ||
    normalized === "argon" ||
    normalized === "gemini" ||
    normalized === "muse" ||
    normalized.startsWith("muse-") ||
    normalized === "claude" ||
    normalized.startsWith("claude-")
  ) {
    return 1_000_000;
  }
  if (
    normalized === "grok" ||
    normalized.startsWith("grok-") ||
    normalized === "codex" ||
    normalized.startsWith("codex-")
  ) {
    return 500_000;
  }
  return 200_000;
}

/**
 * Estimate active context usage from loaded transcript turns. The hub
 * returns a sliding transcript window, so this is an approximation:
 * server-reported usage, when present, always wins.
 */
export function estimateSessionContextUsage(
  turns: TranscriptTurn[],
  adapter?: string | null,
  serverUsage?: SessionContextUsage | null,
): EstimatedContextUsage {
  if (serverUsage && serverUsage.max_tokens > 0) {
    const usedTokens = Math.max(0, serverUsage.used_tokens);
    return {
      usedTokens,
      maxTokens: serverUsage.max_tokens,
      percent: Math.min(100, Math.round((usedTokens / serverUsage.max_tokens) * 100)),
    };
  }
  const maxTokens = maxTokensForAdapter(adapter);
  if (!turns || turns.length === 0) {
    return { usedTokens: 0, maxTokens, percent: 0 };
  }
  let boundaryIndex = -1;
  for (let i = turns.length - 1; i >= 0; i -= 1) {
    const content = turns[i].content || "";
    if (content.includes("<CONTEXT_SUMMARY>") || content.includes("[Compacted context]")) {
      boundaryIndex = i;
      break;
    }
  }
  const activeTurns = boundaryIndex >= 0 ? turns.slice(boundaryIndex) : turns;
  let usedTokens = BASE_SYSTEM_OVERHEAD_TOKENS;
  for (const turn of activeTurns) {
    const chars =
      (turn.content?.length ?? 0) +
      (turn.thinking?.length ?? 0) +
      JSON.stringify(turn.tool_calls ?? []).length;
    usedTokens += Math.ceil(chars / 4);
  }
  return {
    usedTokens,
    maxTokens,
    percent: Math.min(100, Math.round((usedTokens / maxTokens) * 100)),
  };
}

export function formatTokenCount(value: number): string {
  if (value >= 1_000_000) {
    return `${(value / 1_000_000).toFixed(1).replace(/\.0$/, "")}m`;
  }
  if (value >= 1_000) {
    return `${(value / 1_000).toFixed(1).replace(/\.0$/, "")}k`;
  }
  return String(value);
}

export function hasQuotaUsageData(quota?: SessionQuotaUsage | null): boolean {
  if (!quota) {
    return false;
  }
  return (
    Boolean(quota.tier && quota.tier.trim().length > 0) ||
    quota.primary_window != null ||
    quota.secondary_window != null ||
    Boolean(quota.exhausted_reason && quota.exhausted_reason.trim().length > 0)
  );
}

export function formatQuotaResetCountdown(
  resetsAtMs?: number | null,
  nowMs: number = Date.now(),
): string | null {
  if (resetsAtMs == null || !Number.isFinite(resetsAtMs)) {
    return null;
  }
  const diffMs = resetsAtMs - nowMs;
  if (diffMs <= 0) {
    return "renews soon";
  }
  const totalMinutes = Math.max(1, Math.round(diffMs / 60_000));
  if (totalMinutes >= 24 * 60) {
    const days = Math.floor(totalMinutes / (24 * 60));
    const hours = Math.floor((totalMinutes % (24 * 60)) / 60);
    return hours > 0 ? `renews in ${days}d ${hours}h` : `renews in ${days}d`;
  }
  if (totalMinutes >= 60) {
    const hours = Math.floor(totalMinutes / 60);
    const mins = totalMinutes % 60;
    return mins > 0 ? `renews in ${hours}h ${mins}m` : `renews in ${hours}h`;
  }
  return `renews in ${totalMinutes}m`;
}

export function formatQuotaWindowRemaining(window: QuotaWindowUsage): number {
  return Math.max(0, Math.min(100, Math.round(100 - window.used_percent)));
}

/**
 * Identity of the transcript tail. The hub returns a fixed-size sliding
 * window, so turn *count* stays flat while new turns arrive; the tail key
 * still changes on every new or streamed turn.
 */
export function computeLastTurnKey(turns: TranscriptTurn[]): string {
  if (!turns || turns.length === 0) {
    return "";
  }
  const last = turns[turns.length - 1];
  return `${last.index}:${last.role}:${last.content?.length ?? 0}:${last.thinking?.length ?? 0}`;
}

export function hasNewTurnActivity(
  prevCount: number,
  prevLastTurnKey: string,
  turns: TranscriptTurn[],
): boolean {
  const lastTurnKey = computeLastTurnKey(turns);
  return (
    turns.length > prevCount ||
    (lastTurnKey !== "" && lastTurnKey !== prevLastTurnKey)
  );
}

export const ALWAYS_SEND_NOW_STORAGE_KEY = "ferrodeck.always_send_now";

function readInitialAlwaysSendNow(): boolean {
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      return window.localStorage.getItem(ALWAYS_SEND_NOW_STORAGE_KEY) === "true";
    }
  } catch {
    // Ignore storage errors
  }
  return false;
}

export interface SessionScrollState {
  scrollTop: number;
  pinnedToBottom: boolean;
}

export const sessionScrollMemory = new Map<string, SessionScrollState>();

export interface SessionDraft {
  promptText: string;
  attachments: PromptAttachment[];
}

/** In-progress composer drafts survive tab switches and agent switches. */
export const sessionDraftMemory = new Map<string, SessionDraft>();

export const DRAFT_STORAGE_PREFIX = "ferrodeck.draft.";

function loadSessionDraft(targetKey: string): SessionDraft {
  const cached = targetKey ? sessionDraftMemory.get(targetKey) : undefined;
  if (cached) {
    return cached;
  }
  let storedText = "";
  try {
    if (targetKey && typeof window !== "undefined" && window.localStorage) {
      storedText = window.localStorage.getItem(DRAFT_STORAGE_PREFIX + targetKey) ?? "";
    }
  } catch {
    // Ignore storage errors.
  }
  return { promptText: storedText, attachments: [] };
}

function clearSessionDraft(targetKey: string): void {
  if (!targetKey) {
    return;
  }
  sessionDraftMemory.delete(targetKey);
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      window.localStorage.removeItem(DRAFT_STORAGE_PREFIX + targetKey);
    }
  } catch {
    // Ignore storage errors.
  }
}

export interface ChatPaneProps {
  agentName?: string | null;
  agentDisplayName?: string | null;
  agentRole?: string | null;
  agentProject?: string | null;
  adapter?: string | null;
  model?: string | null;
  thinkingLevel?: string | null;
  availableModels?: string[] | null;
  sessionId?: string | null;
  turns?: TranscriptTurn[];
  /** Server-measured context usage; estimates apply when absent. */
  contextUsage?: SessionContextUsage | null;
  /** Vendor account quota/allowance usage when reported by the adapter/hub. */
  quotaUsage?: SessionQuotaUsage | null;
  isBusy?: boolean;
  queue?: QueuedOperatorPrompt[];
  loading?: boolean;
  error?: string | null;
  wsConnected?: boolean;
  slashCommands?: SlashCommandEntry[];
  agentSettingsOpen?: boolean;
  onOpenAgentSettings?: () => void;
  onCloseAgentSettings?: () => void;
  splitView?: boolean;
  onToggleSplitView?: () => void;
  onOpenMobileRoster?: () => void;
  mobileSidebarOpen?: boolean;
  onRefresh?: () => void;
  onSendPrompt?: (
    targetSessionOrAgentId: string,
    payload: PromptRequest,
  ) => Promise<unknown> | void;
  onSendQueuedNow?: (promptId: string) => Promise<unknown> | void;
  onCancelQueued?: (promptId: string) => Promise<unknown> | void;
  onUpdateQueued?: (promptId: string, prompt: string) => Promise<unknown> | void;
  onDecommissionSession?: (
    mode: DecommissionMode,
  ) => Promise<DecommissionSessionResponse | unknown> | void;
  onInterruptSession?: () => Promise<InterruptSessionResponse | unknown> | void;
  onSetSessionModel?: (
    model: string,
    thinkingLevel?: string | null,
  ) => Promise<SetSessionModelResponse | unknown> | void;
  onOpenArtifact?: (pathOrUrl: string) => void;
  onOpenHelpTopic?: (topic: HelpTopicId) => void;
  apiClient?: HubApiClient;
}

function fileToPromptAttachment(file: File): Promise<PromptAttachment> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = typeof reader.result === "string" ? reader.result : "";
      const commaIdx = result.indexOf(",");
      const data_base64 = commaIdx >= 0 ? result.slice(commaIdx + 1) : result;
      resolve({
        name: file.name || "attachment",
        media_type: file.type || "application/octet-stream",
        data_base64,
      });
    };
    reader.onerror = () => {
      reject(new Error(`Failed to read file ${file.name}`));
    };
    reader.readAsDataURL(file);
  });
}

export const ChatPane: React.FC<ChatPaneProps> = ({
  agentName,
  agentDisplayName,
  agentRole = null,
  agentProject = null,
  adapter,
  model = null,
  thinkingLevel = null,
  availableModels = null,
  sessionId,
  turns,
  contextUsage = null,
  quotaUsage = null,
  isBusy,
  queue,
  loading = false,
  error = null,
  wsConnected = false,
  slashCommands,
  agentSettingsOpen,
  onOpenAgentSettings,
  onCloseAgentSettings,
  splitView = false,
  onToggleSplitView,
  onOpenMobileRoster,
  mobileSidebarOpen = false,
  onRefresh,
  onSendPrompt,
  onSendQueuedNow,
  onCancelQueued,
  onUpdateQueued,
  onDecommissionSession,
  onInterruptSession,
  onSetSessionModel,
  onOpenArtifact,
  onOpenHelpTopic,
  apiClient,
}) => {
  const [internalTurns, setInternalTurns] = React.useState<TranscriptTurn[]>([]);
  const [internalIsBusy, setInternalIsBusy] = React.useState<boolean>(false);
  const [internalQueue, setInternalQueue] = React.useState<QueuedOperatorPrompt[]>([]);
  const [editingQueueId, setEditingQueueId] = React.useState<string | null>(null);
  const [editingQueueText, setEditingQueueText] = React.useState<string>("");
  const [internalLoading, setInternalLoading] = React.useState<boolean>(false);
  const [internalError, setInternalError] = React.useState<string | null>(null);
  const [internalContextUsage, setInternalContextUsage] =
    React.useState<SessionContextUsage | null>(null);
  const [internalQuotaUsage, setInternalQuotaUsage] =
    React.useState<SessionQuotaUsage | null>(null);
  const [compacting, setCompacting] = React.useState<boolean>(false);
  const [compactFeedback, setCompactFeedback] = React.useState<string | null>(null);
  const [internalAgentSettingsOpen, setInternalAgentSettingsOpen] =
    React.useState<boolean>(false);
  const [clearModalOpen, setClearModalOpen] = React.useState<boolean>(false);
  const [clearHelpVisible, setClearHelpVisible] = React.useState<boolean>(false);
  const [clearing, setClearing] = React.useState<boolean>(false);
  const clearHelpTimerRef = React.useRef<ReturnType<typeof setTimeout> | null>(null);
  const [decommissionModalOpen, setDecommissionModalOpen] = React.useState<boolean>(false);
  const [decommissionHelpVisible, setDecommissionHelpVisible] = React.useState<boolean>(false);
  const [decommissioning, setDecommissioning] = React.useState<boolean>(false);
  const decommissionHelpTimerRef = React.useRef<ReturnType<typeof setTimeout> | null>(null);
  const [interruptArmed, setInterruptArmed] = React.useState<boolean>(false);
  const [interrupting, setInterrupting] = React.useState<boolean>(false);
  const interruptTimerRef = React.useRef<ReturnType<typeof setTimeout> | null>(null);
  const [switchingModel, setSwitchingModel] = React.useState<boolean>(false);
  const [composerHeight, setComposerHeight] = React.useState<number>(80);
  const resizeCleanupRef = React.useRef<(() => void) | null>(null);
  const [internalSlashCommands, setInternalSlashCommands] =
    React.useState<SlashCommandEntry[]>(DEFAULT_SLASH_COMMANDS);
  const [slashMenuDismissed, setSlashMenuDismissed] = React.useState<boolean>(false);
  const [slashActiveIndex, setSlashActiveIndex] = React.useState<number>(0);

  const [filterQuery, setFilterQuery] = React.useState<string>("");
  const [visibleTurnLimit, setVisibleTurnLimit] = React.useState<number>(100);
  const [sinceLastPromptExpanded, setSinceLastPromptExpanded] =
    React.useState<boolean>(false);
  const [sinceLastPromptCategory, setSinceLastPromptCategory] = React.useState<
    "all" | "replies" | "tools" | "wire"
  >("all");
  useActiveTimeZone();
  const targetSessionOrAgent = sessionId || agentName || "";
  const initialDraftRef = React.useRef<SessionDraft | null>(null);
  if (initialDraftRef.current === null) {
    initialDraftRef.current = loadSessionDraft(targetSessionOrAgent);
  }
  const [promptText, setPromptText] = React.useState<string>(
    initialDraftRef.current.promptText,
  );
  const [alwaysSendNow, setAlwaysSendNow] = React.useState<boolean>(() =>
    readInitialAlwaysSendNow(),
  );
  const [attachments, setAttachments] = React.useState<PromptAttachment[]>(
    initialDraftRef.current.attachments,
  );
  // Render-phase draft load on target switch: swap in the incoming
  // target's draft before commit so effects never see the new target
  // paired with the old target's draft.
  const [prevDraftTarget, setPrevDraftTarget] =
    React.useState<string>(targetSessionOrAgent);
  if (prevDraftTarget !== targetSessionOrAgent) {
    const draft = loadSessionDraft(targetSessionOrAgent);
    setPrevDraftTarget(targetSessionOrAgent);
    setPromptText(draft.promptText);
    setAttachments(draft.attachments);
  }
  // Tracks the live target so async completions can tell whether the composer
  // still belongs to the agent/session they were started for.
  const currentTargetRef = React.useRef<string>(targetSessionOrAgent);
  currentTargetRef.current = targetSessionOrAgent;
  const [submittingByTarget, setSubmittingByTarget] = React.useState<
    Record<string, number>
  >({});
  const submitting = Boolean(
    targetSessionOrAgent && (submittingByTarget[targetSessionOrAgent] ?? 0) > 0,
  );
  const beginTargetSubmitting = React.useCallback((targetKey: string) => {
    if (!targetKey) return;
    setSubmittingByTarget((prev) => ({
      ...prev,
      [targetKey]: (prev[targetKey] ?? 0) + 1,
    }));
  }, []);
  const endTargetSubmitting = React.useCallback((targetKey: string) => {
    if (!targetKey) return;
    setSubmittingByTarget((prev) => {
      const nextCount = (prev[targetKey] ?? 0) - 1;
      if (nextCount <= 0) {
        const { [targetKey]: _removed, ...rest } = prev;
        return rest;
      }
      return { ...prev, [targetKey]: nextCount };
    });
  }, []);
  const [submitError, setSubmitError] = React.useState<string | null>(null);

  const scrollContainerRef = React.useRef<HTMLDivElement | null>(null);
  const fileInputRef = React.useRef<HTMLInputElement | null>(null);
  const textareaRef = React.useRef<HTMLTextAreaElement | null>(null);
  const prevTargetRef = React.useRef<string>("");
  const prevWsConnectedRef = React.useRef<boolean>(Boolean(wsConnected));
  const prevTurnCountRef = React.useRef<number>(0);
  const prevLastTurnKeyRef = React.useRef<string>("");
  const isNearBottomRef = React.useRef<boolean>(true);

  const effectiveTurns = turns ?? internalTurns;
  const effectiveIsBusy = isBusy ?? internalIsBusy;
  const effectiveQueue = queue ?? internalQueue;
  const effectiveLoading = loading || internalLoading;
  const effectiveError = error ?? internalError;

  const displayTarget = agentDisplayName || agentName || sessionId || "Agent";

  React.useEffect(() => {
    if (!targetSessionOrAgent) {
      return;
    }
    if (promptText.length === 0 && attachments.length === 0) {
      clearSessionDraft(targetSessionOrAgent);
      return;
    }
    sessionDraftMemory.set(targetSessionOrAgent, { promptText, attachments });
    try {
      if (typeof window !== "undefined" && window.localStorage) {
        window.localStorage.setItem(
          DRAFT_STORAGE_PREFIX + targetSessionOrAgent,
          promptText,
        );
      }
    } catch {
      // Ignore storage errors.
    }
  }, [promptText, attachments, targetSessionOrAgent]);

  const contextEstimate = React.useMemo(
    () =>
      estimateSessionContextUsage(
        effectiveTurns,
        adapter,
        contextUsage ?? internalContextUsage,
      ),
    [effectiveTurns, adapter, contextUsage, internalContextUsage],
  );
  const effectiveQuotaUsage = quotaUsage ?? internalQuotaUsage;
  const showQuotaBadge = hasQuotaUsageData(effectiveQuotaUsage);
  const quotaExhausted = Boolean(
    effectiveQuotaUsage &&
      ((effectiveQuotaUsage.exhausted_reason &&
        effectiveQuotaUsage.exhausted_reason.trim().length > 0) ||
        (effectiveQuotaUsage.primary_window?.used_percent ?? 0) >= 100 ||
        (effectiveQuotaUsage.secondary_window?.used_percent ?? 0) >= 100),
  );
  const canCompact =
    contextEstimate.percent >= 50 &&
    Boolean(targetSessionOrAgent) &&
    !compacting &&
    !clearing &&
    !decommissioning &&
    !submitting;
  const canClear =
    Boolean(targetSessionOrAgent) && !clearing && !decommissioning && !compacting && !submitting;
  const canDecommission =
    Boolean(targetSessionOrAgent) && !decommissioning && !clearing && !compacting && !submitting;
  const canInterrupt =
    Boolean(targetSessionOrAgent) && !interrupting && !decommissioning && !clearing;

  const effectiveAgentSettingsOpen = agentSettingsOpen ?? internalAgentSettingsOpen;
  const handleOpenAgentSettings = React.useCallback(() => {
    setInternalAgentSettingsOpen(true);
    onOpenAgentSettings?.();
  }, [onOpenAgentSettings]);
  const handleCloseAgentSettings = React.useCallback(() => {
    setInternalAgentSettingsOpen(false);
    onCloseAgentSettings?.();
  }, [onCloseAgentSettings]);

  const [selectedModel, setSelectedModel] = React.useState<string>(
    () =>
      (model && model.trim()) ||
      getVendorModelOptions(adapter, model, availableModels)[0] ||
      "",
  );
  const [selectedThinkingLevel, setSelectedThinkingLevel] = React.useState<string>(
    () => (thinkingLevel && thinkingLevel.trim().toLowerCase()) || "high",
  );

  React.useEffect(() => {
    const trimmed = (model ?? "").trim();
    if (trimmed) {
      setSelectedModel(trimmed);
    } else {
      const opts = getVendorModelOptions(adapter, model, availableModels);
      setSelectedModel(opts[0] ?? "");
    }
  }, [model, adapter, availableModels, targetSessionOrAgent]);

  React.useEffect(() => {
    const trimmedThinking = (thinkingLevel ?? "").trim().toLowerCase();
    if (trimmedThinking) {
      setSelectedThinkingLevel(trimmedThinking);
    } else {
      setSelectedThinkingLevel("high");
    }
  }, [thinkingLevel, targetSessionOrAgent]);

  React.useEffect(() => {
    if (slashCommands) {
      return;
    }
    if (!apiClient) {
      return;
    }
    let cancelled = false;
    apiClient
      .listSlashCommands()
      .then((res) => {
        if (!cancelled && Array.isArray(res.commands) && res.commands.length > 0) {
          setInternalSlashCommands(res.commands);
        }
      })
      .catch(() => {
        // Fallback to DEFAULT_SLASH_COMMANDS
      });
    return () => {
      cancelled = true;
    };
  }, [apiClient, slashCommands]);

  const availableSlashCommands = slashCommands ?? internalSlashCommands;

  const slashQuery = React.useMemo(() => {
    if (!promptText.startsWith("/")) {
      return null;
    }
    const afterSlash = promptText.slice(1);
    if (/\s/.test(afterSlash)) {
      return null;
    }
    return afterSlash.toLowerCase();
  }, [promptText]);

  const matchingSlashCommands = React.useMemo(() => {
    if (slashQuery === null) {
      return [];
    }
    return availableSlashCommands.filter((cmd) => {
      const cleanName = cmd.name.replace(/^\/+/, "").toLowerCase();
      return (
        cleanName.startsWith(slashQuery) ||
        cleanName.includes(slashQuery)
      );
    });
  }, [availableSlashCommands, slashQuery]);

  const showSlashMenu =
    slashQuery !== null && !slashMenuDismissed && matchingSlashCommands.length > 0;

  React.useEffect(() => {
    setSlashMenuDismissed(false);
    setSlashActiveIndex(0);
  }, [promptText]);

  const handleSelectSlashCommand = (cmd: SlashCommandEntry) => {
    const cleanName = cmd.name.replace(/^\/+/, "");
    setPromptText(`/${cleanName} `);
    setSlashMenuDismissed(true);
    setSlashActiveIndex(0);
  };

  React.useEffect(() => {
    return () => {
      if (clearHelpTimerRef.current !== null) {
        clearTimeout(clearHelpTimerRef.current);
      }
      if (decommissionHelpTimerRef.current !== null) {
        clearTimeout(decommissionHelpTimerRef.current);
      }
      if (interruptTimerRef.current !== null) {
        clearTimeout(interruptTimerRef.current);
      }
      resizeCleanupRef.current?.();
    };
  }, []);

  const handleComposerResizeStart = React.useCallback(
    (e: React.MouseEvent<HTMLDivElement>) => {
      e.preventDefault();
      resizeCleanupRef.current?.();
      const startY = e.clientY;
      const startHeight = textareaRef.current?.offsetHeight || composerHeight;
      const handleMouseMove = (moveEvent: MouseEvent) => {
        const delta = startY - moveEvent.clientY;
        const nextHeight = Math.max(72, Math.min(420, startHeight + delta));
        setComposerHeight(nextHeight);
      };
      const handleMouseUp = () => {
        window.removeEventListener("mousemove", handleMouseMove);
        window.removeEventListener("mouseup", handleMouseUp);
        resizeCleanupRef.current = null;
      };
      window.addEventListener("mousemove", handleMouseMove);
      window.addEventListener("mouseup", handleMouseUp);
      resizeCleanupRef.current = handleMouseUp;
    },
    [composerHeight],
  );

  const disarmInterrupt = React.useCallback(() => {
    if (interruptTimerRef.current !== null) {
      clearTimeout(interruptTimerRef.current);
      interruptTimerRef.current = null;
    }
    setInterruptArmed(false);
  }, []);

  React.useEffect(() => {
    disarmInterrupt();
  }, [targetSessionOrAgent, disarmInterrupt]);

  const handleArmInterrupt = () => {
    if (!canInterrupt) return;
    if (interruptTimerRef.current !== null) {
      clearTimeout(interruptTimerRef.current);
    }
    setInterruptArmed(true);
    interruptTimerRef.current = setTimeout(() => {
      setInterruptArmed(false);
      interruptTimerRef.current = null;
    }, 3_000);
  };

  const handleConfirmInterrupt = async () => {
    if (!targetSessionOrAgent || interrupting) {
      return;
    }
    const submittedTarget = targetSessionOrAgent;
    disarmInterrupt();
    setInterrupting(true);
    setCompactFeedback(null);
    setSubmitError(null);
    try {
      if (onInterruptSession) {
        await onInterruptSession();
      } else if (apiClient) {
        await apiClient.interruptSession(submittedTarget);
        if (currentTargetRef.current === submittedTarget) {
          setInternalIsBusy(false);
        }
        if (onRefresh) {
          onRefresh();
        }
      } else {
        throw new Error("No session route is configured for interrupt.");
      }
      if (currentTargetRef.current === submittedTarget) {
        setCompactFeedback("Stopped session");
      }
    } catch (err) {
      if (currentTargetRef.current === submittedTarget) {
        setSubmitError(
          err instanceof Error ? err.message : "Failed to interrupt session",
        );
      }
    } finally {
      setInterrupting(false);
    }
  };

  const handleChangeModelAndThinking = async (
    nextModel: string,
    nextThinkingLevel?: string,
  ) => {
    const prevModel = selectedModel;
    const prevThinking = selectedThinkingLevel;
    const effectiveThinking = (nextThinkingLevel ?? selectedThinkingLevel ?? "high")
      .trim()
      .toLowerCase();
    setSelectedModel(nextModel);
    setSelectedThinkingLevel(effectiveThinking);
    if (!targetSessionOrAgent || !nextModel || switchingModel) {
      return;
    }
    setSwitchingModel(true);
    setCompactFeedback(null);
    setSubmitError(null);
    try {
      if (onSetSessionModel) {
        await onSetSessionModel(nextModel, effectiveThinking);
      } else if (apiClient) {
        await apiClient.setSessionModel(
          targetSessionOrAgent,
          nextModel,
          effectiveThinking,
        );
        if (onRefresh) {
          onRefresh();
        }
      } else {
        throw new Error("No session route is configured for switching model.");
      }
      setCompactFeedback(`Model: ${nextModel} (${effectiveThinking})`);
    } catch (err) {
      setSelectedModel(prevModel);
      setSelectedThinkingLevel(prevThinking);
      setSubmitError(
        err instanceof Error ? err.message : "Failed to switch model",
      );
    } finally {
      setSwitchingModel(false);
    }
  };

  const handleOpenClearModal = () => {
    if (!canClear) return;
    if (clearHelpTimerRef.current !== null) {
      clearTimeout(clearHelpTimerRef.current);
      clearHelpTimerRef.current = null;
    }
    setClearHelpVisible(false);
    setClearModalOpen(true);
  };

  const handleCloseClearModal = React.useCallback(() => {
    if (clearHelpTimerRef.current !== null) {
      clearTimeout(clearHelpTimerRef.current);
      clearHelpTimerRef.current = null;
    }
    setClearHelpVisible(false);
    setClearModalOpen(false);
  }, []);

  React.useEffect(() => {
    handleCloseClearModal();
  }, [targetSessionOrAgent, handleCloseClearModal]);

  React.useEffect(() => {
    if (!clearModalOpen) return;
    const handleKeyDownEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !clearing) {
        handleCloseClearModal();
      }
    };
    window.addEventListener("keydown", handleKeyDownEscape);
    return () => window.removeEventListener("keydown", handleKeyDownEscape);
  }, [clearModalOpen, clearing, handleCloseClearModal]);

  const handleShowClearHelp = () => {
    if (clearHelpTimerRef.current !== null) {
      clearTimeout(clearHelpTimerRef.current);
    }
    setClearHelpVisible(true);
    clearHelpTimerRef.current = setTimeout(() => {
      setClearHelpVisible(false);
      clearHelpTimerRef.current = null;
    }, 5_000);
  };

  const handleClearOrReforge = async (mode: ClearSessionMode) => {
    if (!targetSessionOrAgent || clearing) {
      return;
    }
    setClearing(true);
    setCompactFeedback(null);
    setSubmitError(null);
    try {
      if (apiClient) {
        await apiClient.clearSession(targetSessionOrAgent, mode);
        if (turns === undefined) {
          void fetchTranscript();
        }
        if (onRefresh) {
          onRefresh();
        }
      } else if (onSendPrompt) {
        await onSendPrompt(targetSessionOrAgent, {
          prompt: "/clear",
          subject: mode === "reforge" ? "Reforge Context" : "Clear Context",
          urgency: "normal",
          send_now: true,
        });
      } else {
        throw new Error("No session route is configured for clearing.");
      }
      handleCloseClearModal();
      setCompactFeedback(mode === "reforge" ? "Reforged session" : "Cleared context");
    } catch (err) {
      setSubmitError(
        err instanceof Error ? err.message : `Failed to ${mode} session`,
      );
      handleCloseClearModal();
    } finally {
      setClearing(false);
    }
  };

  const handleOpenDecommissionModal = () => {
    if (!canDecommission) return;
    if (decommissionHelpTimerRef.current !== null) {
      clearTimeout(decommissionHelpTimerRef.current);
      decommissionHelpTimerRef.current = null;
    }
    setDecommissionHelpVisible(false);
    setDecommissionModalOpen(true);
  };

  const handleCloseDecommissionModal = React.useCallback(() => {
    if (decommissionHelpTimerRef.current !== null) {
      clearTimeout(decommissionHelpTimerRef.current);
      decommissionHelpTimerRef.current = null;
    }
    setDecommissionHelpVisible(false);
    setDecommissionModalOpen(false);
  }, []);

  React.useEffect(() => {
    handleCloseDecommissionModal();
  }, [targetSessionOrAgent, handleCloseDecommissionModal]);

  React.useEffect(() => {
    if (!decommissionModalOpen) return;
    const handleKeyDownEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !decommissioning) {
        handleCloseDecommissionModal();
      }
    };
    window.addEventListener("keydown", handleKeyDownEscape);
    return () => window.removeEventListener("keydown", handleKeyDownEscape);
  }, [decommissionModalOpen, decommissioning, handleCloseDecommissionModal]);

  const handleShowDecommissionHelp = () => {
    if (decommissionHelpTimerRef.current !== null) {
      clearTimeout(decommissionHelpTimerRef.current);
    }
    setDecommissionHelpVisible(true);
    decommissionHelpTimerRef.current = setTimeout(() => {
      setDecommissionHelpVisible(false);
      decommissionHelpTimerRef.current = null;
    }, 6_000);
  };

  const handleDecommission = async (mode: DecommissionMode) => {
    if (!targetSessionOrAgent || decommissioning) {
      return;
    }
    const submittedTarget = targetSessionOrAgent;
    setDecommissioning(true);
    setCompactFeedback(null);
    setSubmitError(null);
    try {
      if (onDecommissionSession) {
        await onDecommissionSession(mode);
      } else if (apiClient) {
        await apiClient.decommissionSession(submittedTarget, mode);
        if (currentTargetRef.current === submittedTarget) {
          if (mode === "teardown") {
            setInternalTurns([]);
            setInternalQueue([]);
            setInternalIsBusy(false);
          } else if (turns === undefined) {
            void fetchTranscript();
          }
        }
        if (onRefresh) {
          onRefresh();
        }
      } else {
        throw new Error("No session route is configured for decommissioning.");
      }
      handleCloseDecommissionModal();
      if (currentTargetRef.current === submittedTarget) {
        setCompactFeedback(
          mode === "prepare" ? "Prepared decommission" : "Tore down session",
        );
      }
    } catch (err) {
      if (currentTargetRef.current === submittedTarget) {
        setSubmitError(
          err instanceof Error ? err.message : `Failed to ${mode} session`,
        );
      }
      handleCloseDecommissionModal();
    } finally {
      setDecommissioning(false);
    }
  };

  const handleCompactContext = async () => {
    if (!canCompact) {
      return;
    }
    const submittedTarget = targetSessionOrAgent;
    setCompacting(true);
    setCompactFeedback(null);
    setSubmitError(null);
    // Best effort only: a missing checkpoint must not block compaction.
    if (apiClient) {
      try {
        await apiClient.createStateCheckpoint({
          summary: "Pre-compaction automatic checkpoint from FerroDeck",
        });
      } catch {
        // Ignore checkpoint failures.
      }
    }
    const payload: PromptRequest = { prompt: "/compact", send_now: true };
    try {
      if (onSendPrompt) {
        await onSendPrompt(submittedTarget, payload);
      } else if (apiClient) {
        const res = await apiClient.sendSessionPrompt(submittedTarget, payload);
        if (currentTargetRef.current === submittedTarget) {
          if (Array.isArray(res.queue)) {
            setInternalQueue(res.queue);
          }
          if (typeof res.is_busy === "boolean") {
            setInternalIsBusy(res.is_busy);
          }
          if (!res.queued && res.status !== "queued") {
            const content = res.effective_body || payload.prompt;
            setInternalTurns((prev) => [
              ...prev,
              {
                index: prev.length,
                role: "user",
                content,
                thinking: null,
                tool_calls: [],
                created_at: new Date().toISOString(),
              },
            ]);
          }
          void fetchTranscript();
        }
      } else {
        throw new Error("No message route is configured for this session.");
      }
      if (currentTargetRef.current === submittedTarget) {
        setCompactFeedback("Sent /compact");
        scrollToBottom("smooth");
        requestAnimationFrame(() => scrollToBottom("smooth"));
      }
    } catch (err) {
      if (currentTargetRef.current === submittedTarget) {
        setSubmitError(err instanceof Error ? err.message : "Failed to send /compact");
      }
    } finally {
      setCompacting(false);
    }
  };

  const scrollToBottom = React.useCallback(
    (behavior: ScrollBehavior = "auto") => {
      const el = scrollContainerRef.current;
      if (!el) return;
      el.scrollTop = el.scrollHeight;
      if (typeof el.scrollTo === "function") {
        el.scrollTo({ top: el.scrollHeight, behavior });
      }
      isNearBottomRef.current = true;
      if (targetSessionOrAgent) {
        sessionScrollMemory.set(targetSessionOrAgent, {
          scrollTop: el.scrollHeight,
          pinnedToBottom: true,
        });
      }
    },
    [targetSessionOrAgent],
  );

  const restoreOrScrollSession = React.useCallback(
    (behavior: ScrollBehavior = "auto") => {
      const el = scrollContainerRef.current;
      if (!el) return;
      const saved = targetSessionOrAgent
        ? sessionScrollMemory.get(targetSessionOrAgent)
        : undefined;
      if (saved && !saved.pinnedToBottom) {
        el.scrollTop = saved.scrollTop;
        if (typeof el.scrollTo === "function") {
          el.scrollTo({ top: saved.scrollTop, behavior: "auto" });
        }
        isNearBottomRef.current = false;
      } else {
        scrollToBottom(behavior);
      }
    },
    [targetSessionOrAgent, scrollToBottom],
  );

  const handleScroll = () => {
    const el = scrollContainerRef.current;
    if (!el) return;
    const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    const pinnedToBottom = distanceFromBottom <= 120;
    isNearBottomRef.current = pinnedToBottom;
    if (targetSessionOrAgent) {
      sessionScrollMemory.set(targetSessionOrAgent, {
        scrollTop: el.scrollTop,
        pinnedToBottom,
      });
    }
  };

  const fetchTranscript = React.useCallback(async () => {
    if (!apiClient || turns !== undefined) {
      return;
    }
    const targetId = sessionId || agentName;
    if (!targetId) {
      setInternalTurns([]);
      return;
    }
    setInternalLoading(true);
    setInternalError(null);
    try {
      const response = await apiClient.getSessionTranscript(targetId);
      if (currentTargetRef.current === targetId) {
        setInternalTurns(response.turns ?? []);
        setInternalIsBusy(Boolean(response.is_busy));
        setInternalQueue(response.queue ?? []);
        setInternalContextUsage(response.context_usage ?? null);
        setInternalQuotaUsage(response.quota_usage ?? null);
      }
    } catch (err) {
      if (currentTargetRef.current === targetId) {
        setInternalError(
          err instanceof Error ? err.message : "Failed to load transcript",
        );
      }
    } finally {
      if (currentTargetRef.current === targetId) {
        setInternalLoading(false);
      }
    }
  }, [apiClient, turns, sessionId, agentName]);

  React.useEffect(() => {
    void fetchTranscript();
  }, [fetchTranscript]);

  const filteredTurns = React.useMemo(() => {
    const q = filterQuery.trim().toLowerCase();
    if (!q) {
      return effectiveTurns;
    }
    return effectiveTurns.filter((t) => {
      if (t.content.toLowerCase().includes(q)) return true;
      if (t.thinking && t.thinking.toLowerCase().includes(q)) return true;
      if (t.role.toLowerCase().includes(q)) return true;
      return (t.tool_calls ?? []).some(
        (tc) =>
          tc.name.toLowerCase().includes(q) ||
          JSON.stringify(tc.arguments ?? "").toLowerCase().includes(q),
      );
    });
  }, [effectiveTurns, filterQuery]);

  const { displayedTurns, hiddenTurnCount } = React.useMemo(() => {
    if (filterQuery.trim() || filteredTurns.length <= visibleTurnLimit) {
      return { displayedTurns: filteredTurns, hiddenTurnCount: 0 };
    }
    const hidden = filteredTurns.length - visibleTurnLimit;
    return {
      displayedTurns: filteredTurns.slice(hidden),
      hiddenTurnCount: hidden,
    };
  }, [filteredTurns, filterQuery, visibleTurnLimit]);

  React.useEffect(() => {
    setVisibleTurnLimit(100);
    setSinceLastPromptExpanded(false);
  }, [targetSessionOrAgent]);

  const sinceLastPrompt = React.useMemo(
    () => computeSinceLastPromptSummary(effectiveTurns),
    [effectiveTurns],
  );

  const scrollToTurnIndex = React.useCallback(
    (turnIndex: number) => {
      if (typeof window !== "undefined") {
        window.dispatchEvent(
          new CustomEvent(EXPAND_TURN_EVENT, { detail: { turnIndex } }),
        );
      }
      const container = scrollContainerRef.current;
      const tryScroll = () => {
        const root = container ?? document;
        const card =
          root.querySelector<HTMLElement>(
            `[data-testid="turn-card-${turnIndex}"]`,
          ) ??
          Array.from(
            root.querySelectorAll<HTMLElement>("[data-turn-indices]"),
          ).find((el) =>
            (el.getAttribute("data-turn-indices") || "")
              .split(",")
              .includes(String(turnIndex)),
          ) ??
          null;
        if (card) {
          if (typeof card.scrollIntoView === "function") {
            card.scrollIntoView({ behavior: "smooth", block: "center" });
          }
          return true;
        }
        return false;
      };
      if (tryScroll()) {
        return;
      }
      // If the turn is inside hidden older history, expand the visible window first.
      if (filteredTurns.length > visibleTurnLimit) {
        setVisibleTurnLimit(filteredTurns.length);
        requestAnimationFrame(() => {
          tryScroll();
        });
      }
    },
    [filteredTurns.length, visibleTurnLimit],
  );

  const lastTurnKey = React.useMemo(
    () => computeLastTurnKey(filteredTurns),
    [filteredTurns],
  );

  React.useEffect(() => {
    const targetChanged = prevTargetRef.current !== targetSessionOrAgent;
    const reconnected = !prevWsConnectedRef.current && Boolean(wsConnected);
    const initialLoad = prevTurnCountRef.current === 0 && filteredTurns.length > 0;
    // Count alone misses new turns in a full sliding window; the tail key
    // changes on every new or streamed turn.
    const newTurnsArrived = hasNewTurnActivity(
      prevTurnCountRef.current,
      prevLastTurnKeyRef.current,
      filteredTurns,
    );

    if (targetChanged) {
      prevTargetRef.current = targetSessionOrAgent;
      if (filteredTurns.length > 0) {
        restoreOrScrollSession("auto");
      } else {
        const saved = targetSessionOrAgent
          ? sessionScrollMemory.get(targetSessionOrAgent)
          : undefined;
        isNearBottomRef.current = saved ? saved.pinnedToBottom : true;
      }
    } else if (reconnected || initialLoad) {
      restoreOrScrollSession("auto");
    } else if (newTurnsArrived && isNearBottomRef.current) {
      scrollToBottom("auto");
    }

    prevWsConnectedRef.current = Boolean(wsConnected);
    prevTurnCountRef.current = filteredTurns.length;
    prevLastTurnKeyRef.current = lastTurnKey;
  }, [
    targetSessionOrAgent,
    filteredTurns.length,
    lastTurnKey,
    filteredTurns,
    wsConnected,
    scrollToBottom,
    restoreOrScrollSession,
  ]);

  const handleJumpToBottom = () => {
    scrollToBottom("smooth");
  };

  const readFilesAsAttachments = React.useCallback(async (files: FileList | File[]) => {
    const fileArray = Array.from(files);
    if (fileArray.length === 0) return;
    try {
      const loaded = await Promise.all(fileArray.map((f) => fileToPromptAttachment(f)));
      setAttachments((prev) => [...prev, ...loaded]);
    } catch (err) {
      setSubmitError(err instanceof Error ? err.message : "Failed to read attachment");
    }
  }, []);

  const handleFileChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    if (e.target.files && e.target.files.length > 0) {
      void readFilesAsAttachments(e.target.files);
      e.target.value = "";
    }
  };

  const handlePaste = (e: React.ClipboardEvent<HTMLTextAreaElement>) => {
    if (e.clipboardData?.files && e.clipboardData.files.length > 0) {
      e.preventDefault();
      void readFilesAsAttachments(e.clipboardData.files);
    }
  };

  const handleToggleAlwaysSendNow = (next: boolean) => {
    setAlwaysSendNow(next);
    try {
      if (typeof window !== "undefined" && window.localStorage) {
        window.localStorage.setItem(ALWAYS_SEND_NOW_STORAGE_KEY, String(next));
      }
    } catch {
      // Ignore storage errors
    }
  };

  const handleSendQueuedNow = async (promptId: string) => {
    if (!targetSessionOrAgent) return;
    const submittedTarget = targetSessionOrAgent;
    try {
      if (onSendQueuedNow) {
        await onSendQueuedNow(promptId);
      } else if (apiClient) {
        const res = await apiClient.sendQueuedPromptNow(submittedTarget, promptId);
        if (currentTargetRef.current === submittedTarget) {
          if (Array.isArray(res.queue)) {
            setInternalQueue(res.queue);
          }
          if (typeof res.is_busy === "boolean") {
            setInternalIsBusy(res.is_busy);
          }
          const content = res.effective_body || res.prompt;
          if (content) {
            setInternalTurns((prev) => [
              ...prev,
              {
                index: prev.length,
                role: "user",
                content,
                thinking: null,
                tool_calls: [],
                created_at: new Date().toISOString(),
              },
            ]);
          }
        }
      }
      if (currentTargetRef.current === submittedTarget) {
        scrollToBottom("smooth");
      }
    } catch (err) {
      if (currentTargetRef.current === submittedTarget) {
        setSubmitError(
          err instanceof Error ? err.message : "Failed to send queued message",
        );
      }
    }
  };

  const handleCancelQueued = async (promptId: string) => {
    if (!targetSessionOrAgent) return;
    const submittedTarget = targetSessionOrAgent;
    if (editingQueueId === promptId) {
      setEditingQueueId(null);
      setEditingQueueText("");
    }
    try {
      if (onCancelQueued) {
        await onCancelQueued(promptId);
      } else if (apiClient) {
        const res = await apiClient.cancelQueuedPrompt(submittedTarget, promptId);
        if (currentTargetRef.current === submittedTarget) {
          if (Array.isArray(res.queue)) {
            setInternalQueue(res.queue);
          }
          if (typeof res.is_busy === "boolean") {
            setInternalIsBusy(res.is_busy);
          }
        }
      }
    } catch (err) {
      if (currentTargetRef.current === submittedTarget) {
        setSubmitError(
          err instanceof Error ? err.message : "Failed to cancel queued message",
        );
      }
    }
  };

  const handleSaveEditedQueued = async (promptId: string) => {
    const trimmed = editingQueueText.trim();
    if (!targetSessionOrAgent || !trimmed) return;
    const submittedTarget = targetSessionOrAgent;
    try {
      if (onUpdateQueued) {
        await onUpdateQueued(promptId, trimmed);
      } else if (apiClient) {
        const res = await apiClient.updateQueuedPrompt(
          submittedTarget,
          promptId,
          trimmed,
        );
        if (currentTargetRef.current === submittedTarget) {
          if (Array.isArray(res.queue)) {
            setInternalQueue(res.queue);
          }
          if (typeof res.is_busy === "boolean") {
            setInternalIsBusy(res.is_busy);
          }
        }
      } else {
        setInternalQueue((prev) =>
          prev.map((item) =>
            item.id === promptId
              ? { ...item, prompt: trimmed, effective_body: trimmed }
              : item,
          ),
        );
      }
      if (currentTargetRef.current === submittedTarget) {
        setEditingQueueId(null);
        setEditingQueueText("");
      }
    } catch (err) {
      if (currentTargetRef.current === submittedTarget) {
        setSubmitError(
          err instanceof Error ? err.message : "Failed to update queued message",
        );
      }
    }
  };

  const canSend =
    (promptText.trim().length > 0 || attachments.length > 0) &&
    !submitting &&
    Boolean(targetSessionOrAgent);

  const submitPrompt = async () => {
    const trimmedPrompt = promptText.trim();
    if ((!trimmedPrompt && attachments.length === 0) || submitting || !targetSessionOrAgent) {
      return;
    }

    const payload: PromptRequest = {
      prompt: trimmedPrompt || "Attached file(s).",
      urgency: "normal",
      subject: undefined,
      ...(attachments.length > 0 ? { attachments } : {}),
      ...(alwaysSendNow ? { send_now: true } : {}),
    };

    const submittedTarget = targetSessionOrAgent;
    beginTargetSubmitting(submittedTarget);
    setSubmitError(null);
    try {
      let wasQueued = false;
      if (onSendPrompt) {
        const res = (await onSendPrompt(submittedTarget, payload)) as
          | PromptResponse
          | undefined;
        if (res && (res.queued || res.status === "queued")) {
          wasQueued = true;
        }
      } else if (apiClient) {
        const res = await apiClient.sendSessionPrompt(submittedTarget, payload);
        if (res.queued || res.status === "queued") {
          wasQueued = true;
        }
        if (currentTargetRef.current === submittedTarget) {
          if (Array.isArray(res.queue)) {
            setInternalQueue(res.queue);
          }
          if (typeof res.is_busy === "boolean") {
            setInternalIsBusy(res.is_busy);
          }
          if (!wasQueued) {
            const content = res.effective_body || payload.prompt;
            setInternalTurns((prev) => [
              ...prev,
              {
                index: prev.length,
                role: "user",
                content,
                thinking: null,
                tool_calls: [],
                created_at: new Date().toISOString(),
              },
            ]);
          }
          await fetchTranscript();
        }
      }
      clearSessionDraft(submittedTarget);
      // The operator may have switched agents while the request was in
      // flight: the composer now holds another target's draft, which must
      // not be wiped (or scrolled) on behalf of the finished send.
      if (currentTargetRef.current === submittedTarget) {
        setPromptText("");
        setAttachments([]);
        if (!wasQueued) {
          scrollToBottom("smooth");
          // Run again after the optimistic turn and live indicator render.
          requestAnimationFrame(() => scrollToBottom("smooth"));
        }
      }
    } catch (err) {
      if (currentTargetRef.current === submittedTarget) {
        setSubmitError(err instanceof Error ? err.message : "Failed to send message");
      }
    } finally {
      endTargetSubmitting(submittedTarget);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (showSlashMenu) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setSlashActiveIndex((prev) => (prev + 1) % matchingSlashCommands.length);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        setSlashActiveIndex(
          (prev) =>
            (prev - 1 + matchingSlashCommands.length) %
            matchingSlashCommands.length,
        );
        return;
      }
      if (
        (e.key === "Enter" && !e.metaKey && !e.ctrlKey && !e.shiftKey) ||
        e.key === "Tab"
      ) {
        const chosen =
          matchingSlashCommands[slashActiveIndex] ?? matchingSlashCommands[0];
        if (chosen) {
          e.preventDefault();
          handleSelectSlashCommand(chosen);
          return;
        }
      }
      if (e.key === "Escape") {
        e.preventDefault();
        setSlashMenuDismissed(true);
        return;
      }
    }
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      void submitPrompt();
    }
  };

  return (
    <section className="fd-pane" data-testid="chat-pane">
      {/* Toolbar */}
      <div
        className="fd-chat-toolbar"
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: "8px",
          padding: "6px 16px",
          borderBottom: "1px solid var(--border-subtle)",
          backgroundColor: "var(--bg-surface)",
          flexWrap: "nowrap",
        }}
      >
        <div
          className="chat-header-meta"
          style={{
            display: "flex",
            alignItems: "center",
            gap: "8px",
            flex: 1,
            flexWrap: "nowrap",
            minWidth: 0,
          }}
        >
          {onOpenMobileRoster && (
            <button
              type="button"
              className={`fd-btn fd-mobile-sidebar-toggle${mobileSidebarOpen ? " fd-btn-active" : ""}`}
              onClick={onOpenMobileRoster}
              aria-label="Toggle Agent Roster"
              title="Toggle Agent Roster"
              aria-expanded={mobileSidebarOpen}
              data-testid="mobile-sidebar-toggle-btn"
              style={{ flexShrink: 0 }}
            >
              <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                <rect
                  x="1.5"
                  y="2.5"
                  width="13"
                  height="11"
                  rx="2"
                  stroke="currentColor"
                  strokeWidth="1.4"
                />
                <line
                  x1="6.5"
                  y1="2.5"
                  x2="6.5"
                  y2="13.5"
                  stroke="currentColor"
                  strokeWidth="1.4"
                />
                <rect
                  x="2.2"
                  y="3.2"
                  width="3.6"
                  height="9.6"
                  rx="0.8"
                  fill="currentColor"
                  fillOpacity={mobileSidebarOpen ? 0.45 : 0.12}
                />
              </svg>
            </button>
          )}
          <input
            type="search"
            name="fd-transcript-filter"
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            data-1p-ignore="true"
            data-lpignore="true"
            data-form-type="other"
            className="fd-input fd-transcript-filter-input"
            placeholder="Filter transcript turns, tools, or reasoning..."
            aria-label="Filter transcript"
            value={filterQuery}
            onChange={(e) => setFilterQuery(e.target.value)}
            style={{ width: "100%", maxWidth: "320px", fontSize: "12px", padding: "5px 9px" }}
          />
          <span
            className="fd-badge"
            data-testid="session-busy-badge"
            title={
              effectiveIsBusy
                ? "Agent session is actively working"
                : "Agent session is idle"
            }
            style={{
              backgroundColor: effectiveIsBusy
                ? "rgba(245, 158, 11, 0.16)"
                : "rgba(16, 185, 129, 0.14)",
              color: effectiveIsBusy ? "#FBBF24" : "#34D399",
              borderColor: effectiveIsBusy
                ? "rgba(245, 158, 11, 0.35)"
                : "rgba(16, 185, 129, 0.3)",
              whiteSpace: "nowrap",
              flexShrink: 0,
            }}
          >
            {effectiveIsBusy ? "Busy · Working" : "Idle"}
          </span>
          {showQuotaBadge && effectiveQuotaUsage && quotaExhausted && (
            <button
              type="button"
              data-testid="quota-exhausted-toolbar-pill"
              className="fd-badge fd-quota-badge is-exhausted"
              title={
                effectiveQuotaUsage.exhausted_reason?.trim() ||
                "Vendor account quota exhausted — click to open Agent Settings"
              }
              onClick={handleOpenAgentSettings}
              style={{
                display: "inline-flex",
                alignItems: "center",
                gap: "4px",
                padding: "2px 8px",
                cursor: "pointer",
                whiteSpace: "nowrap",
                borderColor: "var(--diff-del-border, rgba(239, 68, 68, 0.45))",
                backgroundColor: "var(--diff-del-bg, rgba(239, 68, 68, 0.14))",
                color: "var(--diff-del-text, #FCA5A5)",
                fontWeight: 600,
                flexShrink: 0,
              }}
            >
              {effectiveQuotaUsage.exhausted_reason?.trim() || "Quota exhausted"}
            </button>
          )}
          {compactFeedback && (
            <span
              role="status"
              data-testid="compact-feedback"
              className="fd-badge"
              style={{ color: "var(--diff-add-text)", whiteSpace: "nowrap", flexShrink: 0 }}
            >
              {compactFeedback}
            </span>
          )}
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: "6px", flexShrink: 0 }}>
          {(onRefresh || apiClient) && (
            <button
              type="button"
              className="fd-btn"
              data-testid="chat-refresh-btn"
              aria-label={effectiveLoading ? "Syncing..." : "Refresh"}
              title={effectiveLoading ? "Syncing..." : "Refresh"}
              onClick={() => {
                if (onRefresh) {
                  onRefresh();
                } else {
                  void fetchTranscript();
                }
              }}
              disabled={effectiveLoading}
            >
              <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                <path
                  d="M13.5 8A5.5 5.5 0 0 1 3.7 11.45"
                  stroke="currentColor"
                  strokeWidth="1.45"
                  strokeLinecap="round"
                />
                <path
                  d="M2.5 8A5.5 5.5 0 0 1 12.3 4.55"
                  stroke="currentColor"
                  strokeWidth="1.45"
                  strokeLinecap="round"
                />
                <polyline
                  points="10.2 4.8 12.5 4.7 12.2 2.4"
                  stroke="currentColor"
                  strokeWidth="1.45"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                />
                <polyline
                  points="5.8 11.2 3.5 11.3 3.8 13.6"
                  stroke="currentColor"
                  strokeWidth="1.45"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                />
              </svg>
            </button>
          )}
          <button
            type="button"
            className="fd-btn"
            onClick={handleJumpToBottom}
            title="Scroll to latest turn"
          >
            ↓ Latest
          </button>
          <button
            type="button"
            data-testid="split-view-toggle"
            className={`fd-btn${splitView ? " fd-btn-active" : ""}`}
            onClick={() => onToggleSplitView?.()}
            title="Toggle Desktop Split-View (Chat + Secondary Pane)"
            aria-label={splitView ? "Close right split panel" : "Open right split panel"}
            aria-pressed={splitView}
          >
            <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
              <rect x="1.5" y="2.5" width="13" height="11" rx="2" stroke="currentColor" strokeWidth="1.4" />
              <line x1="9.5" y1="2.5" x2="9.5" y2="13.5" stroke="currentColor" strokeWidth="1.4" />
              <rect x="10.2" y="3.2" width="3.6" height="9.6" rx="0.8" fill="currentColor" fillOpacity={splitView ? 0.45 : 0.12} />
            </svg>
          </button>
        </div>
      </div>

      {/* Docked "Since Your Last Prompt" Activity Bar */}
      {sinceLastPrompt && (
        <div
          data-testid="since-last-prompt-bar"
          style={{
            display: "flex",
            flexDirection: "column",
            backgroundColor: "var(--bg-surface)",
            borderBottom: "1px solid var(--border-subtle)",
            padding: "6px 16px",
            gap: "6px",
            flexShrink: 0,
            zIndex: 2,
          }}
        >
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "10px",
              flexWrap: "wrap",
            }}
          >
            <div
              style={{
                display: "flex",
                alignItems: "center",
                gap: "8px",
                flex: 1,
                minWidth: 0,
              }}
            >
              <button
                type="button"
                data-testid="since-last-prompt-toggle"
                aria-expanded={sinceLastPromptExpanded}
                onClick={() => {
                  if (sinceLastPromptExpanded && sinceLastPromptCategory === "all") {
                    setSinceLastPromptExpanded(false);
                  } else {
                    setSinceLastPromptCategory("all");
                    setSinceLastPromptExpanded(true);
                  }
                }}
                style={{
                  background: "transparent",
                  border: "none",
                  padding: "2px 4px",
                  color: "var(--accent-primary, #F59E0B)",
                  fontSize: "11px",
                  cursor: "pointer",
                  flexShrink: 0,
                }}
                title="Click to expand or collapse activity since your last prompt"
              >
                {sinceLastPromptExpanded ? "▾" : "▸"}
              </button>
              <button
                type="button"
                data-testid="since-last-prompt-jump-btn"
                className="fd-badge"
                onClick={() => scrollToTurnIndex(sinceLastPrompt.promptTurnIndex)}
                onMouseEnter={(e) => {
                  e.currentTarget.style.borderColor = "var(--border-accent)";
                }}
                onMouseLeave={(e) => {
                  e.currentTarget.style.borderColor = "var(--border-subtle)";
                }}
                onFocus={(e) => {
                  e.currentTarget.style.borderColor = "var(--border-accent)";
                }}
                onBlur={(e) => {
                  e.currentTarget.style.borderColor = "var(--border-subtle)";
                }}
                style={{
                  background: "var(--bg-elevated)",
                  border: "1px solid var(--border-subtle)",
                  borderRadius: "var(--radius-sm, 4px)",
                  padding: "2px 8px",
                  cursor: "pointer",
                  textAlign: "left",
                  fontFamily: "inherit",
                  maxWidth: "100%",
                  minWidth: 0,
                  overflow: "hidden",
                }}
                title={`Jump to your prompt at turn #${sinceLastPrompt.promptTurnIndex}`}
              >
                <span
                  data-testid="since-last-prompt-text"
                  style={{
                    fontSize: "12px",
                    fontWeight: 500,
                    color: "var(--accent-primary, #F59E0B)",
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                    display: "-webkit-box",
                    WebkitLineClamp: sinceLastPromptExpanded ? 4 : 2,
                    WebkitBoxOrient: "vertical",
                    lineHeight: 1.35,
                  }}
                >
                  {sinceLastPrompt.promptText || "(attachment / empty prompt)"}
                </span>
              </button>
            </div>

            <div
              data-testid="since-last-prompt-counts"
              style={{
                display: "flex",
                alignItems: "center",
                gap: "6px",
                flexWrap: "wrap",
                flexShrink: 0,
              }}
            >
              <button
                type="button"
                data-testid="since-last-prompt-replies-btn"
                aria-pressed={
                  sinceLastPromptExpanded && sinceLastPromptCategory === "replies"
                }
                className="fd-badge fd-mono"
                onClick={() => {
                  if (sinceLastPromptExpanded && sinceLastPromptCategory === "replies") {
                    setSinceLastPromptExpanded(false);
                    setSinceLastPromptCategory("all");
                  } else {
                    setSinceLastPromptCategory("replies");
                    setSinceLastPromptExpanded(true);
                  }
                }}
                style={{
                  fontSize: "10px",
                  padding: "1px 6px",
                  cursor: "pointer",
                  borderColor:
                    sinceLastPromptExpanded && sinceLastPromptCategory === "replies"
                      ? "var(--border-accent)"
                      : undefined,
                }}
                title="Click to filter assistant replies since your last prompt"
              >
                {sinceLastPrompt.assistantRepliesCount}{" "}
                {sinceLastPrompt.assistantRepliesCount === 1 ? "reply" : "replies"}
              </button>
              <button
                type="button"
                data-testid="since-last-prompt-tools-btn"
                aria-pressed={
                  sinceLastPromptExpanded && sinceLastPromptCategory === "tools"
                }
                className="fd-badge fd-mono"
                onClick={() => {
                  if (sinceLastPromptExpanded && sinceLastPromptCategory === "tools") {
                    setSinceLastPromptExpanded(false);
                    setSinceLastPromptCategory("all");
                  } else {
                    setSinceLastPromptCategory("tools");
                    setSinceLastPromptExpanded(true);
                  }
                }}
                style={{
                  fontSize: "10px",
                  padding: "1px 6px",
                  cursor: "pointer",
                  color:
                    sinceLastPrompt.toolCallsCount > 0
                      ? "var(--status-warn, #F59E0B)"
                      : "var(--text-muted)",
                  borderColor:
                    sinceLastPromptExpanded && sinceLastPromptCategory === "tools"
                      ? "var(--border-accent)"
                      : undefined,
                }}
                title={
                  sinceLastPrompt.toolSummaries.length > 0
                    ? sinceLastPrompt.toolSummaries
                        .map((t) => (t.count > 1 ? `${t.name} ×${t.count}` : t.name))
                        .join(", ")
                    : "Click to filter tool calls since your last prompt"
                }
              >
                {sinceLastPrompt.toolCallsCount}{" "}
                {sinceLastPrompt.toolCallsCount === 1 ? "tool call" : "tool calls"}
              </button>
              {sinceLastPrompt.touchedFiles.length > 0 && (
                <span
                  className="fd-badge fd-mono"
                  data-testid="since-last-prompt-files-badge"
                  style={{
                    fontSize: "10px",
                    padding: "1px 6px",
                    color: "var(--diff-add-text, #10B981)",
                  }}
                  title={sinceLastPrompt.touchedFiles.map((f) => f.path).join("\n")}
                >
                  {sinceLastPrompt.touchedFiles.length}{" "}
                  {sinceLastPrompt.touchedFiles.length === 1
                    ? "file touched"
                    : "files touched"}
                </span>
              )}
              {sinceLastPrompt.wireMessagesCount > 0 && (
                <button
                  type="button"
                  data-testid="since-last-prompt-wire-btn"
                  aria-pressed={
                    sinceLastPromptExpanded && sinceLastPromptCategory === "wire"
                  }
                  className="fd-badge fd-mono"
                  onClick={() => {
                    if (sinceLastPromptExpanded && sinceLastPromptCategory === "wire") {
                      setSinceLastPromptExpanded(false);
                      setSinceLastPromptCategory("all");
                    } else {
                      setSinceLastPromptCategory("wire");
                      setSinceLastPromptExpanded(true);
                    }
                  }}
                  style={{
                    fontSize: "10px",
                    padding: "1px 6px",
                    cursor: "pointer",
                    borderColor:
                      sinceLastPromptExpanded && sinceLastPromptCategory === "wire"
                        ? "var(--border-accent)"
                        : undefined,
                  }}
                  title="Click to filter wire messages since your last prompt"
                >
                  {sinceLastPrompt.wireMessagesCount} wire
                </button>
              )}
            </div>
          </div>

          {sinceLastPromptExpanded && (() => {
            const visibleSteps = sinceLastPrompt.steps.filter((step) => {
              if (sinceLastPromptCategory === "replies") {
                return step.kind === "reply";
              }
              if (sinceLastPromptCategory === "tools") {
                return step.kind === "tool" || step.kind === "file";
              }
              if (sinceLastPromptCategory === "wire") {
                return step.kind === "wire";
              }
              return true;
            });

            return (
              <div
                data-testid="since-last-prompt-details"
                data-filter-category={sinceLastPromptCategory}
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
                  paddingTop: "6px",
                  borderTop: "1px solid var(--border-subtle)",
                  maxHeight: "180px",
                  overflowY: "auto",
                }}
              >
                {sinceLastPrompt.touchedFiles.length > 0 &&
                  (sinceLastPromptCategory === "all" ||
                    sinceLastPromptCategory === "tools") && (
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: "6px",
                        flexWrap: "wrap",
                      }}
                    >
                      <span
                        className="fd-mono"
                        style={{
                          fontSize: "10px",
                          fontWeight: 600,
                          color: "var(--text-muted)",
                          textTransform: "uppercase",
                        }}
                      >
                        Files touched:
                      </span>
                      {sinceLastPrompt.touchedFiles.map((file) => (
                        <button
                          key={file.path}
                          type="button"
                          className="fd-badge fd-mono"
                          data-testid={`since-last-prompt-file-${file.basename}`}
                          onClick={() => scrollToTurnIndex(file.turnIndex)}
                          title={`${file.path} (turn #${file.turnIndex})`}
                          style={{
                            fontSize: "10px",
                            padding: "1px 6px",
                            cursor: "pointer",
                            color: "var(--diff-add-text, #10B981)",
                          }}
                        >
                          {file.basename} · #{file.turnIndex}
                        </button>
                      ))}
                    </div>
                  )}

                {visibleSteps.length === 0 ? (
                  <div style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                    {sinceLastPrompt.steps.length === 0
                      ? "Waiting for agent activity after this prompt…"
                      : `No ${sinceLastPromptCategory} recorded since this prompt.`}
                  </div>
                ) : (
                  <div
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      gap: "3px",
                    }}
                  >
                    {visibleSteps.map((step) => (
                      <button
                        key={step.id}
                        type="button"
                        data-testid={`since-last-prompt-step-${step.id}`}
                        onClick={() => scrollToTurnIndex(step.turnIndex)}
                        style={{
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "space-between",
                          gap: "8px",
                          background: "transparent",
                          border: "none",
                          padding: "2px 6px",
                          borderRadius: "4px",
                          color: "var(--text-secondary)",
                          fontSize: "11px",
                          cursor: "pointer",
                          textAlign: "left",
                          fontFamily: "inherit",
                        }}
                      >
                        <span
                          style={{
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                          }}
                        >
                          <span
                            className="fd-mono"
                            style={{
                              color:
                                step.kind === "tool"
                                  ? "var(--status-warn, #F59E0B)"
                                  : step.kind === "file"
                                    ? "var(--diff-add-text, #10B981)"
                                    : step.kind === "wire"
                                      ? "var(--accent-primary, #3B82F6)"
                                      : "var(--text-muted)",
                              marginRight: "6px",
                            }}
                          >
                            [{step.kind}]
                          </span>
                          {step.label}
                        </span>
                        <span
                          className="fd-mono"
                          style={{
                            fontSize: "10px",
                            color: "var(--text-muted)",
                            flexShrink: 0,
                          }}
                        >
                          #{step.turnIndex}
                        </span>
                      </button>
                    ))}
                  </div>
                )}
              </div>
            );
          })()}
        </div>
      )}

      {/* Transcript Stream */}
      <div
        ref={scrollContainerRef}
        onScroll={handleScroll}
        className="fd-scroll"
        style={{
          display: "flex",
          flexDirection: "column",
          gap: "12px",
        }}
      >
        {effectiveError && (
          <div
            role="alert"
            className="fd-card"
            style={{
              borderColor: "rgba(239, 68, 68, 0.45)",
              backgroundColor: "rgba(239, 68, 68, 0.12)",
              color: "#FCA5A5",
              fontSize: "12px",
            }}
          >
            {effectiveError}
          </div>
        )}

        {filteredTurns.length === 0 ? (
          <div
            className="fd-card"
            data-testid="chat-empty-state"
            style={{
              textAlign: "center",
              padding: "32px 20px",
              color: "var(--text-muted)",
              margin: "auto 0",
            }}
          >
            <div
              style={{
                fontWeight: 600,
                fontSize: "14px",
                color: "var(--text-primary)",
                marginBottom: "6px",
              }}
            >
              {effectiveTurns.length === 0
                ? `No transcript turns recorded for ${displayTarget}`
                : "No turns match the current filter"}
            </div>
            <div style={{ fontSize: "12px", maxWidth: "460px", margin: "0 auto" }}>
              {effectiveTurns.length === 0
                ? `Send a message below to dispatch directly to ${displayTarget}${
                    adapter ? ` (${adapter})` : ""
                  } over FerroWire and active session routes.`
                : "Clear the search filter above to view all session turns."}
            </div>
          </div>
        ) : (
          <>
            {hiddenTurnCount > 0 && (
              <div style={{ display: "flex", justifyContent: "center", padding: "4px 0" }}>
                <button
                  type="button"
                  className="fd-btn"
                  data-testid="load-earlier-turns-btn"
                  onClick={() => setVisibleTurnLimit((prev) => prev + 100)}
                  style={{ fontSize: "11px", padding: "4px 12px" }}
                >
                  Show earlier turns ({hiddenTurnCount} older)
                </button>
              </div>
            )}
            {groupConsecutiveWorkRuns(coalesceTurnsWithToolOutputs(displayedTurns)).map(
              (item, idx, arr) => {
                const isLastItem = idx === arr.length - 1;
                if (item.kind === "work-run") {
                  return (
                    <EncapsulatedWorkBlock
                      key={`work-run-${item.groups[0].turn.index}-${item.groups[item.groups.length - 1].turn.index}`}
                      groups={item.groups}
                      agentDisplayName={agentDisplayName ?? agentName ?? undefined}
                      isLastActive={effectiveIsBusy && isLastItem}
                      onOpenArtifact={onOpenArtifact}
                    />
                  );
                }
                const group = item.group;
                return (
                  <TurnCard
                    key={`${group.turn.index}-${group.turn.created_at ?? ""}`}
                    turn={group.turn}
                    attachedToolOutputs={group.attachedToolOutputs}
                    agentDisplayName={agentDisplayName ?? agentName ?? undefined}
                    isActive={
                      effectiveIsBusy &&
                      isLastItem &&
                      group.attachedToolOutputs.length === 0
                    }
                    onOpenArtifact={onOpenArtifact}
                  />
                );
              },
            )}
          </>
        )}

        {effectiveIsBusy &&
          !displayedTurns[displayedTurns.length - 1]?.thinking?.trim() && (
            <div data-testid="live-thinking-indicator">
              <ThinkingBlock
                thinking="Agent is reasoning and processing the current step..."
                isActive
              />
            </div>
          )}
      </div>

      {/* Unified Message Composer */}
      <div
        role="region"
        aria-label="Message Composer"
        data-testid="prompt-composer"
        className="fd-composer"
        onDragOver={(e) => e.preventDefault()}
        onDrop={(e) => {
          if (e.dataTransfer?.files?.length) {
            e.preventDefault();
            void readFilesAsAttachments(e.dataTransfer.files);
          }
        }}
        style={{
          borderTop: "1px solid var(--border-subtle)",
          backgroundColor: "var(--bg-surface)",
          padding: "10px 16px calc(18px + env(safe-area-inset-bottom, 0px))",
          display: "flex",
          flexDirection: "column",
          gap: "8px",
        }}
      >
        <div
          data-testid="composer-resize-handle"
          role="separator"
          aria-orientation="horizontal"
          aria-label="Resize message composer"
          title="Drag up or down to resize prompt composer"
          onMouseDown={handleComposerResizeStart}
          style={{
            height: "8px",
            margin: "-6px 0 2px 0",
            cursor: "ns-resize",
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            userSelect: "none",
          }}
        >
          <div
            style={{
              width: "36px",
              height: "3px",
              borderRadius: "2px",
              backgroundColor: "var(--border-subtle)",
            }}
          />
        </div>
        {submitError && (
          <div
            role="alert"
            style={{
              fontSize: "12px",
              color: "#FCA5A5",
            }}
          >
            {submitError}
          </div>
        )}

        {effectiveQueue.length > 0 && (
          <div
            data-testid="prompt-queue-bar"
            className="fd-card"
            style={{
              padding: "8px 10px",
              display: "flex",
              flexDirection: "column",
              gap: "6px",
              backgroundColor: "rgba(245, 158, 11, 0.08)",
              borderColor: "rgba(245, 158, 11, 0.3)",
            }}
          >
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                fontSize: "11px",
                fontWeight: 600,
                color: "#FBBF24",
              }}
            >
              <span>Queued Messages ({effectiveQueue.length})</span>
              <span style={{ fontWeight: 400, color: "var(--text-muted)" }}>
                Waiting for session to finish current turn
              </span>
            </div>
            <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
              {effectiveQueue.map((item, idx) => {
                const isEditing = editingQueueId === item.id;
                return (
                  <div
                    key={item.id}
                    data-testid={`queued-prompt-item-${item.id}`}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: "8px",
                      padding: "4px 8px",
                      borderRadius: "4px",
                      backgroundColor: "var(--bg-surface)",
                      border: "1px solid var(--border-subtle)",
                      fontSize: "12px",
                    }}
                  >
                    <span
                      className="fd-badge"
                      data-testid={`queued-prompt-pos-${idx}`}
                      style={{ fontSize: "11px", padding: "1px 6px" }}
                    >
                      #{idx + 1}
                    </span>
                    {isEditing ? (
                      <>
                        <input
                          type="text"
                          className="fd-input"
                          data-testid={`queued-edit-input-${item.id}`}
                          aria-label="Edit queued message"
                          value={editingQueueText}
                          onChange={(e) => setEditingQueueText(e.target.value)}
                          onKeyDown={(e) => {
                            if (e.key === "Enter") {
                              e.preventDefault();
                              void handleSaveEditedQueued(item.id);
                            } else if (e.key === "Escape") {
                              setEditingQueueId(null);
                              setEditingQueueText("");
                            }
                          }}
                          style={{
                            flex: 1,
                            height: "24px",
                            padding: "2px 8px",
                            fontSize: "12px",
                          }}
                        />
                        <button
                          type="button"
                          className="fd-btn fd-btn-primary"
                          data-testid={`queued-save-${item.id}`}
                          disabled={!editingQueueText.trim()}
                          onClick={() => void handleSaveEditedQueued(item.id)}
                          style={{ fontSize: "11px", padding: "2px 8px", height: "24px" }}
                        >
                          Save
                        </button>
                        <button
                          type="button"
                          className="fd-btn"
                          data-testid={`queued-edit-cancel-${item.id}`}
                          onClick={() => {
                            setEditingQueueId(null);
                            setEditingQueueText("");
                          }}
                          style={{ fontSize: "11px", padding: "2px 8px", height: "24px" }}
                        >
                          Cancel
                        </button>
                      </>
                    ) : (
                      <>
                        <span
                          style={{
                            flex: 1,
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                            color: "var(--text-primary)",
                          }}
                          title={item.prompt}
                        >
                          {item.prompt}
                        </span>
                        {item.attachments && item.attachments.length > 0 && (
                          <span className="fd-badge" style={{ fontSize: "10px" }}>
                            {item.attachments.length} file{item.attachments.length === 1 ? "" : "s"}
                          </span>
                        )}
                        <button
                          type="button"
                          className="fd-btn"
                          data-testid={`queued-edit-${item.id}`}
                          onClick={() => {
                            setEditingQueueId(item.id);
                            setEditingQueueText(item.prompt);
                          }}
                          style={{ fontSize: "11px", padding: "2px 8px", height: "24px" }}
                          title="Edit queued message"
                        >
                          Edit
                        </button>
                        <button
                          type="button"
                          className="fd-btn fd-btn-primary"
                          data-testid={`queued-send-now-${item.id}`}
                          onClick={() => void handleSendQueuedNow(item.id)}
                          style={{ fontSize: "11px", padding: "2px 8px", height: "24px" }}
                        >
                          Send Now
                        </button>
                        <button
                          type="button"
                          className="fd-btn"
                          aria-label="Remove"
                          data-testid={`queued-cancel-${item.id}`}
                          onClick={() => void handleCancelQueued(item.id)}
                          style={{ fontSize: "11px", padding: "2px 6px", height: "24px" }}
                          title="Remove from queue"
                        >
                          ✕
                        </button>
                      </>
                    )}
                  </div>
                );
              })}
            </div>
          </div>
        )}

        {showSlashMenu && (
          <div
            role="listbox"
            aria-label="Slash Commands"
            data-testid="slash-command-menu"
            className="fd-card"
            style={{
              padding: "6px",
              display: "flex",
              flexDirection: "column",
              gap: "2px",
              maxHeight: "220px",
              overflowY: "auto",
              backgroundColor: "var(--bg-elevated)",
              borderColor: "var(--border-accent)",
            }}
          >
            {matchingSlashCommands.map((cmd, idx) => {
              const cleanName = cmd.name.replace(/^\/+/, "");
              const isSelected = idx === slashActiveIndex;
              return (
                <button
                  key={cleanName}
                  type="button"
                  role="option"
                  aria-selected={isSelected}
                  data-testid={`slash-command-item-${cleanName}`}
                  onClick={() => handleSelectSlashCommand(cmd)}
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "10px",
                    width: "100%",
                    textAlign: "left",
                    padding: "6px 10px",
                    borderRadius: "6px",
                    border: "none",
                    cursor: "pointer",
                    backgroundColor: isSelected
                      ? "var(--accent-subtle)"
                      : "transparent",
                    color: "var(--text-primary)",
                    fontSize: "12px",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                    <span className="fd-mono" style={{ fontWeight: 600 }}>
                      /{cleanName}
                    </span>
                    <span
                      style={{
                        color: "var(--text-secondary)",
                        fontSize: "11.5px",
                      }}
                    >
                      {cmd.description}
                    </span>
                  </div>
                  <span
                    className="fd-badge"
                    style={{ fontSize: "10px", textTransform: "uppercase" }}
                  >
                    {cmd.kind}
                  </span>
                </button>
              );
            })}
          </div>
        )}

        <div
          data-testid="composer-card"
          style={{
            borderRadius: "16px",
            border: "1px solid var(--border-subtle)",
            backgroundColor: "var(--bg-surface)",
            padding: "10px 12px",
            display: "flex",
            flexDirection: "column",
            gap: "8px",
          }}
        >
          {attachments.length > 0 && (
            <div
              data-testid="attachment-chip-list"
              style={{
                display: "flex",
                flexWrap: "wrap",
                gap: "6px",
                alignItems: "center",
              }}
            >
              {attachments.map((att, idx) => (
                <div
                  key={`${att.name}-${idx}`}
                  className="fd-badge"
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    padding: "4px 8px",
                  }}
                >
                  {att.media_type.startsWith("image/") && (
                    <img
                      src={`data:${att.media_type};base64,${att.data_base64}`}
                      alt={att.name}
                      style={{
                        width: "20px",
                        height: "20px",
                        objectFit: "cover",
                        borderRadius: "2px",
                      }}
                    />
                  )}
                  <span>{att.name}</span>
                  <button
                    type="button"
                    data-testid={`remove-attachment-${idx}`}
                    aria-label={`Remove ${att.name}`}
                    onClick={() =>
                      setAttachments((prev) => prev.filter((_, i) => i !== idx))
                    }
                    style={{
                      background: "none",
                      border: "none",
                      color: "var(--text-muted)",
                      cursor: "pointer",
                      padding: 0,
                      fontSize: "11px",
                    }}
                  >
                    ✕
                  </button>
                </div>
              ))}
            </div>
          )}

          <textarea
            ref={textareaRef}
            rows={2}
            role="searchbox"
            name="fd-dispatch-command"
            autoComplete="one-time-code"
            autoCorrect="off"
            autoCapitalize="sentences"
            spellCheck={true}
            enterKeyHint="send"
            data-1p-ignore="true"
            data-lpignore="true"
            data-bwignore="true"
            data-form-type="other"
            aria-autocomplete="none"
            aria-label="Message"
            placeholder={`Send a message to ${displayTarget}... (Ctrl+Enter to submit, paste or attach files)`}
            className="fd-textarea fd-prompt-input"
            value={promptText}
            onChange={(e) => setPromptText(e.target.value)}
            onKeyDown={handleKeyDown}
            onPaste={handlePaste}
            style={{
              width: "100%",
              resize: "vertical",
              height: `${composerHeight}px`,
              minHeight: "72px",
              maxHeight: "420px",
              border: "none",
              outline: "none",
              backgroundColor: "transparent",
              padding: "2px 0",
              fontFamily: "var(--font-sans)",
            }}
          />

          <div
            data-testid="composer-action-bar"
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "8px",
              flexWrap: "wrap",
              paddingTop: "6px",
              borderTop: "1px solid var(--border-subtle)",
            }}
          >
            <div style={{ display: "flex", alignItems: "center", gap: "10px", flexWrap: "wrap" }}>
              <input
                ref={fileInputRef}
                type="file"
                accept="image/*,.png,.jpg,.jpeg,.webp,.gif,.pdf,.txt,.md,.json,.log"
                multiple
                style={{ display: "none" }}
                data-testid="prompt-file-input"
                onChange={handleFileChange}
              />
              <button
                type="button"
                className="fd-btn"
                data-testid="prompt-attach-btn"
                onClick={() => fileInputRef.current?.click()}
                style={{ fontSize: "12px", padding: "5px 10px" }}
              >
                + Attach
              </button>

              <label
                style={{
                  display: "inline-flex",
                  alignItems: "center",
                  gap: "6px",
                  fontSize: "12px",
                  color: "var(--text-muted)",
                  cursor: "pointer",
                }}
              >
                <input
                  type="checkbox"
                  aria-label="Always send now"
                  data-testid="always-send-now-toggle"
                  checked={alwaysSendNow}
                  onChange={(e) => handleToggleAlwaysSendNow(e.target.checked)}
                />
                <span>Always send now</span>
              </label>
            </div>

            <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
              {interruptArmed ? (
                <div style={{ display: "inline-flex", alignItems: "center", gap: "4px" }}>
                  <button
                    type="button"
                    className="fd-btn fd-btn-danger"
                    data-testid="interrupt-confirm-btn"
                    disabled={interrupting}
                    onClick={() => void handleConfirmInterrupt()}
                    style={{ height: "34px", padding: "0 12px", whiteSpace: "nowrap" }}
                  >
                    {interrupting ? "Stopping..." : "Confirm Stop"}
                  </button>
                  <button
                    type="button"
                    className="fd-btn"
                    data-testid="interrupt-cancel-btn"
                    disabled={interrupting}
                    onClick={disarmInterrupt}
                    style={{ height: "34px", padding: "0 10px", whiteSpace: "nowrap" }}
                  >
                    Cancel
                  </button>
                </div>
              ) : (
                <button
                  type="button"
                  className="fd-btn fd-btn-danger"
                  data-testid="interrupt-session-btn"
                  disabled={!canInterrupt}
                  title="Stop active session execution (requires confirmation click)"
                  onClick={handleArmInterrupt}
                  style={{ height: "34px", padding: "0 12px", whiteSpace: "nowrap" }}
                >
                  {interrupting ? "Stopping..." : "Stop"}
                </button>
              )}

              <button
                type="button"
                className="fd-btn fd-btn-primary"
                data-testid="send-prompt-btn"
                disabled={!canSend}
                onClick={() => void submitPrompt()}
                style={{ height: "34px", padding: "0 16px" }}
              >
                {submitting ? "Sending..." : "Send Message"}
              </button>
            </div>
          </div>
        </div>
      </div>

      {clearModalOpen && (
        <div
          className="fd-modal-backdrop"
          onClick={handleCloseClearModal}
        >
          <div
            role="dialog"
            aria-modal="true"
            aria-labelledby="clear-session-modal-title"
            data-testid="clear-session-modal"
            className="fd-modal"
            onClick={(e) => e.stopPropagation()}
            style={{ position: "relative", maxWidth: "420px" }}
          >
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "8px",
                marginBottom: "10px",
              }}
            >
              <h3
                id="clear-session-modal-title"
                style={{
                  margin: 0,
                  fontSize: "15px",
                  fontWeight: 600,
                  color: "var(--text-primary)",
                }}
              >
                Clear Session Context
              </h3>
              <button
                type="button"
                data-testid="clear-modal-help-btn"
                aria-label="Help"
                title="Explain Clear vs. Reforge"
                className="fd-btn fd-help-icon-btn"
                onClick={handleShowClearHelp}
              >
                ?
              </button>
            </div>

            {clearHelpVisible && (
              <div
                role="note"
                data-testid="clear-modal-help-popover"
                className="fd-ephemeral-help-popover"
              >
                <div style={{ marginBottom: "6px" }}>
                  <strong style={{ color: "var(--text-primary)" }}>Clear:</strong>{" "}
                  Clears the agent&apos;s conversation context and leaves the session blank.
                </div>
                <div>
                  <strong style={{ color: "var(--accent-hover)" }}>Reforge:</strong>{" "}
                  Clears the agent&apos;s context and immediately re-primes the agent with its
                  registered identity, project, and role prompt.
                </div>
                <div style={{ marginTop: "8px" }}>
                  <button
                    type="button"
                    data-testid="open-full-help-btn"
                    className="fd-btn"
                    style={{ fontSize: "11px", padding: "3px 8px" }}
                    onClick={() => {
                      onOpenHelpTopic?.("lifecycle");
                      dispatchOpenHelp("lifecycle");
                    }}
                  >
                    Open Full Documentation
                  </button>
                </div>
              </div>
            )}

            <p
              style={{
                margin: "0 0 16px 0",
                fontSize: "13px",
                color: "var(--text-secondary)",
                lineHeight: 1.5,
              }}
            >
              Choose how to reset context for{" "}
              <strong style={{ color: "var(--text-primary)" }}>{displayTarget}</strong>:
            </p>

            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "flex-end",
                gap: "8px",
              }}
            >
              <button
                type="button"
                className="fd-btn"
                data-testid="clear-modal-cancel-btn"
                disabled={clearing}
                onClick={handleCloseClearModal}
              >
                Cancel
              </button>
              <button
                type="button"
                className="fd-btn fd-btn-danger"
                data-testid="clear-modal-clear-btn"
                disabled={clearing}
                onClick={() => void handleClearOrReforge("clear")}
              >
                Clear
              </button>
              <button
                type="button"
                className="fd-btn fd-btn-primary"
                data-testid="clear-modal-reforge-btn"
                disabled={clearing}
                onClick={() => void handleClearOrReforge("reforge")}
              >
                Reforge
              </button>
            </div>
          </div>
        </div>
      )}

      {decommissionModalOpen && (
        <div
          className="fd-modal-backdrop"
          onClick={handleCloseDecommissionModal}
        >
          <div
            role="dialog"
            aria-modal="true"
            aria-labelledby="decommission-session-modal-title"
            data-testid="decommission-modal"
            className="fd-modal"
            onClick={(e) => e.stopPropagation()}
            style={{ position: "relative", maxWidth: "460px" }}
          >
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "8px",
                marginBottom: "10px",
              }}
            >
              <h3
                id="decommission-session-modal-title"
                style={{
                  margin: 0,
                  fontSize: "15px",
                  fontWeight: 600,
                  color: "var(--text-primary)",
                }}
              >
                Decommission Agent Session
              </h3>
              <button
                type="button"
                data-testid="decommission-help-btn"
                aria-label="Help"
                title="Explain Prepare & Decommission vs. Tear Down"
                className="fd-btn fd-help-icon-btn"
                onClick={handleShowDecommissionHelp}
              >
                ?
              </button>
            </div>

            {decommissionHelpVisible && (
              <div
                role="note"
                data-testid="decommission-help-popover"
                className="fd-ephemeral-help-popover"
              >
                <div style={{ marginBottom: "6px" }}>
                  <strong style={{ color: "var(--accent-hover)" }}>
                    Prepare &amp; Decommission:
                  </strong>{" "}
                  Sends a pre-decommission state and worktree hygiene handshake prompt so the
                  agent saves STATE.md/FerroState, cleans up worktrees, and messages its Lead.
                </div>
                <div>
                  <strong style={{ color: "var(--text-primary)" }}>Tear Down:</strong>{" "}
                  Immediately stops the runtime session, clears queues and routes, and
                  unregisters the persona.
                </div>
              </div>
            )}

            <p
              style={{
                margin: "0 0 16px 0",
                fontSize: "13px",
                color: "var(--text-secondary)",
                lineHeight: 1.5,
              }}
            >
              Choose how to decommission{" "}
              <strong style={{ color: "var(--text-primary)" }}>{displayTarget}</strong>:
            </p>

            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "flex-end",
                gap: "8px",
                flexWrap: "wrap",
              }}
            >
              <button
                type="button"
                className="fd-btn"
                data-testid="decommission-cancel-btn"
                disabled={decommissioning}
                onClick={handleCloseDecommissionModal}
              >
                Cancel
              </button>
              <button
                type="button"
                className="fd-btn fd-btn-danger"
                data-testid="decommission-teardown-btn"
                disabled={decommissioning}
                onClick={() => void handleDecommission("teardown")}
              >
                Tear Down
              </button>
              <button
                type="button"
                className="fd-btn fd-btn-primary"
                data-testid="decommission-prepare-btn"
                disabled={decommissioning}
                onClick={() => void handleDecommission("prepare")}
              >
                Prepare &amp; Decommission
              </button>
            </div>
          </div>
        </div>
      )}

      <AgentSettingsModal
        isOpen={effectiveAgentSettingsOpen}
        onClose={handleCloseAgentSettings}
        agentName={agentName}
        agentDisplayName={agentDisplayName}
        agentRole={agentRole}
        agentProject={agentProject}
        adapter={adapter}
        sessionId={sessionId}
        selectedModel={selectedModel}
        selectedThinkingLevel={selectedThinkingLevel}
        availableModels={availableModels}
        switchingModel={switchingModel}
        onChangeModelAndThinking={(nextModel, nextThinking) =>
          void handleChangeModelAndThinking(nextModel, nextThinking)
        }
        contextEstimate={contextEstimate}
        quotaUsage={effectiveQuotaUsage}
        turnCount={filteredTurns.length}
        canCompact={canCompact}
        compacting={compacting}
        onCompactContext={() => void handleCompactContext()}
        canClear={canClear}
        clearing={clearing}
        onOpenClearModal={handleOpenClearModal}
        canDecommission={canDecommission}
        decommissioning={decommissioning}
        onOpenDecommissionModal={handleOpenDecommissionModal}
        apiClient={apiClient}
        onAgentsUpdated={onRefresh}
      />
    </section>
  );
};
