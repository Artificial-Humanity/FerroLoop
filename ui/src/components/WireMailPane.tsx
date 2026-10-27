import React, { useMemo, useState } from "react";
import type { HubApiClient } from "../api/client";
import {
  DEFAULT_WORKSPACE_AGENTS,
  type AgentRecord,
  type WireInboxItem,
  type WireReceipt,
  type WireSendRequest,
} from "../types/api";

export interface WireMailPaneProps {
  messages?: WireInboxItem[];
  selectedAgentName?: string | null;
  agents?: AgentRecord[];
  loading?: boolean;
  error?: string | null;
  onRefresh?: () => void;
  onAckMessage?: (messageId: string, byAgent?: string) => Promise<unknown> | void;
  onSendMessage?: (payload: WireSendRequest) => Promise<unknown> | void;
  apiClient?: HubApiClient;
}

export type RecipientScope = "selected" | "all";

function resolvePrimaryReceipt(
  item: WireInboxItem,
  selectedAgentName?: string | null,
): WireReceipt | null {
  if (item.receipt) {
    return item.receipt;
  }
  const receipts = item.receipts ?? [];
  if (receipts.length === 0) {
    return null;
  }
  if (selectedAgentName) {
    const key = selectedAgentName.trim().toLowerCase();
    const match = receipts.find((r) => r.recipient_name.trim().toLowerCase() === key);
    if (match) {
      return match;
    }
  }
  return receipts[0] ?? null;
}

function resolveDeliveryStatus(
  item: WireInboxItem,
  selectedAgentName?: string | null,
): "delivered" | "acked" | "pending" | string {
  const receipt = resolvePrimaryReceipt(item, selectedAgentName);
  if (receipt?.acked_at || receipt?.status === "acked") {
    return "acked";
  }
  if (receipt?.status) {
    return receipt.status;
  }
  return "delivered";
}

