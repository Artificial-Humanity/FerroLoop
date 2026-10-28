import React from "react";
import type { HubApiClient } from "../api/client";
import type {
  PrMergeRequest,
  PrReviewRequest,
  PullRequestItem,
  WorktreeDiff,
  WorktreeFileStatus,
  WorktreeSummary,
} from "../types/api";
import { DiffViewer } from "./DiffViewer";

export type PrReviewPayload = PrReviewRequest;
export type PrMergePayload = PrMergeRequest;
export type GitSubView = "worktrees" | "prs";
export type GovernanceFormVerdict = "approved" | "changes_requested" | "commented";

export interface GitPrPaneProps {
  worktrees?: WorktreeSummary[];
  selectedWorktreeId?: string | null;
  worktreeDiff?: WorktreeDiff | null;
  prs?: PullRequestItem[];
  onSelectWorktree?: (id: string) => void;
  onRefreshWorktrees?: () => void;
  onReviewPr?: (number: number, payload: PrReviewPayload) => Promise<unknown> | void;
  onMergePr?: (number: number, payload: PrMergePayload) => Promise<unknown> | void;
  loading?: boolean;
  error?: string | null;
  apiClient?: HubApiClient;
  defaultSubView?: GitSubView;
}

const LEAD_AUTHORS = new Set(["kayla", "sonya", "ferris", "lucy", "penelope", "jarvis"]);

function inferRequiredTier(pr: PullRequestItem): "lead" | "lab_manager" | "owner" {
  const explicit =
    typeof pr.required_tier === "string"
      ? pr.required_tier.toLowerCase()
      : typeof pr.approval_tier === "string"
        ? pr.approval_tier.toLowerCase()
        : null;
  if (explicit === "owner" || explicit === "lab_manager" || explicit === "lead") {
    return explicit;
  }
  const project = (pr.project ?? "").toLowerCase();
  if (project.includes("artificialhumanity.io") || project === "website") {
    return "owner";
  }
  const author = (pr.author ?? "").toLowerCase();
  if (LEAD_AUTHORS.has(author)) {
    return "lab_manager";
  }
  return "lead";
}

function summarizeReviewStatus(pr: PullRequestItem): {
  label: string;
  tone: "approved" | "changes_requested" | "pending";
} {
  const reviews = pr.reviews ?? [];
  if (reviews.some((r) => r.state.toUpperCase().includes("CHANGES_REQUESTED"))) {
    return { label: "changes_requested", tone: "changes_requested" };
  }
  if (reviews.some((r) => r.state.toUpperCase().includes("APPROV"))) {
    return { label: "approved", tone: "approved" };
  }
  if (typeof pr.status === "string" && pr.status.trim()) {
    return { label: pr.status, tone: "pending" };
  }
  return { label: "review_required", tone: "pending" };
}

function normalizeStatusBadge(status: string): {
  code: string;
  color: string;
  bg: string;
  border: string;
} {
  const trimmed = status.trim().toUpperCase() || "M";
  if (trimmed === "A" || trimmed.startsWith("A")) {
    return {
      code: "A",
      color: "var(--diff-add-text)",
      bg: "var(--diff-add-bg)",
      border: "var(--diff-add-border)",
    };
  }
  if (trimmed === "D" || trimmed.startsWith("D")) {
    return {
      code: "D",
      color: "var(--diff-del-text)",
      bg: "var(--diff-del-bg)",
      border: "var(--diff-del-border)",
    };
  }
  if (trimmed === "??" || trimmed === "U") {
    return {
      code: "??",
      color: "var(--status-info)",
      bg: "rgba(56, 189, 248, 0.12)",
      border: "rgba(56, 189, 248, 0.35)",
    };
  }
  return {
    code: trimmed.slice(0, 2),
    color: "var(--accent-hover)",
    bg: "var(--accent-subtle)",
    border: "var(--border-accent)",
  };
}

