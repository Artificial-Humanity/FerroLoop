import React from "react";
import type { HubApiClient } from "../api/client";
import type {
  PromptAttachment,
  PromptRequest,
  PromptResponse,
  QueuedOperatorPrompt,
  SessionContextUsage,
  TranscriptTurn,
} from "../types/api";
import { ThinkingBlock } from "./ThinkingBlock";
import { TurnCard } from "./TurnCard";

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
    normalized === "gemini"
  ) {
    return 1_000_000;
  }
  if (normalized === "grok") {
    return 131_072;
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
  adapter?: string | null;
  sessionId?: string | null;
  turns?: TranscriptTurn[];
  /** Server-measured context usage; estimates apply when absent. */
  contextUsage?: SessionContextUsage | null;
  isBusy?: boolean;
  queue?: QueuedOperatorPrompt[];
  loading?: boolean;
  error?: string | null;
  wsConnected?: boolean;
  onRefresh?: () => void;
  onSendPrompt?: (
    targetSessionOrAgentId: string,
    payload: PromptRequest,
  ) => Promise<unknown> | void;
  onSendQueuedNow?: (promptId: string) => Promise<unknown> | void;
  onCancelQueued?: (promptId: string) => Promise<unknown> | void;
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
  adapter,
  sessionId,
  turns,
  contextUsage = null,
  isBusy,
  queue,
  loading = false,
  error = null,
  wsConnected = false,
  onRefresh,
  onSendPrompt,
  onSendQueuedNow,
  onCancelQueued,
  apiClient,
}) => {
  const [internalTurns, setInternalTurns] = React.useState<TranscriptTurn[]>([]);
  const [internalIsBusy, setInternalIsBusy] = React.useState<boolean>(false);
  const [internalQueue, setInternalQueue] = React.useState<QueuedOperatorPrompt[]>([]);
  const [internalLoading, setInternalLoading] = React.useState<boolean>(false);
  const [internalError, setInternalError] = React.useState<string | null>(null);
  const [internalContextUsage, setInternalContextUsage] =
    React.useState<SessionContextUsage | null>(null);
  const [compacting, setCompacting] = React.useState<boolean>(false);
  const [compactFeedback, setCompactFeedback] = React.useState<string | null>(null);

  const [filterQuery, setFilterQuery] = React.useState<string>("");
  const [visibleTurnLimit, setVisibleTurnLimit] = React.useState<number>(100);
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
  const [submitting, setSubmitting] = React.useState<boolean>(false);
  const [submitError, setSubmitError] = React.useState<string | null>(null);

  const scrollContainerRef = React.useRef<HTMLDivElement | null>(null);
  const fileInputRef = React.useRef<HTMLInputElement | null>(null);
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
  const canCompact =
    contextEstimate.percent >= 50 &&
    Boolean(targetSessionOrAgent) &&
    !compacting &&
    !submitting;

  const handleCompactContext = async () => {
    if (!canCompact) {
      return;
    }
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
        await onSendPrompt(targetSessionOrAgent, payload);
      } else if (apiClient) {
        const res = await apiClient.sendSessionPrompt(targetSessionOrAgent, payload);
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
      } else {
        throw new Error("No prompt route is configured for this session.");
      }
      setCompactFeedback("Sent /compact");
      scrollToBottom("smooth");
      requestAnimationFrame(() => scrollToBottom("smooth"));
    } catch (err) {
      setSubmitError(err instanceof Error ? err.message : "Failed to send /compact");
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
      setInternalTurns(response.turns ?? []);
      setInternalIsBusy(Boolean(response.is_busy));
      setInternalQueue(response.queue ?? []);
      setInternalContextUsage(response.context_usage ?? null);
    } catch (err) {
      setInternalError(err instanceof Error ? err.message : "Failed to load transcript");
    } finally {
      setInternalLoading(false);
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
  }, [targetSessionOrAgent]);

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
    try {
      if (onSendQueuedNow) {
        await onSendQueuedNow(promptId);
      } else if (apiClient) {
        const res = await apiClient.sendQueuedPromptNow(targetSessionOrAgent, promptId);
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
      scrollToBottom("smooth");
    } catch (err) {
      setSubmitError(err instanceof Error ? err.message : "Failed to send queued prompt");
    }
  };

  const handleCancelQueued = async (promptId: string) => {
    if (!targetSessionOrAgent) return;
    try {
      if (onCancelQueued) {
        await onCancelQueued(promptId);
      } else if (apiClient) {
        const res = await apiClient.cancelQueuedPrompt(targetSessionOrAgent, promptId);
        if (Array.isArray(res.queue)) {
          setInternalQueue(res.queue);
        }
        if (typeof res.is_busy === "boolean") {
          setInternalIsBusy(res.is_busy);
        }
      }
    } catch (err) {
      setSubmitError(err instanceof Error ? err.message : "Failed to cancel queued prompt");
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

    setSubmitting(true);
    setSubmitError(null);
    try {
      let wasQueued = false;
      if (onSendPrompt) {
        const res = (await onSendPrompt(targetSessionOrAgent, payload)) as
          | PromptResponse
          | undefined;
        if (res && (res.queued || res.status === "queued")) {
          wasQueued = true;
        }
      } else if (apiClient) {
        const res = await apiClient.sendSessionPrompt(targetSessionOrAgent, payload);
        if (Array.isArray(res.queue)) {
          setInternalQueue(res.queue);
        }
        if (typeof res.is_busy === "boolean") {
          setInternalIsBusy(res.is_busy);
        }
        if (res.queued || res.status === "queued") {
          wasQueued = true;
        } else {
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
      clearSessionDraft(targetSessionOrAgent);
      setPromptText("");
      setAttachments([]);
      if (!wasQueued) {
        scrollToBottom("smooth");
        // Run again after the optimistic turn and live indicator render.
        requestAnimationFrame(() => scrollToBottom("smooth"));
      }
    } catch (err) {
      setSubmitError(err instanceof Error ? err.message : "Failed to send prompt");
    } finally {
      setSubmitting(false);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      void submitPrompt();
    }
  };

  return (
    <section className="fd-pane" data-testid="chat-pane">
      {/* Toolbar */}
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
        <div style={{ display: "flex", alignItems: "center", gap: "8px", flex: 1 }}>
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
            className="fd-input"
            placeholder="Filter transcript turns, tools, or reasoning..."
            aria-label="Filter transcript"
            value={filterQuery}
            onChange={(e) => setFilterQuery(e.target.value)}
            style={{ width: "100%", maxWidth: "320px", fontSize: "12px", padding: "5px 9px" }}
          />
          <span className="fd-badge" data-testid="turn-count-badge">
            {filteredTurns.length} {filteredTurns.length === 1 ? "turn" : "turns"}
          </span>
          <span
            className="fd-badge"
            data-testid="session-busy-badge"
            style={{
              backgroundColor: effectiveIsBusy
                ? "rgba(245, 158, 11, 0.16)"
                : "rgba(16, 185, 129, 0.14)",
              color: effectiveIsBusy ? "#FBBF24" : "#34D399",
              borderColor: effectiveIsBusy
                ? "rgba(245, 158, 11, 0.35)"
                : "rgba(16, 185, 129, 0.3)",
            }}
          >
            {effectiveIsBusy ? "Busy · Working" : "Idle"}
          </span>
          <div
            data-testid="context-level-meter"
            style={{
              display: "flex",
              alignItems: "center",
              gap: "8px",
              minWidth: "190px",
            }}
          >
            <span className="fd-mono" style={{ fontSize: "11px", color: "var(--text-secondary)", whiteSpace: "nowrap" }}>
              Context: {formatTokenCount(contextEstimate.usedTokens)} /{" "}
              {formatTokenCount(contextEstimate.maxTokens)} ({contextEstimate.percent}%)
            </span>
            <div
              role="progressbar"
              aria-label="Context level"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={contextEstimate.percent}
              style={{
                width: "64px",
                height: "6px",
                borderRadius: "3px",
                backgroundColor: "var(--bg-input)",
                border: "1px solid var(--border-subtle)",
                overflow: "hidden",
                flexShrink: 0,
              }}
            >
              <div
                style={{
                  width: `${contextEstimate.percent}%`,
                  height: "100%",
                  backgroundColor:
                    contextEstimate.percent >= 80
                      ? "var(--status-danger)"
                      : contextEstimate.percent >= 50
                        ? "var(--status-stale)"
                        : "var(--status-alive)",
                }}
              />
            </div>
          </div>
          <button
            type="button"
            className={`fd-btn${canCompact ? " fd-btn-primary" : ""}`}
            data-testid="compact-context-btn"
            disabled={!canCompact}
            title={
              contextEstimate.percent >= 50
                ? "Snapshot a FerroState checkpoint, then send /compact to the session"
                : "Available when context reaches 50%"
            }
            onClick={() => void handleCompactContext()}
            style={{ whiteSpace: "nowrap" }}
          >
            {compacting ? "Compacting..." : "Compact Context"}
          </button>
          {compactFeedback && (
            <span
              role="status"
              data-testid="compact-feedback"
              className="fd-badge"
              style={{ color: "var(--diff-add-text)" }}
            >
              {compactFeedback}
            </span>
          )}
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
          {(onRefresh || apiClient) && (
            <button
              type="button"
              className="fd-btn"
              onClick={() => {
                if (onRefresh) {
                  onRefresh();
                } else {
                  void fetchTranscript();
                }
              }}
              disabled={effectiveLoading}
            >
              {effectiveLoading ? "Syncing..." : "Refresh"}
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
        </div>
      </div>

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
                ? `Send a prompt below to dispatch directly to ${displayTarget}${
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
            {displayedTurns.map((turn, idx) => (
              <TurnCard
                key={`${turn.index}-${turn.created_at ?? ""}`}
                turn={turn}
                agentDisplayName={agentDisplayName ?? agentName ?? undefined}
                isActive={effectiveIsBusy && idx === displayedTurns.length - 1}
              />
            ))}
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

      {/* Unified Prompt Composer */}
      <div
        role="region"
        aria-label="Prompt Composer"
        data-testid="prompt-composer"
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
              <span>Queued Prompts ({effectiveQueue.length})</span>
              <span style={{ fontWeight: 400, color: "var(--text-muted)" }}>
                Waiting for session to finish current turn
              </span>
            </div>
            <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
              {effectiveQueue.map((item, idx) => (
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
                </div>
              ))}
            </div>
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
            aria-label="Prompt Message"
            placeholder={`Send a prompt to ${displayTarget}... (Ctrl+Enter to send)`}
            className="fd-textarea"
            value={promptText}
            onChange={(e) => setPromptText(e.target.value)}
            onKeyDown={handleKeyDown}
            onPaste={handlePaste}
            style={{
              width: "100%",
              resize: "vertical",
              minHeight: "56px",
              maxHeight: "180px",
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

            <button
              type="button"
              className="fd-btn fd-btn-primary"
              disabled={!canSend}
              onClick={() => void submitPrompt()}
              style={{ height: "34px", padding: "0 16px" }}
            >
              {submitting ? "Sending..." : "Send Prompt"}
            </button>
          </div>
        </div>
      </div>
    </section>
  );
};
