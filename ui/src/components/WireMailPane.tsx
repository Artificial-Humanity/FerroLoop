import React, { useMemo, useState } from "react";
import {
  DEFAULT_ROUTED_CI_PR_EVENTS,
  type RoutedCiPrFeedbackEvent,
} from "./TriageQueuePane";
export { DEFAULT_ROUTED_CI_PR_EVENTS, type RoutedCiPrFeedbackEvent };
import type { HubApiClient } from "../api/client";
import { formatTimestamp, useActiveTimeZone } from "../lib/datetime";
import {
  type AgentRecord,
  type CommitteeVoteVerdict,
  type WireCommitteeMetadata,
  type WireDaemonState,
  type WireDelegateMetadata,
  type WireInboxItem,
  type WireMessage,
  type WireReceipt,
  type WireSendRequest,
  type WireStatusResponse,
} from "../types/api";

export type WireIntentMode = "mail" | "delegate" | "committee";

export interface ParsedWireIntent {
  intent: WireIntentMode;
  delegate?: WireDelegateMetadata;
  committee?: WireCommitteeMetadata;
}

export function normalizeCommitteeVerdict(raw: string): CommitteeVoteVerdict {
  const upper = raw.trim().toUpperCase().replace(/_/g, " ");
  if (upper === "APPROVE") return "APPROVE";
  if (
    upper === "APPROVE WITH AMENDMENTS" ||
    upper === "AMEND" ||
    upper === "APPROVE AMENDMENTS"
  ) {
    return "APPROVE WITH AMENDMENTS";
  }
  if (upper === "BLOCK" || upper === "REJECT") return "BLOCK";
  return "PENDING";
}

export function parseCommitteeVotesSpec(
  rawSpec: string,
): Record<string, CommitteeVoteVerdict> {
  const votes: Record<string, CommitteeVoteVerdict> = {};
  for (const part of rawSpec.split(",")) {
    const trimmed = part.trim();
    if (!trimmed) continue;
    const colonIdx = trimmed.indexOf(":");
    if (colonIdx === -1) {
      votes[trimmed.toLowerCase()] = "PENDING";
    } else {
      const seat = trimmed.slice(0, colonIdx).trim().toLowerCase();
      const verdict = normalizeCommitteeVerdict(trimmed.slice(colonIdx + 1));
      if (seat) {
        votes[seat] = verdict;
      }
    }
  }
  return votes;
}

export function formatWireIntentBody(
  baseBody: string,
  intent: WireIntentMode,
  delegate?: WireDelegateMetadata,
  committee?: WireCommitteeMetadata,
): string {
  const cleanBody = baseBody.trim();
  if (intent === "delegate" && delegate) {
    const lines = [
      "[FerroWire Intent: delegate]",
      `Target-Record: ${delegate.target_record || "#0"}`,
      `Deliverable: ${delegate.deliverable || "Assigned task"}`,
      ...(delegate.branch_or_pr
        ? [`Branch-Or-PR: ${delegate.branch_or_pr}`]
        : []),
      `Resume-On-Reply: ${delegate.resume_on_reply ? "true" : "false"}`,
      "",
      cleanBody,
    ];
    return lines.join("\n").trim();
  }
  if (intent === "committee" && committee) {
    const votesEntries = Object.entries(committee.votes ?? {});
    const votesLine =
      votesEntries.length > 0
        ? votesEntries.map(([seat, v]) => `${seat}:${v}`).join(", ")
        : "krono:PENDING, kendi:PENDING, kora:PENDING";
    const lines = [
      "[FerroWire Intent: committee]",
      `Council-Track: ${committee.council_track || "Strategy Council"}`,
      `Quorum: ${committee.quorum || "3/3 unanimous"}`,
      `Artifact-Path: ${committee.artifact_path || "docs/specs/proposal.md"}`,
      ...(committee.artifact_sha256
        ? [`Artifact-SHA256: ${committee.artifact_sha256}`]
        : []),
      `Seat-Votes: ${votesLine}`,
      "",
      cleanBody,
    ];
    return lines.join("\n").trim();
  }
  return cleanBody;
}