export const WireMailPane: React.FC<WireMailPaneProps> = ({
  messages,
  selectedAgentName,
  agents,
  loading = false,
  error = null,
  onRefresh,
  onAckMessage,
  onSendMessage,
  apiClient,
}) => {
  const [internalMessages, setInternalMessages] = useState<WireInboxItem[]>([]);
  const [internalLoading, setInternalLoading] = useState(false);
  const [internalError, setInternalError] = useState<string | null>(null);

  const [scope, setScope] = useState<RecipientScope>("selected");
  const [unreadOnly, setUnreadOnly] = useState<boolean>(false);
  const [searchQuery, setSearchQuery] = useState<string>("");
  const [localAckedIds, setLocalAckedIds] = useState<Set<string>>(() => new Set());

  // Compose Form State
  const effectiveAgents = useMemo(
    () => (agents && agents.length > 0 ? agents : DEFAULT_WORKSPACE_AGENTS),
    [agents],
  );
  const [toRecipient, setToRecipient] = useState<string>(
    selectedAgentName?.trim().toLowerCase() || "jarvis",
  );
  const [customRecipient, setCustomRecipient] = useState<string>("");
  const [urgency, setUrgency] = useState<"normal" | "urgent">("normal");
  const [subject, setSubject] = useState<string>("");
  const [body, setBody] = useState<string>("");
  const [sending, setSending] = useState<boolean>(false);
  const [actionMessage, setActionMessage] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const effectiveMessages = messages ?? internalMessages;
  const effectiveLoading = loading || internalLoading;
  const effectiveError = error ?? internalError;

  const fetchMessages = React.useCallback(async () => {
    if (!apiClient || messages !== undefined) {
      return;
    }
    setInternalLoading(true);
    setInternalError(null);
    try {
      const res = await apiClient.getWireMessages();
      setInternalMessages(res.messages ?? []);
    } catch (err) {
      setInternalError(err instanceof Error ? err.message : "Failed to load FerroWire messages");
    } finally {
      setInternalLoading(false);
    }
  }, [apiClient, messages]);

  React.useEffect(() => {
    void fetchMessages();
  }, [fetchMessages]);

  const filteredMessages = useMemo(() => {
    const selectedKey = (selectedAgentName ?? "").trim().toLowerCase();
    const query = searchQuery.trim().toLowerCase();

    return effectiveMessages.filter((item) => {
      const msg = item.message;
      const toKey = (msg.to ?? "").trim().toLowerCase();
      const senderKey = (msg.sender_name ?? "").trim().toLowerCase();

      if (scope === "selected" && selectedKey) {
        const matchesAgent =
          toKey === selectedKey ||
          toKey === "all" ||
          toKey === "agents.all" ||
          senderKey === selectedKey ||
          (item.receipt?.recipient_name ?? "").trim().toLowerCase() === selectedKey;
        if (!matchesAgent) {
          return false;
        }
      }

      const status = localAckedIds.has(msg.id)
        ? "acked"
        : resolveDeliveryStatus(item, selectedAgentName);

      if (unreadOnly && status === "acked") {
        return false;
      }

      if (query) {
        const haystack = `${msg.subject} ${msg.body} ${msg.sender_name} ${msg.to}`.toLowerCase();
        if (!haystack.includes(query)) {
          return false;
        }
      }

      return true;
    });
  }, [effectiveMessages, scope, selectedAgentName, unreadOnly, searchQuery, localAckedIds]);

  const handleAck = async (messageId: string) => {
    const byAgent = selectedAgentName?.trim().toLowerCase() || "owner";
    setActionError(null);
    try {
      if (onAckMessage) {
        await onAckMessage(messageId, byAgent);
      } else if (apiClient) {
        await apiClient.ackWireMessage({ id: messageId, by: byAgent });
        await fetchMessages();
      }
      setLocalAckedIds((prev) => {
        const next = new Set(prev);
        next.add(messageId);
        return next;
      });
    } catch (err) {
      setActionError(err instanceof Error ? err.message : "Failed to acknowledge message");
    }
  };

  const submitCompose = async () => {
    const targetTo = (customRecipient.trim() || toRecipient.trim()).toLowerCase();
    const trimmedSubject = subject.trim();
    const trimmedBody = body.trim();

    if (!targetTo || !trimmedSubject || !trimmedBody) {
      setActionError("Recipient, subject, and body are required.");
      return;
    }

    const fromAgent = selectedAgentName?.trim().toLowerCase() || "owner";
    const payload: WireSendRequest = {
      to: targetTo,
      subject: trimmedSubject,
      body: trimmedBody,
      urgency,
      from: fromAgent,
      sender_name: fromAgent,
      durable: true,
    };

    setSending(true);
    setActionError(null);
    setActionMessage(null);
    try {
      if (onSendMessage) {
        await onSendMessage(payload);
      } else if (apiClient) {
        await apiClient.sendWireMessage(payload);
        await fetchMessages();
      }
      setSubject("");
      setBody("");
      setCustomRecipient("");
      setActionMessage(`Dispatched FerroWire message to ${targetTo}`);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : "Failed to send FerroWire message");
    } finally {
      setSending(false);
    }
  };

  return (
    <section className="fd-pane" data-testid="wire-mail-pane">
      {/* Filter & Scope Toolbar */}
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
        <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
          <button
            type="button"
            className={`fd-btn ${scope === "selected" ? "fd-btn-active" : ""}`}
            aria-pressed={scope === "selected"}
            onClick={() => setScope("selected")}
          >
            Selected Agent ({selectedAgentName || "All"})
          </button>
          <button
            type="button"
            className={`fd-btn ${scope === "all" ? "fd-btn-active" : ""}`}
            aria-pressed={scope === "all"}
            onClick={() => setScope("all")}
          >
            All / Broadcast
          </button>
          <button
            type="button"
            className={`fd-btn ${unreadOnly ? "fd-btn-active" : ""}`}
            aria-pressed={unreadOnly}
            onClick={() => setUnreadOnly((prev) => !prev)}
          >
            Unread Only
          </button>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
          <input
            type="search"
            name="fd-wire-search"
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            data-1p-ignore="true"
            data-lpignore="true"
            data-form-type="other"
            className="fd-input"
            placeholder="Search subject, body, sender..."
            aria-label="Search FerroWire messages"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            style={{ width: "220px", fontSize: "12px", padding: "5px 9px" }}
          />
          <button
            type="button"
            className="fd-btn"
            onClick={() => {
              if (onRefresh) {
                onRefresh();
              } else {
                void fetchMessages();
              }
            }}
            disabled={effectiveLoading}
          >
            {effectiveLoading ? "Syncing..." : "Refresh"}
          </button>
        </div>
      </div>

      {/* Main Split Content: Inbox List + Compose Panel */}
      <div
        className="fd-scroll"
        style={{
          display: "grid",
          gridTemplateColumns: "minmax(0, 1.4fr) minmax(280px, 1fr)",
          gap: "16px",
          alignItems: "start",
        }}
      >
        {/* Left Column: Inbox Messages */}
        <div style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
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

          {(actionMessage || actionError) && (
            <div
              role="status"
              className="fd-card"
              style={{
                borderColor: actionError
                  ? "rgba(239, 68, 68, 0.45)"
                  : "rgba(16, 185, 129, 0.45)",
                backgroundColor: actionError
                  ? "rgba(239, 68, 68, 0.12)"
                  : "rgba(16, 185, 129, 0.12)",
                color: actionError ? "#FCA5A5" : "var(--diff-add-text)",
                fontSize: "12px",
              }}
            >
              {actionError ?? actionMessage}
            </div>
          )}

          {filteredMessages.length === 0 ? (
            <div
              className="fd-card"
              data-testid="wire-mail-empty"
              style={{
                textAlign: "center",
                padding: "28px 16px",
                color: "var(--text-muted)",
              }}
            >
              No FerroWire messages match the current filters.
            </div>
          ) : (
            filteredMessages.map((item) => {
              const msg = item.message;
              const status = localAckedIds.has(msg.id)
                ? "acked"
                : resolveDeliveryStatus(item, selectedAgentName);
              const isUrgent = msg.urgency === "urgent";
              const isAcked = status === "acked";

              return (
                <article
                  key={msg.id}
                  data-testid={`mail-item-${msg.id}`}
                  className="fd-card"
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "8px",
                    borderColor: isUrgent ? "rgba(239, 68, 68, 0.45)" : undefined,
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: "8px",
                      flexWrap: "wrap",
                    }}
                  >
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: "6px",
                        flexWrap: "wrap",
                      }}
                    >
                      <span className="fd-badge fd-mono" style={{ fontWeight: 700 }}>
                        {msg.sender_name}
                      </span>
                      <span style={{ color: "var(--text-dim)", fontSize: "12px" }}>→</span>
                      <span className="fd-badge fd-mono">{msg.to}</span>

                      <span
                        data-testid={`mail-urgency-${msg.id}`}
                        className="fd-badge"
                        style={
                          isUrgent
                            ? {
                                backgroundColor: "rgba(239, 68, 68, 0.16)",
                                borderColor: "rgba(239, 68, 68, 0.45)",
                                color: "#FCA5A5",
                              }
                            : undefined
                        }
                      >
                        {msg.urgency}
                      </span>

                      <span
                        data-testid={`mail-status-${msg.id}`}
                        className="fd-badge"
                        style={
                          isAcked
                            ? {
                                color: "var(--status-alive)",
                                borderColor: "rgba(16, 185, 129, 0.35)",
                                backgroundColor: "rgba(16, 185, 129, 0.1)",
                              }
                            : {
                                color: "var(--accent-hover)",
                                borderColor: "var(--border-accent)",
                                backgroundColor: "var(--accent-subtle)",
                              }
                        }
                      >
                        {status}
                      </span>
                    </div>

                    <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                      <time
                        className="fd-mono"
                        style={{ fontSize: "11px", color: "var(--text-dim)" }}
                      >
                        {msg.created_at}
                      </time>

                      {!isAcked && (
                        <button
                          type="button"
                          data-testid={`ack-btn-${msg.id}`}
                          className="fd-btn fd-btn-primary"
                          style={{ padding: "3px 8px", fontSize: "11px" }}
                          onClick={() => void handleAck(msg.id)}
                        >
                          Acknowledge
                        </button>
                      )}
                    </div>
                  </div>

                  <div style={{ fontWeight: 600, fontSize: "14px", color: "var(--text-primary)" }}>
                    {msg.subject}
                  </div>

                  <div
                    style={{
                      fontSize: "13px",
                      color: "var(--text-secondary)",
                      whiteSpace: "pre-wrap",
                      lineHeight: 1.5,
                    }}
                  >
                    {msg.body}
                  </div>
                </article>
              );
            })
          )}
        </div>

        {/* Right Column: Compose FerroWire Message Panel */}
        <div
          role="region"
          aria-label="Compose FerroWire Message"
          className="fd-card"
          data-testid="wire-compose-form"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "10px",
          }}
        >
          <div
            style={{
              fontWeight: 700,
              fontSize: "13px",
              textTransform: "uppercase",
              letterSpacing: "0.05em",
              color: "var(--text-secondary)",
            }}
          >
            Compose FerroWire Message
          </div>

          <label style={{ display: "flex", flexDirection: "column", gap: "4px", fontSize: "12px" }}>
            <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Recipient (To)</span>
            <select
              aria-label="Recipient (To)"
              className="fd-select"
              value={toRecipient}
              onChange={(e) => setToRecipient(e.target.value)}
            >
              <option value="all">all (Broadcast to Workspace)</option>
              {effectiveAgents.map((ag) => (
                <option key={ag.name_key} value={ag.name_key.toLowerCase()}>
                  {ag.display_name} ({ag.name_key} · {ag.project})
                </option>
              ))}
            </select>
          </label>

          <label style={{ display: "flex", flexDirection: "column", gap: "4px", fontSize: "12px" }}>
            <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>
              Custom Address Override (Optional)
            </span>
            <input
              type="text"
              name="fd-wire-custom-recipient"
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              data-1p-ignore="true"
              data-lpignore="true"
              data-form-type="other"
              aria-label="Custom Address Override"
              className="fd-input fd-mono"
              placeholder="e.g. kacey or agents.all"
              value={customRecipient}
              onChange={(e) => setCustomRecipient(e.target.value)}
            />
          </label>

          <label style={{ display: "flex", flexDirection: "column", gap: "4px", fontSize: "12px" }}>
            <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Message Urgency</span>
            <select
              aria-label="Message Urgency"
              className="fd-select"
              value={urgency}
              onChange={(e) => setUrgency(e.target.value as "normal" | "urgent")}
            >
              <option value="normal">normal</option>
              <option value="urgent">urgent</option>
            </select>
          </label>

          <label style={{ display: "flex", flexDirection: "column", gap: "4px", fontSize: "12px" }}>
            <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Message Subject</span>
            <input
              type="text"
              name="fd-wire-subject"
              autoComplete="off"
              autoCorrect="off"
              spellCheck={false}
              data-1p-ignore="true"
              data-lpignore="true"
              data-form-type="other"
              aria-label="Message Subject"
              className="fd-input"
              placeholder="Subject line"
              value={subject}
              onChange={(e) => setSubject(e.target.value)}
            />
          </label>

          <label style={{ display: "flex", flexDirection: "column", gap: "4px", fontSize: "12px" }}>
            <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Message Body</span>
            <textarea
              rows={4}
              name="fd-wire-body"
              autoComplete="off"
              autoCorrect="off"
              spellCheck={false}
              data-1p-ignore="true"
              data-lpignore="true"
              data-form-type="other"
              aria-label="Message Body"
              className="fd-textarea"
              placeholder="Write your FerroWire message..."
              value={body}
              onChange={(e) => setBody(e.target.value)}
              style={{ resize: "vertical" }}
            />
          </label>

          <button
            type="button"
            className="fd-btn fd-btn-primary"
            onClick={() => void submitCompose()}
            disabled={sending || !subject.trim() || !body.trim()}
          >
            {sending ? "Sending..." : "Send Message"}
          </button>
        </div>
      </div>
    </section>
  );
};
