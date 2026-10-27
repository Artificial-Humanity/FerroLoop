import React from "react";
import type { HubApiClient } from "../api/client";
import type { PromptAttachment, PromptRequest, TranscriptTurn } from "../types/api";
import { TurnCard } from "./TurnCard";

export interface SessionScrollState {
  scrollTop: number;
  pinnedToBottom: boolean;
}

export const sessionScrollMemory = new Map<string, SessionScrollState>();

export interface ChatPaneProps {
  agentName?: string | null;
  agentDisplayName?: string | null;
  adapter?: string | null;
  sessionId?: string | null;
  turns?: TranscriptTurn[];
  loading?: boolean;
  error?: string | null;
  wsConnected?: boolean;
  onRefresh?: () => void;
  onSendPrompt?: (
    targetSessionOrAgentId: string,
    payload: PromptRequest,
  ) => Promise<unknown> | void;
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
  loading = false,
  error = null,
  wsConnected = false,
  onRefresh,
  onSendPrompt,
  apiClient,
}) => {
  const [internalTurns, setInternalTurns] = React.useState<TranscriptTurn[]>([]);
  const [internalLoading, setInternalLoading] = React.useState<boolean>(false);
  const [internalError, setInternalError] = React.useState<string | null>(null);

  const [filterQuery, setFilterQuery] = React.useState<string>("");
  const [promptText, setPromptText] = React.useState<string>("");
  const [urgency, setUrgency] = React.useState<"normal" | "urgent">("normal");
  const [subject, setSubject] = React.useState<string>("");
  const [attachments, setAttachments] = React.useState<PromptAttachment[]>([]);
  const [submitting, setSubmitting] = React.useState<boolean>(false);
  const [submitError, setSubmitError] = React.useState<string | null>(null);

  const scrollContainerRef = React.useRef<HTMLDivElement | null>(null);
  const fileInputRef = React.useRef<HTMLInputElement | null>(null);
  const prevTargetRef = React.useRef<string>("");
  const prevWsConnectedRef = React.useRef<boolean>(Boolean(wsConnected));
  const prevTurnCountRef = React.useRef<number>(0);
  const isNearBottomRef = React.useRef<boolean>(true);

  const effectiveTurns = turns ?? internalTurns;
  const effectiveLoading = loading || internalLoading;
  const effectiveError = error ?? internalError;

  const targetSessionOrAgent = sessionId || agentName || "";
  const displayTarget = agentDisplayName || agentName || sessionId || "Agent";

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

  React.useEffect(() => {
    const targetChanged = prevTargetRef.current !== targetSessionOrAgent;
    const reconnected = !prevWsConnectedRef.current && Boolean(wsConnected);
    const initialLoad = prevTurnCountRef.current === 0 && filteredTurns.length > 0;
    const newTurnsArrived = filteredTurns.length > prevTurnCountRef.current;

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
  }, [
    targetSessionOrAgent,
    filteredTurns.length,
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

  const canSend =
    (promptText.trim().length > 0 || attachments.length > 0) &&
    !submitting &&
    Boolean(targetSessionOrAgent);

  const submitPrompt = async () => {
    const trimmedPrompt = promptText.trim();
    if ((!trimmedPrompt && attachments.length === 0) || submitting || !targetSessionOrAgent) {
      return;
    }

    const trimmedSubject = subject.trim();
    const payload: PromptRequest = {
      prompt: trimmedPrompt || "Attached file(s).",
      urgency,
      subject: trimmedSubject.length > 0 ? trimmedSubject : undefined,
      ...(attachments.length > 0 ? { attachments } : {}),
    };

    setSubmitting(true);
    setSubmitError(null);
    try {
      if (onSendPrompt) {
        await onSendPrompt(targetSessionOrAgent, payload);
      } else if (apiClient) {
        await apiClient.sendSessionPrompt(targetSessionOrAgent, payload);
        await fetchTranscript();
      }
      setPromptText("");
      setSubject("");
      setAttachments([]);
      scrollToBottom("smooth");
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
          filteredTurns.map((turn) => (
            <TurnCard
              key={`${turn.index}-${turn.created_at ?? ""}`}
              turn={turn}
              agentDisplayName={agentDisplayName ?? agentName ?? undefined}
            />
          ))
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
          padding: "12px 16px",
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

        <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
          <label
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: "6px",
              fontSize: "12px",
              color: "var(--text-muted)",
            }}
          >
            <span>Urgency</span>
            <select
              aria-label="Urgency"
              className="fd-select"
              value={urgency}
              onChange={(e) => setUrgency(e.target.value as "normal" | "urgent")}
              style={{ fontSize: "12px", padding: "4px 8px" }}
            >
              <option value="normal">Normal</option>
              <option value="urgent">Urgent</option>
            </select>
          </label>

          <input
            type="text"
            name="fd-dispatch-tag"
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            data-1p-ignore="true"
            data-lpignore="true"
            data-form-type="other"
            aria-label="Prompt Subject"
            placeholder="Optional subject (defaults to cockpit prompt)"
            className="fd-input"
            value={subject}
            onChange={(e) => setSubject(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                void submitPrompt();
              }
            }}
            style={{ flex: 1, minWidth: "180px", fontSize: "12px", padding: "5px 9px" }}
          />

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
        </div>

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

        <div style={{ display: "flex", gap: "8px", alignItems: "flex-end" }}>
          <textarea
            rows={2}
            name="fd-dispatch-command"
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="sentences"
            spellCheck={false}
            enterKeyHint="send"
            data-1p-ignore="true"
            data-lpignore="true"
            data-form-type="other"
            aria-label="Prompt Message"
            placeholder={`Send a prompt to ${displayTarget}... (Ctrl+Enter to send)`}
            className="fd-textarea"
            value={promptText}
            onChange={(e) => setPromptText(e.target.value)}
            onKeyDown={handleKeyDown}
            onPaste={handlePaste}
            style={{
              flex: 1,
              resize: "vertical",
              minHeight: "54px",
              maxHeight: "180px",
              fontFamily: "var(--font-sans)",
            }}
          />

          <button
            type="button"
            className="fd-btn fd-btn-primary"
            disabled={!canSend}
            onClick={() => void submitPrompt()}
            style={{ height: "38px", padding: "0 16px" }}
          >
            {submitting ? "Sending..." : "Send Prompt"}
          </button>
        </div>
      </div>
    </section>
  );
};