export function parseWireIntentMetadata(
  input: Pick<WireMessage, "kind" | "subject" | "body" | "delegate_meta" | "committee_meta"> | string,
): ParsedWireIntent {
  const isString = typeof input === "string";
  const rawKind = isString ? "" : (input.kind || "").toLowerCase();
  const rawSubject = isString ? "" : input.subject || "";
  const rawBody = isString ? input : input.body || "";

  if (!isString && input.delegate_meta) {
    return {
      intent: "delegate",
      delegate: input.delegate_meta,
    };
  }
  if (!isString && input.committee_meta) {
    return {
      intent: "committee",
      committee: input.committee_meta,
    };
  }

  const combined = `${rawSubject}\n${rawBody}`;
  const isDelegateHeader =
    rawKind === "delegate" ||
    /\[(?:FerroWire\s+)?Intent:\s*delegate\b/i.test(combined) ||
    /^\s*Target-Record:\s*\S+/im.test(rawBody);
  if (isDelegateHeader) {
    const targetRecord =
      rawBody.match(/^\s*Target-Record:\s*(.+)$/im)?.[1]?.trim() ||
      rawSubject.match(/#(\d+)/)?.[0] ||
      "unassigned";
    const deliverable =
      rawBody.match(/^\s*Deliverable:\s*(.+)$/im)?.[1]?.trim() ||
      rawSubject.trim() ||
      "Delegated task";
    const branchOrPr =
      rawBody.match(/^\s*Branch-Or-PR:\s*(.+)$/im)?.[1]?.trim() ?? null;
    const resumeRaw = rawBody
      .match(/^\s*Resume-On-Reply:\s*(.+)$/im)?.[1]
      ?.trim()
      .toLowerCase();
    const resumeOnReply = resumeRaw ? resumeRaw !== "false" : true;
    return {
      intent: "delegate",
      delegate: {
        target_record: targetRecord,
        deliverable,
        branch_or_pr: branchOrPr,
        resume_on_reply: resumeOnReply,
      },
    };
  }

  const isCommitteeHeader =
    rawKind === "committee" ||
    /\[(?:FerroWire\s+)?Intent:\s*committee\b/i.test(combined) ||
    /^\s*Council-Track:\s*\S+/im.test(rawBody);
  if (isCommitteeHeader) {
    const councilTrack =
      rawBody.match(/^\s*Council-Track:\s*(.+)$/im)?.[1]?.trim() ||
      "Strategy Council #2";
    const quorum =
      rawBody.match(/^\s*Quorum:\s*(.+)$/im)?.[1]?.trim() || "3/3 unanimous";
    const artifactPath =
      rawBody.match(/^\s*Artifact-Path:\s*(.+)$/im)?.[1]?.trim() ||
      "docs/specs/deliberation.md";
    const artifactSha =
      rawBody.match(/^\s*Artifact-SHA256:\s*(.+)$/im)?.[1]?.trim() ?? null;
    const votesRaw =
      rawBody.match(/^\s*Seat-Votes:\s*(.+)$/im)?.[1]?.trim() ||
      "krono:PENDING, kendi:PENDING, kora:PENDING";
    return {
      intent: "committee",
      committee: {
        council_track: councilTrack,
        quorum,
        artifact_path: artifactPath,
        artifact_sha256: artifactSha,
        votes: parseCommitteeVotesSpec(votesRaw),
      },
    };
  }

  return { intent: "mail" };
}

export interface WireMailPaneProps {
  messages?: WireInboxItem[];
  wireStatus?: WireStatusResponse | null;
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

function isValidWireDaemonState(val: unknown): val is WireDaemonState {
  return val === "active" || val === "inactive" || val === "unreachable";
}

export function formatPendingAge(ageMs?: number | null): string {
  if (ageMs === null || ageMs === undefined || !Number.isFinite(ageMs) || ageMs < 0) {
    return "unknown";
  }
  if (ageMs < 1_000) {
    return `${Math.floor(ageMs)}ms`;
  }
  if (ageMs < 60_000) {
    return `${Math.floor(ageMs / 1_000)}s`;
  }
  if (ageMs < 3_600_000) {
    const mins = Math.floor(ageMs / 60_000);
    const secs = Math.floor((ageMs % 60_000) / 1_000);
    return `${mins}m ${secs}s`;
  }
  const hours = Math.floor(ageMs / 3_600_000);
  const mins = Math.floor((ageMs % 3_600_000) / 60_000);
  return `${hours}h ${mins}m`;
}

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

function ackKey(agent: string | null | undefined, messageId: string): string {
  return `${(agent ?? "").trim().toLowerCase() || "owner"}\u0000${messageId}`;
}

export const WireMailPane: React.FC<WireMailPaneProps> = ({
  messages,
  wireStatus,
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
  const [internalWireStatus, setInternalWireStatus] = useState<WireStatusResponse | null>(null);
  const [internalLoading, setInternalLoading] = useState(false);
  const [internalError, setInternalError] = useState<string | null>(null);
  const activeTimeZone = useActiveTimeZone();

  const [scope, setScope] = useState<RecipientScope>("selected");
  const [unreadOnly, setUnreadOnly] = useState<boolean>(false);
  const [searchQuery, setSearchQuery] = useState<string>("");
  // Receipts are per recipient, so the optimistic "acked" marker is keyed by
  // (agent, message). Keyed by message alone, acking as one agent hid the
  // message as acked for every other agent the operator switched to.
  const [localAckedIds, setLocalAckedIds] = useState<Set<string>>(() => new Set());

  // Compose Form State
  const effectiveAgents = useMemo(() => agents ?? [], [agents]);
  const [toRecipient, setToRecipient] = useState<string>(
    selectedAgentName?.trim().toLowerCase() || "jarvis",
  );
  const [customRecipient, setCustomRecipient] = useState<string>("");
  const [urgency, setUrgency] = useState<"normal" | "urgent">("normal");
  const [intentMode, setIntentMode] = useState<WireIntentMode>("mail");
  // Record #101: Typed Delegate & Committee composer state
  const [delegateTargetRecord, setDelegateTargetRecord] = useState<string>("#101");
  const [delegateDeliverable, setDelegateDeliverable] = useState<string>("");
  const [delegateBranchOrPr, setDelegateBranchOrPr] = useState<string>("");
  const [delegateResumeOnReply, setDelegateResumeOnReply] = useState<boolean>(true);
  const [committeeTrack, setCommitteeTrack] = useState<string>("Strategy Council #2");
  const [committeeQuorum, setCommitteeQuorum] = useState<string>("3/3 unanimous");
  const [committeeArtifactPath, setCommitteeArtifactPath] = useState<string>(
    "docs/specs/proposal.md",
  );
  const [committeeArtifactSha, setCommitteeArtifactSha] = useState<string>("");
  const [committeeSeatsSpec, setCommitteeSeatsSpec] = useState<string>(
    "krono:APPROVE, kendi:APPROVE WITH AMENDMENTS, kora:PENDING",
  );
  const [subject, setSubject] = useState<string>("");
  const [body, setBody] = useState<string>("");
  const [sending, setSending] = useState<boolean>(false);
  const [actionMessage, setActionMessage] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [expandedIds, setExpandedIds] = useState<Set<string>>(() => new Set());
  const [loadedIds, setLoadedIds] = useState<Set<string>>(() => new Set());
  const paneRef = React.useRef<HTMLElement | null>(null);
  const [paneWidth, setPaneWidth] = useState<number>(0);

  React.useEffect(() => {
    const el = paneRef.current;
    if (!el) return;
    const initialRect = el.getBoundingClientRect?.();
    if (initialRect && initialRect.width > 0) {
      setPaneWidth(initialRect.width);
    }
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const w = entry.contentRect?.width ?? el.getBoundingClientRect?.().width ?? 0;
        setPaneWidth(w);
      }
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  const isNarrow = paneWidth < 680;

  const toggleMessageExpanded = React.useCallback((messageId: string) => {
    setLoadedIds((prev) => {
      if (prev.has(messageId)) return prev;
      const next = new Set(prev);
      next.add(messageId);
      return next;
    });
    setExpandedIds((prev) => {
      const next = new Set(prev);
      if (next.has(messageId)) {
        next.delete(messageId);
      } else {
        next.add(messageId);
      }
      return next;
    });
  }, []);

  const effectiveMessages = messages ?? internalMessages;
  const effectiveWireStatus = wireStatus !== undefined ? wireStatus : internalWireStatus;
  const effectiveLoading = loading || internalLoading;
  const effectiveError = error ?? internalError;

  const fetchWireStatus = React.useCallback(async () => {
    if (
      !apiClient ||
      typeof apiClient.getWireStatus !== "function" ||
      wireStatus !== undefined
    ) {
      return;
    }
    try {
      const res = await apiClient.getWireStatus();
      if (res && typeof res === "object" && isValidWireDaemonState(res.daemon)) {
        setInternalWireStatus({
          daemon: res.daemon,
          dispatch_diagnostics: res.dispatch_diagnostics ?? null,
          pending_recipients_available:
            typeof res.pending_recipients_available === "boolean"
              ? res.pending_recipients_available
              : res.pending_recipients !== null,
          pending_recipients: Array.isArray(res.pending_recipients)
            ? res.pending_recipients
            : res.pending_recipients === null
              ? null
              : [],
        });
      }
    } catch {
      setInternalWireStatus({
        daemon: "unreachable",
        dispatch_diagnostics: null,
        pending_recipients_available: false,
        pending_recipients: [],
      });
    }
  }, [apiClient, wireStatus]);

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

  React.useEffect(() => {
    void fetchWireStatus();
  }, [fetchWireStatus]);

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

      const status = localAckedIds.has(ackKey(selectedAgentName, msg.id))
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
        next.add(ackKey(byAgent, messageId));
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
    const delegateMeta: WireDelegateMetadata | undefined =
      intentMode === "delegate"
        ? {
            target_record: delegateTargetRecord.trim() || "#0",
            deliverable: delegateDeliverable.trim() || trimmedSubject,
            branch_or_pr: delegateBranchOrPr.trim() || null,
            resume_on_reply: delegateResumeOnReply,
          }
        : undefined;
    const committeeMeta: WireCommitteeMetadata | undefined =
      intentMode === "committee"
        ? {
            council_track: committeeTrack.trim() || "Strategy Council #2",
            quorum: committeeQuorum.trim() || "3/3 unanimous",
            artifact_path: committeeArtifactPath.trim() || "docs/specs/proposal.md",
            artifact_sha256: committeeArtifactSha.trim() || null,
            votes: parseCommitteeVotesSpec(committeeSeatsSpec),
          }
        : undefined;

    const effectiveBody = formatWireIntentBody(
      trimmedBody,
      intentMode,
      delegateMeta,
      committeeMeta,
    );

    const payload: WireSendRequest = {
      to: targetTo,
      subject: trimmedSubject,
      body: effectiveBody,
      kind: intentMode,
      urgency,
      from: fromAgent,
      sender_name: fromAgent,
      durable: true,
      ...(delegateMeta ? { delegate_meta: delegateMeta } : {}),
      ...(committeeMeta ? { committee_meta: committeeMeta } : {}),
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
      setActionMessage(`Dispatched FerroWire ${intentMode} message to ${targetTo}`);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : "Failed to send FerroWire message");
    } finally {
      setSending(false);
    }
  };

  const composePanel = (
    <div
      role="region"
      aria-label="Compose FerroWire Message"
      className="fd-card"
      data-testid="wire-compose-form"
      style={{
        display: "flex",
        flexDirection: "column",
        gap: "10px",
        minWidth: 0,
        maxWidth: "100%",
        boxSizing: "border-box",
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
            fontWeight: 700,
            fontSize: "13px",
            textTransform: "uppercase",
            letterSpacing: "0.05em",
            color: "var(--text-secondary)",
          }}
        >
          Compose Message
        </div>
        {intentMode === "delegate" && delegateResumeOnReply && (
          <span
            className="fd-badge"
            data-testid="delegate-composer-status-badge"
            style={{
              backgroundColor: "rgba(56, 189, 248, 0.14)",
              borderColor: "rgba(56, 189, 248, 0.4)",
              color: "var(--accent-primary)",
              fontSize: "10.5px",
            }}
          >
            Non-Blocking · Resume on Reply
          </span>
        )}
      </div>

      <label style={{ display: "flex", flexDirection: "column", gap: "4px", fontSize: "12px", minWidth: 0 }}>
        <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>FerroWire Intent Mode</span>
        <select
          aria-label="FerroWire Intent Mode"
          data-testid="wire-compose-intent-select"
          className="fd-select"
          value={intentMode}
          onChange={(e) => setIntentMode(e.target.value as WireIntentMode)}
          style={{ width: "100%", minWidth: 0, boxSizing: "border-box" }}
        >
          <option value="mail">mail (Standard Message)</option>
          <option value="delegate">delegate (Single-Assignee Task Delegation)</option>
          <option value="committee">committee (Multi-Seat Deliberation Round)</option>
        </select>
      </label>

      {intentMode === "delegate" && (
        <div
          data-testid="wire-delegate-fields"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "8px",
            padding: "8px 10px",
            borderRadius: "6px",
            backgroundColor: "var(--bg-elevated)",
            border: "1px solid var(--border-subtle)",
          }}
        >
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: "8px" }}>
            <label style={{ display: "flex", flexDirection: "column", gap: "3px", fontSize: "11.5px" }}>
              <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Target Record</span>
              <input
                type="text"
                data-testid="delegate-target-record-input"
                aria-label="Target Record"
                className="fd-input fd-mono"
                placeholder="#101"
                value={delegateTargetRecord}
                onChange={(e) => setDelegateTargetRecord(e.target.value)}
              />
            </label>
            <label style={{ display: "flex", flexDirection: "column", gap: "3px", fontSize: "11.5px" }}>
              <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Branch or PR</span>
              <input
                type="text"
                data-testid="delegate-branch-pr-input"
                aria-label="Branch or PR"
                className="fd-input fd-mono"
                placeholder="felix/record-101"
                value={delegateBranchOrPr}
                onChange={(e) => setDelegateBranchOrPr(e.target.value)}
              />
            </label>
          </div>
          <label style={{ display: "flex", flexDirection: "column", gap: "3px", fontSize: "11.5px" }}>
            <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Deliverable Summary</span>
            <input
              type="text"
              data-testid="delegate-deliverable-input"
              aria-label="Deliverable Summary"
              className="fd-input"
              placeholder="Implement typed delegate and committee intents"
              value={delegateDeliverable}
              onChange={(e) => setDelegateDeliverable(e.target.value)}
            />
          </label>
          <label
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: "6px",
              fontSize: "11.5px",
              cursor: "pointer",
            }}
          >
            <input
              type="checkbox"
              data-testid="delegate-resume-on-reply-checkbox"
              checked={delegateResumeOnReply}
              onChange={(e) => setDelegateResumeOnReply(e.target.checked)}
            />
            <span>Non-Blocking · Resume on Reply (`resume_on_reply`)</span>
          </label>
        </div>
      )}

      {intentMode === "committee" && (
        <div
          data-testid="wire-committee-fields"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "8px",
            padding: "8px 10px",
            borderRadius: "6px",
            backgroundColor: "var(--bg-elevated)",
            border: "1px solid var(--border-subtle)",
          }}
        >
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: "8px" }}>
            <label style={{ display: "flex", flexDirection: "column", gap: "3px", fontSize: "11.5px" }}>
              <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Council Track</span>
              <input
                type="text"
                data-testid="committee-track-input"
                aria-label="Council Track"
                className="fd-input"
                value={committeeTrack}
                onChange={(e) => setCommitteeTrack(e.target.value)}
              />
            </label>
            <label style={{ display: "flex", flexDirection: "column", gap: "3px", fontSize: "11.5px" }}>
              <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Quorum Rule</span>
              <input
                type="text"
                data-testid="committee-quorum-input"
                aria-label="Quorum Rule"
                className="fd-input fd-mono"
                value={committeeQuorum}
                onChange={(e) => setCommitteeQuorum(e.target.value)}
              />
            </label>
          </div>
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: "8px" }}>
            <label style={{ display: "flex", flexDirection: "column", gap: "3px", fontSize: "11.5px" }}>
              <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Artifact Path</span>
              <input
                type="text"
                data-testid="committee-artifact-path-input"
                aria-label="Artifact Path"
                className="fd-input fd-mono"
                value={committeeArtifactPath}
                onChange={(e) => setCommitteeArtifactPath(e.target.value)}
              />
            </label>
            <label style={{ display: "flex", flexDirection: "column", gap: "3px", fontSize: "11.5px" }}>
              <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Artifact SHA-256</span>
              <input
                type="text"
                data-testid="committee-artifact-sha-input"
                aria-label="Artifact SHA-256"
                className="fd-input fd-mono"
                placeholder="sha256:9f86d08..."
                value={committeeArtifactSha}
                onChange={(e) => setCommitteeArtifactSha(e.target.value)}
              />
            </label>
          </div>
          <label style={{ display: "flex", flexDirection: "column", gap: "3px", fontSize: "11.5px" }}>
            <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>
              Seat Votes (`seat:APPROVE | APPROVE WITH AMENDMENTS | BLOCK | PENDING`)
            </span>
            <input
              type="text"
              data-testid="committee-seats-input"
              aria-label="Seat Votes"
              className="fd-input fd-mono"
              value={committeeSeatsSpec}
              onChange={(e) => setCommitteeSeatsSpec(e.target.value)}
            />
          </label>
        </div>
      )}

      <label style={{ display: "flex", flexDirection: "column", gap: "4px", fontSize: "12px", minWidth: 0 }}>
        <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Recipient (To)</span>
        <select
          aria-label="Recipient (To)"
          className="fd-select"
          value={toRecipient}
          onChange={(e) => setToRecipient(e.target.value)}
          style={{ width: "100%", minWidth: 0, boxSizing: "border-box" }}
        >
          <option value="all">all (Broadcast to Workspace)</option>
          {effectiveAgents.map((ag) => (
            <option key={ag.name_key} value={ag.name_key.toLowerCase()}>
              {ag.display_name} ({ag.name_key} · {ag.project})
            </option>
          ))}
        </select>
      </label>

      <label style={{ display: "flex", flexDirection: "column", gap: "4px", fontSize: "12px", minWidth: 0 }}>
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
          style={{ width: "100%", minWidth: 0, boxSizing: "border-box" }}
        />
      </label>

      <label style={{ display: "flex", flexDirection: "column", gap: "4px", fontSize: "12px", minWidth: 0 }}>
        <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Message Urgency</span>
        <select
          aria-label="Message Urgency"
          className="fd-select"
          value={urgency}
          onChange={(e) => setUrgency(e.target.value as "normal" | "urgent")}
          style={{ width: "100%", minWidth: 0, boxSizing: "border-box" }}
        >
          <option value="normal">normal</option>
          <option value="urgent">urgent</option>
        </select>
      </label>

      <label style={{ display: "flex", flexDirection: "column", gap: "4px", fontSize: "12px", minWidth: 0 }}>
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
          style={{ width: "100%", minWidth: 0, boxSizing: "border-box" }}
        />
      </label>

      <label style={{ display: "flex", flexDirection: "column", gap: "4px", fontSize: "12px", minWidth: 0 }}>
        <span style={{ color: "var(--text-muted)", fontWeight: 600 }}>Message Body</span>
        <textarea
          rows={3}
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
          style={{ resize: "vertical", width: "100%", minWidth: 0, boxSizing: "border-box" }}
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
  );

  const inboxColumn = (
    <div
      data-testid="wire-inbox-column"
      style={{ display: "flex", flexDirection: "column", gap: "10px", minWidth: 0 }}
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
        filteredMessages.map((item, index) => {
          const msg = item.message;
          const status = localAckedIds.has(ackKey(selectedAgentName, msg.id))
            ? "acked"
            : resolveDeliveryStatus(item, selectedAgentName);
          const isUrgent = msg.urgency === "urgent";
          const isAcked = status === "acked";
          const isExpanded = expandedIds.has(msg.id);
          const isMostRecent = index === 0;
          const shouldMountBody = isMostRecent || isExpanded || loadedIds.has(msg.id);
          const parsedIntent = parseWireIntentMetadata(msg);

          return (
            <article
              key={msg.id}
              data-testid={`mail-item-${msg.id}`}
              data-expanded={isExpanded ? "true" : "false"}
              data-wire-intent={parsedIntent.intent}
              className="fd-card"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "8px",
                minWidth: 0,
                maxWidth: "100%",
                boxSizing: "border-box",
                borderColor: isUrgent ? "rgba(239, 68, 68, 0.45)" : undefined,
              }}
            >
              <div
                data-testid={`mail-toggle-${msg.id}`}
                role="button"
                tabIndex={0}
                aria-expanded={isExpanded}
                onClick={() => toggleMessageExpanded(msg.id)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    toggleMessageExpanded(msg.id);
                  }
                }}
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
                  cursor: "pointer",
                  minWidth: 0,
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
                      minWidth: 0,
                    }}
                  >
                    <span
                      style={{
                        display: "inline-flex",
                        alignItems: "center",
                        gap: "6px",
                        flexShrink: 0,
                      }}
                    >
                      <span
                        aria-hidden="true"
                        style={{
                          fontSize: "11px",
                          color: "var(--text-muted)",
                          width: "12px",
                          display: "inline-block",
                        }}
                      >
                        {isExpanded ? "▾" : "▸"}
                      </span>

                      <span
                        data-testid={`mail-status-${msg.id}`}
                        className="fd-badge"
                        title={
                          item.receipt?.acked_at
                            ? `Acked at ${formatTimestamp(item.receipt.acked_at, activeTimeZone)}`
                            : undefined
                        }
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
                    </span>

                    <span
                      data-testid={`mail-route-${msg.id}`}
                      style={{
                        display: "inline-flex",
                        alignItems: "center",
                        gap: "6px",
                        flexWrap: "wrap",
                        minWidth: 0,
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

                      {parsedIntent.intent !== "mail" && (
                        <span
                          data-testid={`mail-intent-badge-${msg.id}`}
                          className="fd-badge fd-mono"
                          style={{
                            backgroundColor:
                              parsedIntent.intent === "delegate"
                                ? "rgba(56, 189, 248, 0.14)"
                                : "rgba(168, 85, 247, 0.14)",
                            borderColor:
                              parsedIntent.intent === "delegate"
                                ? "rgba(56, 189, 248, 0.4)"
                                : "rgba(168, 85, 247, 0.4)",
                            color:
                              parsedIntent.intent === "delegate"
                                ? "var(--accent-primary)"
                                : "#C084FC",
                          }}
                        >
                          {parsedIntent.intent}
                        </span>
                      )}
                    </span>
                  </div>

                  <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                    <time
                      dateTime={msg.created_at}
                      title={msg.created_at}
                      className="fd-mono"
                      style={{ fontSize: "11px", color: "var(--text-dim)" }}
                    >
                      {formatTimestamp(msg.created_at, activeTimeZone)}
                    </time>

                    {!isAcked && (
                      <button
                        type="button"
                        data-testid={`ack-btn-${msg.id}`}
                        className="fd-btn fd-btn-primary"
                        style={{ padding: "3px 8px", fontSize: "11px" }}
                        onClick={(e) => {
                          e.stopPropagation();
                          void handleAck(msg.id);
                        }}
                      >
                        Acknowledge
                      </button>
                    )}
                  </div>
                </div>

                <div
                  style={{
                    fontWeight: 600,
                    fontSize: "14px",
                    color: "var(--text-primary)",
                    overflowWrap: "anywhere",
                  }}
                >
                  {msg.subject}
                </div>
              </div>

              {parsedIntent.intent === "delegate" && parsedIntent.delegate && (
                <div
                  data-testid={`mail-delegate-card-${msg.id}`}
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "6px",
                    padding: "8px 10px",
                    borderRadius: "6px",
                    backgroundColor: "var(--bg-elevated)",
                    border: "1px solid rgba(56, 189, 248, 0.35)",
                    fontSize: "12px",
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
                    <span className="fd-mono" style={{ fontWeight: 700, fontSize: "11.5px" }}>
                      Record: {parsedIntent.delegate.target_record}
                    </span>
                    {parsedIntent.delegate.resume_on_reply && (
                      <span
                        className="fd-badge"
                        data-testid={`mail-delegate-resume-badge-${msg.id}`}
                        style={{
                          backgroundColor: "rgba(56, 189, 248, 0.14)",
                          borderColor: "rgba(56, 189, 248, 0.4)",
                          color: "var(--accent-primary)",
                        }}
                      >
                        Non-Blocking · Resume on Reply
                      </span>
                    )}
                  </div>
                  <div style={{ color: "var(--text-secondary)" }}>
                    <strong>Deliverable:</strong> {parsedIntent.delegate.deliverable}
                  </div>
                  {parsedIntent.delegate.branch_or_pr && (
                    <div className="fd-mono" style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                      Branch/PR: {parsedIntent.delegate.branch_or_pr}
                    </div>
                  )}
                </div>
              )}

              {parsedIntent.intent === "committee" && parsedIntent.committee && (
                <div
                  data-testid={`mail-committee-card-${msg.id}`}
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "6px",
                    padding: "8px 10px",
                    borderRadius: "6px",
                    backgroundColor: "var(--bg-elevated)",
                    border: "1px solid rgba(168, 85, 247, 0.35)",
                    fontSize: "12px",
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
                    <span style={{ fontWeight: 700 }}>
                      {parsedIntent.committee.council_track}
                    </span>
                    <span className="fd-badge fd-mono">
                      Quorum: {parsedIntent.committee.quorum}
                    </span>
                  </div>
                  <div className="fd-mono" style={{ fontSize: "11px", color: "var(--text-secondary)" }}>
                    Artifact: {parsedIntent.committee.artifact_path}
                    {parsedIntent.committee.artifact_sha256
                      ? ` · ${parsedIntent.committee.artifact_sha256}`
                      : ""}
                  </div>
                  <div
                    data-testid={`mail-committee-votes-${msg.id}`}
                    style={{ display: "flex", gap: "6px", flexWrap: "wrap" }}
                  >
                    {Object.entries(parsedIntent.committee.votes ?? {}).map(
                      ([seat, verdict]) => (
                        <span
                          key={seat}
                          data-testid={`committee-vote-pill-${msg.id}-${seat}`}
                          className="fd-badge fd-mono"
                          style={{
                            fontSize: "10.5px",
                            backgroundColor:
                              verdict === "APPROVE"
                                ? "rgba(52, 211, 153, 0.14)"
                                : verdict === "APPROVE WITH AMENDMENTS"
                                  ? "rgba(251, 191, 36, 0.16)"
                                  : verdict === "BLOCK"
                                    ? "rgba(239, 68, 68, 0.16)"
                                    : "var(--bg-surface)",
                            color:
                              verdict === "APPROVE"
                                ? "#34d399"
                                : verdict === "APPROVE WITH AMENDMENTS"
                                  ? "#fbbf24"
                                  : verdict === "BLOCK"
                                    ? "#f87171"
                                    : "var(--text-muted)",
                          }}
                        >
                          {seat}: {verdict}
                        </span>
                      ),
                    )}
                  </div>
                </div>
              )}

              {shouldMountBody && (
                <div
                  data-testid={`mail-body-${msg.id}`}
                  data-preloaded={isMostRecent ? "true" : "false"}
                  aria-hidden={!isExpanded}
                  style={{
                    display: isExpanded ? "block" : "none",
                    fontSize: "13px",
                    color: "var(--text-secondary)",
                    whiteSpace: "pre-wrap",
                    overflowWrap: "anywhere",
                    lineHeight: 1.5,
                    paddingTop: "6px",
                    borderTop: "1px solid var(--border-subtle)",
                  }}
                >
                  {msg.body}
                </div>
              )}
            </article>
          );
        })
      )}
    </div>
  );

  const selectedKey = (selectedAgentName ?? "").trim().toLowerCase();
  const statusStrip =
    effectiveWireStatus !== null
      ? (() => {
          const diag = effectiveWireStatus.dispatch_diagnostics;
          const pendingAvailable =
            effectiveWireStatus.pending_recipients_available !== false &&
            effectiveWireStatus.pending_recipients !== null;
          const displayedPending = (effectiveWireStatus.pending_recipients ?? []).filter(
            (item) =>
              item.pending_count > 0 &&
              (scope !== "selected" ||
                !selectedKey ||
                item.recipient.trim().toLowerCase() === selectedKey),
          );
          const isSupportedEmpty =
            diag !== null &&
            diag.records.length === 0 &&
            !diag.global_capacity_exhausted &&
            !diag.diagnostics_partial;
          const displayedRecords =
            diag !== null
              ? scope === "selected" && selectedKey
                ? diag.records.filter(
                    (r) => (r.recipient ?? "").trim().toLowerCase() === selectedKey,
                  )
                : diag.records
              : [];
          const hiddenOtherHoldCount =
            diag !== null ? diag.records.length - displayedRecords.length : 0;

          const unavailableReason =
            effectiveWireStatus.daemon === "inactive"
              ? "wire daemon inactive"
              : effectiveWireStatus.daemon === "unreachable"
                ? "status query failed"
                : "unsupported daemon";

          return (
            <div
              data-testid="wire-status-strip"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "6px",
                padding: "6px 12px",
                borderBottom: "1px solid var(--border-subtle)",
                backgroundColor: "var(--bg-elevated)",
                fontSize: "11.5px",
                minWidth: 0,
              }}
            >
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: "8px",
                  flexWrap: "wrap",
                  minWidth: 0,
                }}
              >
                {diag === null ? (
                  <div
                    data-testid="wire-diagnostics-unavailable"
                    className="fd-mono"
                    style={{ color: "var(--text-muted)" }}
                  >
                    {`Auto-push diagnostics: unavailable (${unavailableReason})`}
                  </div>
                ) : isSupportedEmpty ? (
                  <span
                    data-testid="wire-diagnostics-available"
                    className="fd-badge fd-mono"
                    style={{
                      color: "var(--status-alive)",
                      borderColor: "rgba(16, 185, 129, 0.35)",
                      backgroundColor: "rgba(16, 185, 129, 0.1)",
                    }}
                  >
                    Auto-push diagnostics: 0 holds (daemon_lifetime)
                  </span>
                ) : null}

                {!pendingAvailable ? (
                  <span
                    data-testid="wire-pending-unavailable"
                    className="fd-badge fd-mono"
                    style={{ color: "var(--text-muted)" }}
                  >
                    pending: unavailable
                  </span>
                ) : (
                  displayedPending.map((item) => (
                    <span
                      key={item.recipient}
                      data-testid={`wire-pending-recipient-${item.recipient}`}
                      className="fd-badge fd-mono"
                      style={{
                        color: "var(--accent-hover)",
                        borderColor: "var(--border-accent)",
                        backgroundColor: "var(--accent-subtle)",
                      }}
                    >
                      {`${item.recipient}: ${item.pending_count} pending (oldest ${formatPendingAge(item.oldest_pending_age_ms)})`}
                    </span>
                  ))
                )}
              </div>

              {diag !== null && !isSupportedEmpty && (
                <div
                  data-testid="wire-diagnostics-holds"
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "6px",
                    padding: "6px 8px",
                    borderRadius: "6px",
                    border: "1px solid rgba(251, 191, 36, 0.4)",
                    backgroundColor: "rgba(251, 191, 36, 0.08)",
                    minWidth: 0,
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
                    <span
                      data-testid="wire-diagnostics-hold-count"
                      className="fd-badge fd-mono"
                      style={{
                        borderColor: "rgba(251, 191, 36, 0.45)",
                        backgroundColor: "rgba(251, 191, 36, 0.16)",
                        color: "#fbbf24",
                      }}
                    >
                      {`holds=${diag.records.length}`}
                    </span>
                    {diag.global_capacity_exhausted && (
                      <span
                        data-testid="wire-diagnostics-global-cap"
                        className="fd-badge fd-mono"
                        style={{
                          borderColor: "rgba(239, 68, 68, 0.45)",
                          backgroundColor: "rgba(239, 68, 68, 0.16)",
                          color: "#FCA5A5",
                        }}
                      >
                        global_capacity_exhausted
                      </span>
                    )}
                    {diag.diagnostics_partial && (
                      <span
                        data-testid="wire-diagnostics-partial"
                        className="fd-badge fd-mono"
                        style={{
                          borderColor: "rgba(251, 191, 36, 0.45)",
                          backgroundColor: "rgba(251, 191, 36, 0.16)",
                          color: "#fbbf24",
                        }}
                      >
                        {`diagnostics_partial (omitted=${diag.omitted_evidence_total}${diag.omitted_evidence_total_saturated ? "+" : ""})`}
                      </span>
                    )}
                    {hiddenOtherHoldCount > 0 && (
                      <span
                        className="fd-mono"
                        style={{ fontSize: "11px", color: "var(--text-muted)" }}
                      >
                        {`${hiddenOtherHoldCount} hold(s) on other recipients — switch to All / Broadcast to view`}
                      </span>
                    )}
                  </div>

                  {displayedRecords.map((record, index) => {
                    const rowKey =
                      record.recipient ||
                      record.recipient_digest?.slice(0, 12) ||
                      String(index);
                    const recipientLabel =
                      record.recipient_invalid || !record.recipient
                        ? `invalid-recipient (${record.recipient_digest?.slice(0, 12) ?? "unknown"})`
                        : record.recipient;
                    const triggerLabel = record.trigger_message_id_omitted
                      ? `omitted (${record.trigger_message_id_digest?.slice(0, 12) ?? "unknown"})`
                      : (record.trigger_message_id ?? "none");
                    const stagesSuffix =
                      record.omitted_failure_stages > 0
                        ? ` (+${record.omitted_failure_stages}${record.omitted_failure_stages_saturated ? "+" : ""} omitted)`
                        : "";
                    const routeObsSummary = record.route_observations
                      .map((obs) => `${obs.adapter}:${obs.outcome}`)
                      .join(", ");

                    return (
                      <div
                        key={rowKey}
                        data-testid={`wire-hold-row-${rowKey}`}
                        className="fd-mono"
                        style={{
                          display: "flex",
                          alignItems: "center",
                          gap: "8px",
                          flexWrap: "wrap",
                          fontSize: "11px",
                          color: "var(--text-secondary)",
                        }}
                      >
                        <span className="fd-badge fd-mono" style={{ fontWeight: 700 }}>
                          {recipientLabel}
                        </span>
                        <span className="fd-badge fd-mono">{record.phase}</span>
                        <span>{`decisive_reason=${record.decisive_reason}`}</span>
                        {record.first_reason !== record.decisive_reason && (
                          <span>{`first_reason=${record.first_reason}`}</span>
                        )}
                        {record.failure_stages.length > 0 && (
                          <span>
                            {`stages=${record.failure_stages.join(" -> ")}${stagesSuffix}`}
                          </span>
                        )}
                        <span>{`trigger=${triggerLabel}`}</span>
                        <span>
                          {`observed=${formatTimestamp(record.hold_observed_at, activeTimeZone)}`}
                        </span>
                        <span>
                          {`calls=${record.route_dispatch_calls}${record.route_dispatch_calls_saturated ? "+" : ""}`}
                        </span>
                        {routeObsSummary && <span>{routeObsSummary}</span>}
                        <span>
                          {`registration_generation=${record.registration_generation ?? "unknown"}`}
                        </span>
                      </div>
                    );
                  })}

                  <div style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                    Daemon-lifetime hold suppresses automatic pushes; ACK and route registration do not clear it.
                  </div>
                </div>
              )}
            </div>
          );
        })()
      : null;

  return (
    <section ref={paneRef} className="fd-pane" data-testid="wire-mail-pane">
      {/* Filter & Scope Toolbar */}
      <div
        style={{
          display: "flex",
          flexDirection: "column",
          gap: "8px",
          padding: "8px 12px",
          borderBottom: "1px solid var(--border-subtle)",
          backgroundColor: "var(--bg-surface)",
          minWidth: 0,
        }}
      >
        <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: "8px", flexWrap: "wrap" }}>
          <strong
            data-testid="wire-mail-pane-title"
            style={{ fontSize: "14px", color: "var(--text-primary)" }}
          >
            FerroWire Mail
          </strong>
        </div>

        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: "8px",
            flexWrap: "wrap",
            minWidth: 0,
          }}
        >
        <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap", minWidth: 0 }}>
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

        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "8px",
            flexWrap: "wrap",
            flex: "1 1 200px",
            justifyContent: "flex-end",
            minWidth: 0,
          }}
        >
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
            style={{
              flex: "1 1 160px",
              minWidth: 0,
              maxWidth: "100%",
              fontSize: "12px",
              padding: "5px 9px",
            }}
          />
          <button
            type="button"
            className="fd-btn"
            data-testid="wire-refresh-btn"
            aria-label={effectiveLoading ? "Syncing..." : "Refresh"}
            title="Refresh"
            onClick={() => {
              void fetchWireStatus();
              if (onRefresh) {
                onRefresh();
              } else {
                void fetchMessages();
              }
            }}
            disabled={effectiveLoading}
            style={{ padding: "5px 8px", lineHeight: 1, flexShrink: 0 }}
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
        </div>
        </div>
      </div>

      {statusStrip}

      {/* Main Content: Stacked (Compose above Inbox) when narrow, 2-Column Split when wide */}
      <div
        className="fd-scroll"
        data-testid="wire-mail-layout"
        data-layout={isNarrow ? "stacked" : "split"}
        style={{
          display: "grid",
          gridTemplateColumns: isNarrow
            ? "minmax(0, 1fr)"
            : "minmax(0, 1.4fr) minmax(280px, 1fr)",
          gap: "16px",
          alignItems: "start",
          minWidth: 0,
        }}
      >
        {isNarrow ? (
          <>
            {composePanel}
            {inboxColumn}
          </>
        ) : (
          <>
            {inboxColumn}
            {composePanel}
          </>
        )}
      </div>
    </section>
  );
};

export const WireInboxPane = WireMailPane;