export const GitPrPane: React.FC<GitPrPaneProps> = ({
  worktrees,
  selectedWorktreeId,
  worktreeDiff,
  prs,
  onSelectWorktree,
  onRefreshWorktrees,
  onReviewPr,
  onMergePr,
  loading = false,
  error = null,
  apiClient,
  defaultSubView = "worktrees",
}) => {
  const [subView, setSubView] = React.useState<GitSubView>(defaultSubView);
  const [selectedFile, setSelectedFile] = React.useState<string | null>(null);

  const [internalWorktrees, setInternalWorktrees] = React.useState<WorktreeSummary[]>([]);
  const [internalSelectedId, setInternalSelectedId] = React.useState<string | null>(null);
  const [internalDiff, setInternalDiff] = React.useState<WorktreeDiff | null>(null);
  const [internalPrs, setInternalPrs] = React.useState<PullRequestItem[]>([]);
  const [internalLoading, setInternalLoading] = React.useState<boolean>(false);
  const [internalError, setInternalError] = React.useState<string | null>(null);

  // Governance Form State
  const [formPrNumber, setFormPrNumber] = React.useState<string>("1");
  const [formProject, setFormProject] = React.useState<string>("ferroloop");
  const [formVerdict, setFormVerdict] = React.useState<GovernanceFormVerdict>("approved");
  const [formMergeMethod, setFormMergeMethod] = React.useState<"squash" | "merge" | "rebase">(
    "squash",
  );
  const [formBody, setFormBody] = React.useState<string>("");
  const [actionStatus, setActionStatus] = React.useState<string | null>(null);
  const [actionError, setActionError] = React.useState<string | null>(null);
  const [submittingAction, setSubmittingAction] = React.useState<boolean>(false);

  const effectiveWorktrees = worktrees ?? internalWorktrees;
  const effectiveSelectedId =
    selectedWorktreeId !== undefined
      ? selectedWorktreeId
      : (internalSelectedId ?? effectiveWorktrees[0]?.id ?? null);
  const effectiveDiff = worktreeDiff !== undefined ? worktreeDiff : internalDiff;
  const effectivePrs = prs ?? internalPrs;
  const effectiveLoading = loading || internalLoading;
  const effectiveError = error ?? internalError;

  // Reset selected file filter when switching worktree
  React.useEffect(() => {
    setSelectedFile(null);
  }, [effectiveSelectedId]);

  const fetchStandaloneData = React.useCallback(async () => {
    if (!apiClient) {
      return;
    }
    setInternalLoading(true);
    setInternalError(null);
    try {
      const [wtRes, prRes] = await Promise.all([
        worktrees === undefined ? apiClient.getWorktrees() : Promise.resolve({ worktrees }),
        prs === undefined ? apiClient.getPullRequests() : Promise.resolve({ prs }),
      ]);
      if (worktrees === undefined) {
        setInternalWorktrees(wtRes.worktrees ?? []);
        const firstId = wtRes.worktrees?.[0]?.id ?? null;
        setInternalSelectedId((prev) => prev ?? firstId);
      }
      if (prs === undefined) {
        setInternalPrs(prRes.prs ?? []);
      }
    } catch (err) {
      setInternalError(err instanceof Error ? err.message : "Failed to load git & PR state");
    } finally {
      setInternalLoading(false);
    }
  }, [apiClient, worktrees, prs]);

  React.useEffect(() => {
    void fetchStandaloneData();
  }, [fetchStandaloneData]);

  React.useEffect(() => {
    if (!apiClient || worktreeDiff !== undefined || !effectiveSelectedId) {
      return;
    }
    let cancelled = false;
    setInternalLoading(true);
    apiClient
      .getWorktreeDiff(effectiveSelectedId)
      .then((diffRes) => {
        if (!cancelled) {
          setInternalDiff(diffRes);
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setInternalError(err instanceof Error ? err.message : "Failed to load worktree diff");
        }
      })
      .finally(() => {
        if (!cancelled) {
          setInternalLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [apiClient, worktreeDiff, effectiveSelectedId]);

  const handleSelectWorktree = (id: string) => {
    setSelectedFile(null);
    if (onSelectWorktree) {
      onSelectWorktree(id);
    } else {
      setInternalSelectedId(id);
    }
  };

  const handleRefresh = () => {
    if (onRefreshWorktrees) {
      onRefreshWorktrees();
    } else {
      void fetchStandaloneData();
    }
  };

  const dispatchReview = async (prNumber: number, payload: PrReviewPayload) => {
    setSubmittingAction(true);
    setActionError(null);
    setActionStatus(null);
    try {
      if (onReviewPr) {
        await onReviewPr(prNumber, payload);
      } else if (apiClient) {
        await apiClient.reviewPullRequest(prNumber, payload);
      }
      setActionStatus(`Submitted review (${payload.verdict ?? "review"}) on PR #${prNumber}`);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : "PR review failed");
    } finally {
      setSubmittingAction(false);
    }
  };

  const dispatchMerge = async (prNumber: number, payload: PrMergePayload) => {
    setSubmittingAction(true);
    setActionError(null);
    setActionStatus(null);
    try {
      if (onMergePr) {
        await onMergePr(prNumber, payload);
      } else if (apiClient) {
        await apiClient.mergePullRequest(prNumber, payload);
      }
      setActionStatus(`Dispatched merge on PR #${prNumber}`);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : "PR merge failed");
    } finally {
      setSubmittingAction(false);
    }
  };

  const handleFormReviewSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    const parsedNum = Number.parseInt(formPrNumber, 10);
    if (!Number.isFinite(parsedNum) || parsedNum <= 0) {
      setActionError("Enter a valid positive PR number.");
      return;
    }

    const mappedVerdict =
      formVerdict === "approved"
        ? "approve"
        : formVerdict === "changes_requested"
          ? "request_changes"
          : "comment";

    const payload: PrReviewPayload = {
      verdict: mappedVerdict,
      approve: formVerdict === "approved",
      request_changes: formVerdict === "changes_requested",
      project: formProject.trim() || "ferroloop",
      body: formBody.trim() || undefined,
    };

    void dispatchReview(parsedNum, payload);
  };

  const handleFormMergeClick = () => {
    const parsedNum = Number.parseInt(formPrNumber, 10);
    if (!Number.isFinite(parsedNum) || parsedNum <= 0) {
      setActionError("Enter a valid positive PR number.");
      return;
    }

    const payload: PrMergePayload = {
      method: formMergeMethod,
      project: formProject.trim() || "ferroloop",
    };

    void dispatchMerge(parsedNum, payload);
  };

  const modifiedFiles: WorktreeFileStatus[] = effectiveDiff?.files ?? [];

  return (
    <section className="fd-pane" data-testid="git-pr-pane">
      {/* Top Sub-Navigation Toolbar */}
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
        <div style={{ display: "inline-flex", alignItems: "center", gap: "6px" }}>
          <button
            type="button"
            className={`fd-btn ${subView === "worktrees" ? "fd-btn-active" : ""}`}
            aria-pressed={subView === "worktrees"}
            onClick={() => setSubView("worktrees")}
          >
            Worktrees & Diffs ({effectiveWorktrees.length})
          </button>
          <button
            type="button"
            className={`fd-btn ${subView === "prs" ? "fd-btn-active" : ""}`}
            aria-pressed={subView === "prs"}
            onClick={() => setSubView("prs")}
            style={
              effectivePrs.length === 0 && subView !== "prs"
                ? { opacity: 0.65 }
                : undefined
            }
          >
            Pull Requests & Governance ({effectivePrs.length})
          </button>
        </div>

        <button
          type="button"
          className="fd-btn"
          onClick={handleRefresh}
          disabled={effectiveLoading}
        >
          {effectiveLoading ? "Refreshing..." : "Refresh"}
        </button>
      </div>

      {effectiveError && (
        <div
          role="alert"
          className="fd-card"
          style={{
            margin: "12px 16px 0",
            borderColor: "rgba(239, 68, 68, 0.45)",
            backgroundColor: "rgba(239, 68, 68, 0.12)",
            color: "#FCA5A5",
            fontSize: "12px",
          }}
        >
          {effectiveError}
        </div>
      )}

      {subView === "worktrees" ? (
        <div
          className="fd-scroll"
          style={{
            display: "grid",
            gridTemplateColumns: "minmax(260px, 320px) minmax(0, 1fr)",
            gap: "16px",
            alignItems: "start",
          }}
        >
          {/* Left Column: Worktree List & Modified Files */}
          <div style={{ display: "flex", flexDirection: "column", gap: "14px" }}>
            <div className="fd-card" style={{ padding: "10px 12px" }}>
              <div
                style={{
                  fontSize: "11px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.06em",
                  color: "var(--text-muted)",
                  marginBottom: "8px",
                }}
              >
                Discovered Worktrees
              </div>

              {effectiveWorktrees.length === 0 ? (
                <div style={{ fontSize: "12px", color: "var(--text-muted)", padding: "8px 0" }}>
                  No git worktrees discovered.
                </div>
              ) : (
                <div style={{ display: "flex", flexDirection: "column", gap: "6px" }}>
                  {effectiveWorktrees.map((wt) => {
                    const isSelected = wt.id === effectiveSelectedId;
                    const shortSha = (wt.head ?? "").slice(0, 7) || "HEAD";
                    const dirtyCount = wt.dirty_files?.length ?? 0;

                    return (
                      <button
                        key={wt.id}
                        type="button"
                        data-testid={`worktree-item-${wt.id}`}
                        onClick={() => handleSelectWorktree(wt.id)}
                        style={{
                          textAlign: "left",
                          padding: "8px 10px",
                          borderRadius: "var(--radius-md)",
                          border: `1px solid ${
                            isSelected ? "var(--border-accent)" : "var(--border-subtle)"
                          }`,
                          backgroundColor: isSelected
                            ? "var(--accent-subtle)"
                            : "var(--bg-elevated)",
                          color: "var(--text-primary)",
                          cursor: "pointer",
                          display: "flex",
                          flexDirection: "column",
                          gap: "4px",
                        }}
                      >
                        <div
                          style={{
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "space-between",
                            gap: "6px",
                          }}
                        >
                          <span style={{ fontWeight: 600, fontSize: "13px" }}>{wt.id}</span>
                          {wt.is_clean ? (
                            <span
                              className="fd-badge"
                              style={{
                                color: "var(--status-alive)",
                                borderColor: "rgba(16, 185, 129, 0.35)",
                                backgroundColor: "rgba(16, 185, 129, 0.1)",
                              }}
                            >
                              Clean
                            </span>
                          ) : (
                            <span
                              className="fd-badge"
                              style={{
                                color: "var(--accent-hover)",
                                borderColor: "var(--border-accent)",
                                backgroundColor: "var(--accent-subtle)",
                              }}
                            >
                              {dirtyCount} modified
                            </span>
                          )}
                        </div>

                        <div
                          className="fd-mono"
                          style={{
                            fontSize: "11px",
                            color: "var(--text-secondary)",
                            display: "flex",
                            alignItems: "center",
                            gap: "6px",
                            flexWrap: "wrap",
                          }}
                        >
                          <span>{wt.branch || "detached"}</span>
                          <span style={{ color: "var(--text-dim)" }}>@{shortSha}</span>
                        </div>

                        <div
                          className="fd-mono"
                          style={{
                            fontSize: "11px",
                            color: "var(--text-dim)",
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                          }}
                          title={wt.path}
                        >
                          {wt.path}
                        </div>
                      </button>
                    );
                  })}
                </div>
              )}
            </div>

            {/* Modified Files List for Selected Worktree */}
            {effectiveDiff && (
              <div className="fd-card" style={{ padding: "10px 12px" }}>
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    marginBottom: "8px",
                  }}
                >
                  <span
                    style={{
                      fontSize: "11px",
                      fontWeight: 700,
                      textTransform: "uppercase",
                      letterSpacing: "0.06em",
                      color: "var(--text-muted)",
                    }}
                  >
                    Modified Files ({modifiedFiles.length})
                  </span>
                </div>

                {modifiedFiles.length === 0 ? (
                  <div style={{ fontSize: "12px", color: "var(--text-muted)" }}>
                    Working tree is clean.
                  </div>
                ) : (
                  <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                    {modifiedFiles.map((file) => {
                      const badge = normalizeStatusBadge(file.status);
                      const active = selectedFile === file.path;

                      return (
                        <button
                          key={`${file.status}-${file.path}`}
                          type="button"
                          data-testid={`modified-file-${file.path}`}
                          onClick={() => setSelectedFile(active ? null : file.path)}
                          style={{
                            display: "flex",
                            alignItems: "center",
                            gap: "8px",
                            padding: "5px 8px",
                            borderRadius: "var(--radius-sm)",
                            border: `1px solid ${
                              active ? "var(--border-accent)" : "transparent"
                            }`,
                            backgroundColor: active
                              ? "var(--accent-subtle)"
                              : "var(--bg-elevated)",
                            color: "var(--text-primary)",
                            cursor: "pointer",
                            textAlign: "left",
                          }}
                        >
                          <span
                            className="fd-badge fd-mono"
                            style={{
                              color: badge.color,
                              backgroundColor: badge.bg,
                              borderColor: badge.border,
                              minWidth: "24px",
                              justifyContent: "center",
                            }}
                          >
                            {badge.code}
                          </span>
                          <span
                            className="fd-mono"
                            style={{
                              fontSize: "12px",
                              overflow: "hidden",
                              textOverflow: "ellipsis",
                              whiteSpace: "nowrap",
                            }}
                          >
                            {file.path}
                          </span>
                        </button>
                      );
                    })}
                  </div>
                )}
              </div>
            )}
          </div>

          {/* Right Column: DiffViewer */}
          <div style={{ minWidth: 0 }}>
            {effectiveDiff ? (
              <DiffViewer
                diff={effectiveDiff.diff}
                selectedFile={selectedFile}
                onSelectFile={setSelectedFile}
              />
            ) : (
              <div
                className="fd-card"
                style={{
                  textAlign: "center",
                  padding: "32px 16px",
                  color: "var(--text-muted)",
                }}
              >
                Select a worktree on the left to inspect its uncommitted diff.
              </div>
            )}
          </div>
        </div>
      ) : (
        <div
          className="fd-scroll"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "16px",
          }}
        >
          {(actionStatus || actionError) && (
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
              {actionError ?? actionStatus}
            </div>
          )}

          {/* Pull Requests List */}
          <div style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
            <div
              style={{
                fontSize: "12px",
                fontWeight: 700,
                textTransform: "uppercase",
                letterSpacing: "0.06em",
                color: "var(--text-muted)",
              }}
            >
              Tracked Pull Requests ({effectivePrs.length})
            </div>

            {effectivePrs.length === 0 ? (
              <div
                className="fd-card"
                data-testid="prs-empty-state"
                style={{
                  color: "var(--text-muted)",
                  fontSize: "13px",
                  padding: "18px 16px",
                }}
              >
                No runtime PR items currently cached in the local store. Use the PR Governance
                Action Form below to review or merge any PR directly by number.
              </div>
            ) : (
              effectivePrs.map((pr) => {
                const requiredTier = inferRequiredTier(pr);
                const reviewSummary = summarizeReviewStatus(pr);
                const shortSha = (pr.head_sha ?? "").slice(0, 7);

                return (
                  <div
                    key={pr.number}
                    data-testid={`pr-row-${pr.number}`}
                    className="fd-card"
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      gap: "10px",
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
                          gap: "8px",
                          flexWrap: "wrap",
                        }}
                      >
                        <span
                          className="fd-mono"
                          style={{
                            fontWeight: 700,
                            color: "var(--accent-hover)",
                            fontSize: "14px",
                          }}
                        >
                          #{pr.number}
                        </span>
                        <span style={{ fontWeight: 600, fontSize: "14px" }}>
                          {pr.title || `Pull Request #${pr.number}`}
                        </span>
                      </div>

                      <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
                        <span
                          className="fd-badge"
                          title="Required reviewer authority tier"
                          style={{
                            borderColor: "var(--border-accent)",
                            color: "var(--accent-hover)",
                          }}
                        >
                          {requiredTier}
                        </span>
                        <span
                          className="fd-badge"
                          style={{
                            color:
                              reviewSummary.tone === "approved"
                                ? "var(--diff-add-text)"
                                : reviewSummary.tone === "changes_requested"
                                  ? "var(--diff-del-text)"
                                  : "var(--text-secondary)",
                          }}
                        >
                          {reviewSummary.label}
                        </span>
                      </div>
                    </div>

                    <div
                      className="fd-mono"
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: "12px",
                        fontSize: "12px",
                        color: "var(--text-muted)",
                        flexWrap: "wrap",
                      }}
                    >
                      {pr.project && <span>project: {pr.project}</span>}
                      {pr.author && <span>author: {pr.author}</span>}
                      {pr.branch && <span>branch: {pr.branch}</span>}
                      {shortSha && <span>head: {shortSha}</span>}
                    </div>

                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: "8px",
                        flexWrap: "wrap",
                      }}
                    >
                      <button
                        type="button"
                        data-testid={`pr-approve-${pr.number}`}
                        className="fd-btn fd-btn-primary"
                        disabled={submittingAction}
                        onClick={() =>
                          void dispatchReview(pr.number, {
                            verdict: "approve",
                            approve: true,
                            project: pr.project,
                            head_sha: pr.head_sha,
                          })
                        }
                      >
                        Approve
                      </button>
                      <button
                        type="button"
                        data-testid={`pr-request-changes-${pr.number}`}
                        className="fd-btn fd-btn-danger"
                        disabled={submittingAction}
                        onClick={() =>
                          void dispatchReview(pr.number, {
                            verdict: "request_changes",
                            request_changes: true,
                            project: pr.project,
                            head_sha: pr.head_sha,
                          })
                        }
                      >
                        Request Changes
                      </button>
                      <button
                        type="button"
                        data-testid={`pr-merge-${pr.number}`}
                        className="fd-btn"
                        disabled={submittingAction}
                        onClick={() =>
                          void dispatchMerge(pr.number, {
                            method: "squash",
                            project: pr.project,
                            head_sha: pr.head_sha,
                          })
                        }
                      >
                        Merge PR
                      </button>
                    </div>
                  </div>
                );
              })
            )}
          </div>

          {/* Direct PR Governance Action Form */}
          <form
            onSubmit={handleFormReviewSubmit}
            data-testid="pr-governance-form"
            data-prs-empty={effectivePrs.length === 0 ? "true" : undefined}
            className="fd-card"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "12px",
              opacity: effectivePrs.length === 0 ? 0.45 : undefined,
            }}
          >
            <fieldset
              disabled={effectivePrs.length === 0}
              style={{
                border: "none",
                display: "flex",
                flexDirection: "column",
                gap: "12px",
                margin: 0,
                padding: 0,
                minWidth: 0,
              }}
            >
            <div style={{ fontWeight: 600, fontSize: "14px" }}>PR Governance Action Form</div>

            <div
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(auto-fit, minmax(160px, 1fr))",
                gap: "10px",
              }}
            >
              <label style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                <span style={{ fontSize: "12px", color: "var(--text-muted)" }}>PR Number</span>
                <input
                  type="number"
                  min={1}
                  aria-label="PR Number"
                  className="fd-input fd-mono"
                  value={formPrNumber}
                  onChange={(e) => setFormPrNumber(e.target.value)}
                />
              </label>

              <label style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                <span style={{ fontSize: "12px", color: "var(--text-muted)" }}>Project Slug</span>
                <input
                  type="text"
                  aria-label="Project Slug"
                  className="fd-input fd-mono"
                  value={formProject}
                  onChange={(e) => setFormProject(e.target.value)}
                />
              </label>

              <label style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                <span style={{ fontSize: "12px", color: "var(--text-muted)" }}>Review Verdict</span>
                <select
                  aria-label="Review Verdict"
                  className="fd-select"
                  value={formVerdict}
                  onChange={(e) => setFormVerdict(e.target.value as GovernanceFormVerdict)}
                >
                  <option value="approved">approved</option>
                  <option value="changes_requested">changes_requested</option>
                  <option value="commented">commented</option>
                </select>
              </label>

              <label style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                <span style={{ fontSize: "12px", color: "var(--text-muted)" }}>Merge Method</span>
                <select
                  aria-label="Merge Method"
                  className="fd-select"
                  value={formMergeMethod}
                  onChange={(e) =>
                    setFormMergeMethod(e.target.value as "squash" | "merge" | "rebase")
                  }
                >
                  <option value="squash">squash</option>
                  <option value="merge">merge</option>
                  <option value="rebase">rebase</option>
                </select>
              </label>
            </div>

            <label style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
              <span style={{ fontSize: "12px", color: "var(--text-muted)" }}>Review Body</span>
              <textarea
                rows={3}
                aria-label="Review Body"
                placeholder="Enter review notes or governance sign-off justification..."
                className="fd-textarea"
                value={formBody}
                onChange={(e) => setFormBody(e.target.value)}
              />
            </label>

            <div style={{ display: "flex", alignItems: "center", gap: "10px", flexWrap: "wrap" }}>
              <button
                type="submit"
                className="fd-btn fd-btn-primary"
                disabled={submittingAction}
              >
                Submit Review
              </button>
              <button
                type="button"
                className="fd-btn"
                disabled={submittingAction}
                onClick={handleFormMergeClick}
              >
                Merge Approved PR
              </button>
            </div>
            </fieldset>
          </form>
        </div>
      )}
    </section>
  );
};
