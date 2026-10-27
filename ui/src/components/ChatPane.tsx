import React from "react";
import type { HubApiClient } from "../api/client";
import type { PromptRequest, TranscriptTurn } from "../types/api";
import { TurnCard } from "./TurnCard";

export interface ChatPaneProps {
  agentName?: string | null;
  agentDisplayName?: string | null;
  adapter?: string | null;
  sessionId?: string | null;
  turns?: TranscriptTurn[];
  loading?: boolean;
  error?: string | null;
  onRefresh?: () => void;
  onSendPrompt?: (
    targetSessionOrAgentId: string,
    payload: PromptRequest,
  ) => Promise<unknown> | void;
  apiClient?: HubApiClient;
}

export const ChatPane: React.FC<ChatPaneProps> = ({
  agentName,
  agentDisplayName,
  adapter,
  sessionId,
  turns,
  loading = false,
  error = null,
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
  const [submitting, setSubmitting] = React.useState<boolean>(false);
  const [submitError, setSubmitError] = React.useState<string | null>(null);

  const scrollContainerRef = React.useRef<HTMLDivElement | null>(null);

  const effectiveTurns = turns ?? internalTurns;
  const effectiveLoading = loading || internalLoading;
  const effectiveError = error ?? internalError;

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

  const handleJumpToBottom = () => {
    const el = scrollContainerRef.current;
    if (el && typeof el.scrollTo === "function") {
      el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
    }
  };

  const targetSessionOrAgent = sessionId || agentName || "";
  const displayTarget = agentDisplayName || agentName || sessionId || "Agent";

  const submitPrompt = async () => {
    const trimmedPrompt = promptText.trim();
    if (!trimmedPrompt || submitting || !targetSessionOrAgent) {
      return;
    }

    const trimmedSubject = subject.trim();
    const payload: PromptRequest = {
      prompt: trimmedPrompt,
      urgency,
      subject: trimmedSubject.length > 0 ? trimmedSubject : undefined,
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

  const handleFormSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    void submitPrompt();
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
      <form
        onSubmit={handleFormSubmit}
        data-testid="prompt-composer"
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
            aria-label="Prompt Subject"
            placeholder="Optional subject (defaults to cockpit prompt)"
            className="fd-input"
            value={subject}
            onChange={(e) => setSubject(e.target.value)}
            style={{ flex: 1, minWidth: "180px", fontSize: "12px", padding: "5px 9px" }}
          />
        </div>

        <div style={{ display: "flex", gap: "8px", alignItems: "flex-end" }}>
          <textarea
            rows={2}
            aria-label="Prompt Message"
            placeholder={`Send a prompt to ${displayTarget}... (Ctrl+Enter to send)`}
            className="fd-textarea"
            value={promptText}
            onChange={(e) => setPromptText(e.target.value)}
            onKeyDown={handleKeyDown}
            style={{
              flex: 1,
              resize: "vertical",
              minHeight: "54px",
              maxHeight: "180px",
              fontFamily: "var(--font-sans)",
            }}
          />

          <button
            type="submit"
            className="fd-btn fd-btn-primary"
            disabled={submitting || !promptText.trim() || !targetSessionOrAgent}
            style={{ height: "38px", padding: "0 16px" }}
          >
            {submitting ? "Sending..." : "Send Prompt"}
          </button>
        </div>
      </form>
    </section>
  );
};
