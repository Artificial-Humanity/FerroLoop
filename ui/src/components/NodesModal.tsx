import React, { useCallback, useEffect, useState } from "react";
import { HubApiClient } from "../api/client";
import type { EnrolledNode, NodeEnrollTokenResponse } from "../types/api";
import { getAdapterBadgeClass } from "./RosterSidebar";

export interface NodesModalProps {
  isOpen: boolean;
  onClose: () => void;
  nodes?: EnrolledNode[];
  onRefreshNodes?: () => Promise<void> | void;
  onCreateEnrollToken?: (
    label?: string,
    ttlSecs?: number,
  ) => Promise<NodeEnrollTokenResponse>;
  onRevokeNode?: (nodeId: string) => Promise<void>;
  apiClient?: HubApiClient;
}

export function formatRemainingTtl(remainingMs: number): string {
  if (remainingMs <= 0) {
    return "Expired";
  }
  const totalSecs = Math.ceil(remainingMs / 1000);
  const mins = Math.floor(totalSecs / 60);
  const secs = totalSecs % 60;
  if (mins > 0) {
    return `${mins}m ${secs}s`;
  }
  return `${secs}s`;
}

export function getNodeStatusDotClass(status: EnrolledNode["status"]): string {
  if (status === "online") {
    return "fd-dot fd-dot-alive";
  }
  if (status === "stale") {
    return "fd-dot fd-dot-stale";
  }
  return "fd-dot fd-dot-dead";
}

export const NodesModal: React.FC<NodesModalProps> = ({
  isOpen,
  onClose,
  nodes: propNodes,
  onRefreshNodes,
  onCreateEnrollToken,
  onRevokeNode,
  apiClient,
}) => {
  const [localNodes, setLocalNodes] = useState<EnrolledNode[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [labelInput, setLabelInput] = useState("");
  const [enrolling, setEnrolling] = useState(false);
  const [enrollToken, setEnrollToken] = useState<NodeEnrollTokenResponse | null>(null);
  const [copied, setCopied] = useState(false);
  const [revokingId, setRevokingId] = useState<string | null>(null);
  const [nowMs, setNowMs] = useState<number>(() => Date.now());

  const isControlled = propNodes !== undefined;
  const effectiveNodes = isControlled ? propNodes : localNodes;

  const refreshLocalNodes = useCallback(async () => {
    if (!apiClient) return;
    setLoading(true);
    setError(null);
    try {
      const fetched = await apiClient.fetchNodes();
      setLocalNodes(fetched);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load nodes");
    } finally {
      setLoading(false);
    }
  }, [apiClient]);

  useEffect(() => {
    if (!isOpen) return;
    if (!isControlled && apiClient) {
      void refreshLocalNodes();
    } else if (onRefreshNodes) {
      void onRefreshNodes();
    }
  }, [isOpen, isControlled, apiClient, refreshLocalNodes, onRefreshNodes]);

  useEffect(() => {
    if (!isOpen || !enrollToken) return;
    setNowMs(Date.now());
    const timer = setInterval(() => {
      setNowMs(Date.now());
    }, 1000);
    return () => clearInterval(timer);
  }, [isOpen, enrollToken]);

  if (!isOpen) {
    return null;
  }

  const handleEnrollSubmit = async (e?: React.FormEvent) => {
    e?.preventDefault();
    setError(null);
    setEnrolling(true);
    setCopied(false);
    const trimmedLabel = labelInput.trim() || undefined;
    try {
      let tokenRes: NodeEnrollTokenResponse;
      if (onCreateEnrollToken) {
        tokenRes = await onCreateEnrollToken(trimmedLabel);
      } else if (apiClient) {
        tokenRes = await apiClient.createNodeEnrollToken(trimmedLabel);
      } else {
        const client = new HubApiClient();
        tokenRes = await client.createNodeEnrollToken(trimmedLabel);
      }
      setEnrollToken(tokenRes);
      setNowMs(Date.now());
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to generate enrollment token");
    } finally {
      setEnrolling(false);
    }
  };

  const handleCopyCommand = async () => {
    if (!enrollToken?.join_command) return;
    try {
      if (typeof navigator !== "undefined" && navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(enrollToken.join_command);
      }
      setCopied(true);
    } catch {
      setCopied(true);
    }
  };

  const handleRevoke = async (nodeId: string) => {
    setError(null);
    setRevokingId(nodeId);
    try {
      if (onRevokeNode) {
        await onRevokeNode(nodeId);
      } else if (apiClient) {
        await apiClient.revokeNode(nodeId);
      } else {
        const client = new HubApiClient();
        await client.revokeNode(nodeId);
      }
      if (!isControlled) {
        setLocalNodes((prev) => prev.filter((n) => n.node_id !== nodeId));
        if (apiClient) {
          await refreshLocalNodes();
        }
      } else if (onRefreshNodes) {
        await onRefreshNodes();
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to revoke node");
    } finally {
      setRevokingId(null);
    }
  };

  const remainingMs = enrollToken
    ? Math.max(0, enrollToken.expires_at_ms - nowMs)
    : 0;
  const countdownText = enrollToken
    ? remainingMs > 0
      ? formatRemainingTtl(remainingMs)
      : enrollToken.ttl_secs > 0
        ? "Expired"
        : "0s"
    : "";

  return (
    <div
      className="fd-modal-backdrop"
      role="dialog"
      aria-modal="true"
      aria-label="Nodes & Enrollment"
      data-testid="nodes-modal"
    >
      <div className="fd-modal" style={{ maxWidth: "680px", maxHeight: "88vh", display: "flex", flexDirection: "column" }}>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            marginBottom: "14px",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
            <h2 style={{ margin: 0, fontSize: "16px", fontWeight: 700 }}>
              Nodes &amp; Enrollment
            </h2>
            <span className="fd-badge">
              {effectiveNodes.filter((n) => n.status === "online").length} online / {effectiveNodes.length} total
            </span>
          </div>
          <button
            type="button"
            className="fd-btn"
            onClick={onClose}
            aria-label="Close Nodes Modal"
          >
            ✕
          </button>
        </div>

        {error && (
          <div
            role="alert"
            style={{
              marginBottom: "12px",
              padding: "8px 10px",
              borderRadius: "var(--radius-md)",
              backgroundColor: "rgba(239, 68, 68, 0.14)",
              border: "1px solid rgba(239, 68, 68, 0.45)",
              color: "#FCA5A5",
              fontSize: "12px",
            }}
          >
            {error}
          </div>
        )}

        <div
          style={{
            flex: 1,
            overflowY: "auto",
            display: "flex",
            flexDirection: "column",
            gap: "10px",
            marginBottom: "16px",
          }}
        >
          {loading && effectiveNodes.length === 0 ? (
            <div style={{ padding: "16px", textAlign: "center", color: "var(--text-muted)", fontSize: "12px" }}>
              Loading nodes...
            </div>
          ) : effectiveNodes.length === 0 ? (
            <div
              data-testid="nodes-empty-state"
              style={{
                padding: "18px 12px",
                textAlign: "center",
                color: "var(--text-muted)",
                fontSize: "12px",
                border: "1px dashed var(--border-subtle)",
                borderRadius: "var(--radius-md)",
              }}
            >
              No enrolled nodes reported yet. Use <strong>+ Enroll Node</strong> below to join a worker host.
            </div>
          ) : (
            effectiveNodes.map((node) => {
              const isHub = node.role === "hub";
              const isWorker = node.role === "worker";
              return (
                <div
                  key={node.node_id}
                  className="fd-card"
                  data-testid="enrolled-node-row"
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "8px",
                    padding: "10px 12px",
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
                    <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                      <span
                        className={getNodeStatusDotClass(node.status)}
                        title={`Status: ${node.status}`}
                        data-testid={`node-status-dot-${node.node_id}`}
                      />
                      <span style={{ fontWeight: 700, fontSize: "13px" }}>
                        {node.display_name || node.node_id}
                      </span>
                      {node.display_name && node.display_name !== node.node_id && (
                        <span className="fd-mono" style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                          ({node.node_id})
                        </span>
                      )}
                      <span
                        className="fd-badge"
                        style={
                          isHub
                            ? {
                                borderColor: "var(--border-accent)",
                                backgroundColor: "var(--accent-subtle)",
                                color: "var(--accent-hover)",
                              }
                            : undefined
                        }
                      >
                        {isHub ? "HUB" : "WORKER"}
                      </span>
                      <span className="fd-badge" style={{ textTransform: "uppercase" }}>
                        {node.status}
                      </span>
                    </div>

                    {isWorker && node.status !== "revoked" && (
                      <button
                        type="button"
                        className="fd-btn fd-btn-danger"
                        data-testid="revoke-node-btn"
                        disabled={revokingId === node.node_id}
                        onClick={() => void handleRevoke(node.node_id)}
                      >
                        {revokingId === node.node_id ? "Revoking..." : "Revoke"}
                      </button>
                    )}
                  </div>

                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: "8px",
                      flexWrap: "wrap",
                      fontSize: "11px",
                      color: "var(--text-secondary)",
                    }}
                  >
                    <span className="fd-badge fd-mono">
                      {node.os}/{node.arch}
                    </span>
                    {node.hostname && (
                      <span className="fd-mono" style={{ color: "var(--text-muted)" }}>
                        host: {node.hostname}
                      </span>
                    )}
                    {node.listen_addr && (
                      <span className="fd-badge fd-mono" title="Listen Address">
                        {node.listen_addr}
                      </span>
                    )}
                  </div>

                  {(node.capabilities?.length > 0 || node.labels?.length > 0) && (
                    <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                      {(node.capabilities ?? []).map((cap) => (
                        <span
                          key={`cap-${cap}`}
                          className={getAdapterBadgeClass(cap)}
                        >
                          {cap}
                        </span>
                      ))}
                      {(node.labels ?? []).map((lbl) => (
                        <span
                          key={`lbl-${lbl}`}
                          className="fd-badge"
                          style={{
                            borderColor: "var(--border-strong)",
                            color: "var(--text-primary)",
                          }}
                        >
                          #{lbl}
                        </span>
                      ))}
                    </div>
                  )}
                </div>
              );
            })
          )}
        </div>

        <div
          style={{
            borderTop: "1px solid var(--border-subtle)",
            paddingTop: "14px",
            display: "flex",
            flexDirection: "column",
            gap: "10px",
          }}
        >
          <form
            onSubmit={(e) => void handleEnrollSubmit(e)}
            style={{ display: "flex", gap: "8px", alignItems: "center", flexWrap: "wrap" }}
          >
            <input
              type="text"
              name="fd-enroll-node-label"
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              data-1p-ignore="true"
              data-lpignore="true"
              data-form-type="other"
              data-testid="enroll-node-label-input"
              className="fd-input fd-mono"
              placeholder="Optional node label (e.g. macos-xcode, gpu-worker)"
              value={labelInput}
              onChange={(e) => setLabelInput(e.target.value)}
              disabled={enrolling}
              style={{ flex: 1, minWidth: "200px" }}
            />
            <button
              type="submit"
              className="fd-btn fd-btn-primary"
              data-testid="enroll-node-btn"
              disabled={enrolling}
            >
              {enrolling ? "Generating..." : "+ Enroll Node"}
            </button>
          </form>

          {enrollToken && (
            <div
              className="fd-card"
              style={{
                backgroundColor: "var(--bg-elevated)",
                borderColor: "var(--border-accent)",
                display: "flex",
                flexDirection: "column",
                gap: "8px",
              }}
            >
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "8px",
                  flexWrap: "wrap",
                  fontSize: "11px",
                }}
              >
                <span style={{ fontWeight: 600, color: "var(--text-secondary)" }}>
                  One-Time Worker Join Command
                </span>
                <span className="fd-badge fd-mono" data-testid="enroll-token-expiry">
                  Expires in {countdownText}
                </span>
              </div>

              <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                <code
                  data-testid="enroll-join-command"
                  className="fd-mono"
                  style={{
                    flex: 1,
                    padding: "8px 10px",
                    borderRadius: "var(--radius-sm)",
                    backgroundColor: "var(--bg-input)",
                    border: "1px solid var(--border-subtle)",
                    fontSize: "12px",
                    overflowX: "auto",
                    whiteSpace: "nowrap",
                  }}
                >
                  {enrollToken.join_command}
                </code>
                <button
                  type="button"
                  className="fd-btn"
                  data-testid="copy-join-command-btn"
                  onClick={() => void handleCopyCommand()}
                >
                  {copied ? "Copied" : "Copy"}
                </button>
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
