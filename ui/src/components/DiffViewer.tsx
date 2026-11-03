import React from "react";
import type { HubApiClient } from "../api/client";

export type DiffLayoutMode = "unified" | "split";
export type DiffCommentSide = "old" | "new" | "context";

export interface DiffInlineComment {
  id: string;
  filePath: string;
  hunkHeader: string;
  oldLine: number | null;
  newLine: number | null;
  side: DiffCommentSide;
  lineContent: string;
  comment: string;
}

export interface DiffReviewBatchPayload {
  summary: string;
  comments: DiffInlineComment[];
  formattedPrompt: string;
}

export interface ParsedDiffLine {
  type: "add" | "del" | "context";
  content: string;
  raw: string;
  oldLine: number | null;
  newLine: number | null;
}

export interface ParsedDiffHunk {
  header: string;
  oldStart: number;
  oldLines: number;
  newStart: number;
  newLines: number;
  lines: ParsedDiffLine[];
}

export interface ParsedDiffFile {
  oldPath: string;
  newPath: string;
  displayPath: string;
  meta: string[];
  hunks: ParsedDiffHunk[];
}

export interface DiffViewerProps {
  diff: string;
  selectedFile?: string | null;
  onSelectFile?: (path: string | null) => void;
  defaultMode?: DiffLayoutMode;
  sessionId?: string | null;
  agentName?: string | null;
  apiClient?: HubApiClient;
  onSubmitReview?: (
    batch: DiffReviewBatchPayload,
  ) => Promise<{ queued?: boolean; status?: string } | unknown> | void;
}

export function formatDiffReviewPrompt(
  comments: DiffInlineComment[],
  summary = "",
): string {
  const lines: string[] = ["## Inline Diff Review"];
  const trimmedSummary = summary.trim();
  if (trimmedSummary) {
    lines.push("", trimmedSummary);
  }

  const byFile = new Map<string, DiffInlineComment[]>();
  for (const item of comments) {
    const bucket = byFile.get(item.filePath) ?? [];
    bucket.push(item);
    byFile.set(item.filePath, bucket);
  }

  for (const [filePath, fileComments] of byFile.entries()) {
    lines.push("", `### \`${filePath}\``);
    for (const c of fileComments) {
      const lineNum = c.newLine ?? c.oldLine ?? 1;
      const sidePrefix =
        c.side === "new" ? "+" : c.side === "old" ? "-" : "context ";
      const snippet = c.lineContent.trim();
      const snippetLabel = snippet ? ` (\`${snippet}\`)` : "";
      lines.push(
        `- **Line ${lineNum} (${sidePrefix}L${lineNum})**${snippetLabel}: ${c.comment.trim()}`,
      );
    }
  }

  return lines.join("\n");
}

const HUNK_HEADER_RE = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$/;

export function parseUnifiedDiff(rawDiff: string): ParsedDiffFile[] {
  if (!rawDiff || !rawDiff.trim()) {
    return [];
  }

  const lines = rawDiff.replace(/\r\n/g, "\n").split("\n");
  // A diff ends with a newline, so split() yields a final "" that is not a
  // diff line; left in, it renders as a phantom numbered context row.
  if (lines.length > 0 && lines[lines.length - 1] === "") {
    lines.pop();
  }
  const files: ParsedDiffFile[] = [];
  let currentFile: ParsedDiffFile | null = null;
  let currentHunk: ParsedDiffHunk | null = null;
  let oldLineCursor = 0;
  let newLineCursor = 0;

  for (const line of lines) {
    if (line.startsWith("diff --git ")) {
      const match = /^diff --git a\/(.+?) b\/(.+)$/.exec(line);
      const oldPath = match ? match[1] : line.slice("diff --git ".length);
      const newPath = match ? match[2] : oldPath;
      currentFile = {
        oldPath,
        newPath,
        displayPath: newPath || oldPath,
        meta: [line],
        hunks: [],
      };
      files.push(currentFile);
      currentHunk = null;
      continue;
    }

    if (!currentFile) {
      if (!line.trim()) {
        continue;
      }
      currentFile = {
        oldPath: "worktree.diff",
        newPath: "worktree.diff",
        displayPath: "worktree.diff",
        meta: [],
        hunks: [],
      };
      files.push(currentFile);
    }

    const hunkMatch = HUNK_HEADER_RE.exec(line);
    if (hunkMatch) {
      const oldStart = Number.parseInt(hunkMatch[1], 10);
      const oldLines = hunkMatch[2] !== undefined ? Number.parseInt(hunkMatch[2], 10) : 1;
      const newStart = Number.parseInt(hunkMatch[3], 10);
      const newLines = hunkMatch[4] !== undefined ? Number.parseInt(hunkMatch[4], 10) : 1;
      oldLineCursor = oldStart;
      newLineCursor = newStart;
      currentHunk = {
        header: line,
        oldStart,
        oldLines,
        newStart,
        newLines,
        lines: [],
      };
      currentFile.hunks.push(currentHunk);
      continue;
    }

    if (!currentHunk) {
      if (line.startsWith("--- a/")) {
        currentFile.oldPath = line.slice("--- a/".length).trim();
      } else if (line.startsWith("+++ b/")) {
        const parsedNew = line.slice("+++ b/".length).trim();
        currentFile.newPath = parsedNew;
        currentFile.displayPath = parsedNew;
      }
      currentFile.meta.push(line);
      continue;
    }

    if (line.startsWith("\\ No newline at end of file")) {
      continue;
    }

    if (line.startsWith("+")) {
      currentHunk.lines.push({
        type: "add",
        content: line.slice(1),
        raw: line,
        oldLine: null,
        newLine: newLineCursor++,
      });
    } else if (line.startsWith("-")) {
      currentHunk.lines.push({
        type: "del",
        content: line.slice(1),
        raw: line,
        oldLine: oldLineCursor++,
        newLine: null,
      });
    } else if (line.startsWith(" ") || line === "") {
      currentHunk.lines.push({
        type: "context",
        content: line.startsWith(" ") ? line.slice(1) : line,
        raw: line.startsWith(" ") ? line : ` ${line}`,
        oldLine: oldLineCursor++,
        newLine: newLineCursor++,
      });
    }
  }

  return files;
}

interface SplitPair {
  left: ParsedDiffLine | null;
  right: ParsedDiffLine | null;
}

function buildSplitPairs(lines: ParsedDiffLine[]): SplitPair[] {
  const pairs: SplitPair[] = [];
  let idx = 0;

  while (idx < lines.length) {
    const line = lines[idx];
    if (line.type === "context") {
      pairs.push({ left: line, right: line });
      idx += 1;
      continue;
    }

    const deletions: ParsedDiffLine[] = [];
    const additions: ParsedDiffLine[] = [];
    while (idx < lines.length && lines[idx].type === "del") {
      deletions.push(lines[idx]);
      idx += 1;
    }
    while (idx < lines.length && lines[idx].type === "add") {
      additions.push(lines[idx]);
      idx += 1;
    }

    const maxLen = Math.max(deletions.length, additions.length);
    for (let i = 0; i < maxLen; i += 1) {
      pairs.push({
        left: deletions[i] ?? null,
        right: additions[i] ?? null,
      });
    }
  }

  return pairs;
}

interface ActiveInlineEditorTarget {
  lineKey: string;
  filePath: string;
  hunkHeader: string;
  oldLine: number | null;
  newLine: number | null;
  side: DiffCommentSide;
  lineContent: string;
  editingCommentId?: string;
}

function lineSideFromType(type: ParsedDiffLine["type"]): DiffCommentSide {
  if (type === "add") return "new";
  if (type === "del") return "old";
  return "context";
}

function makeLineKey(
  filePath: string,
  side: DiffCommentSide,
  oldLine: number | null,
  newLine: number | null,
): string {
  return `${filePath}::${side}::${oldLine ?? "-"}::${newLine ?? "-"}`;
}

export const DiffViewer: React.FC<DiffViewerProps> = ({
  diff,
  selectedFile = null,
  onSelectFile,
  defaultMode = "unified",
  sessionId = null,
  agentName = null,
  apiClient,
  onSubmitReview,
}) => {
  const [mode, setMode] = React.useState<DiffLayoutMode>(defaultMode);
  const [comments, setComments] = React.useState<DiffInlineComment[]>([]);
  const [reviewSummary, setReviewSummary] = React.useState<string>("");
  const [activeEditor, setActiveEditor] =
    React.useState<ActiveInlineEditorTarget | null>(null);
  const [editorText, setEditorText] = React.useState<string>("");
  const [draftPanelOpen, setDraftPanelOpen] = React.useState<boolean>(false);
  const [submittingReview, setSubmittingReview] = React.useState<boolean>(false);
  const [reviewStatus, setReviewStatus] = React.useState<string | null>(null);
  const [reviewError, setReviewError] = React.useState<string | null>(null);
  const nextIdRef = React.useRef<number>(1);

  const parsedFiles = React.useMemo(() => parseUnifiedDiff(diff), [diff]);

  const visibleFiles = React.useMemo(() => {
    if (!selectedFile) {
      return parsedFiles;
    }
    const normalized = selectedFile.trim();
    return parsedFiles.filter(
      (f) =>
        f.displayPath === normalized ||
        f.newPath === normalized ||
        f.oldPath === normalized ||
        f.displayPath.endsWith(`/${normalized}`),
    );
  }, [parsedFiles, selectedFile]);

  const totalAdditions = React.useMemo(
    () =>
      visibleFiles.reduce(
        (acc, file) =>
          acc +
          file.hunks.reduce(
            (hAcc, h) => hAcc + h.lines.filter((l) => l.type === "add").length,
            0,
          ),
        0,
      ),
    [visibleFiles],
  );

  const totalDeletions = React.useMemo(
    () =>
      visibleFiles.reduce(
        (acc, file) =>
          acc +
          file.hunks.reduce(
            (hAcc, h) => hAcc + h.lines.filter((l) => l.type === "del").length,
            0,
          ),
        0,
      ),
    [visibleFiles],
  );

  const openLineCommentEditor = (
    filePath: string,
    hunkHeader: string,
    line: ParsedDiffLine,
    overrideSide?: DiffCommentSide,
    existingComment?: DiffInlineComment,
  ) => {
    const side = existingComment?.side ?? overrideSide ?? lineSideFromType(line.type);
    const oldLine = existingComment ? existingComment.oldLine : line.oldLine;
    const newLine = existingComment ? existingComment.newLine : line.newLine;
    const lineKey = makeLineKey(filePath, side, oldLine, newLine);
    setActiveEditor({
      lineKey,
      filePath,
      hunkHeader,
      oldLine,
      newLine,
      side,
      lineContent: existingComment ? existingComment.lineContent : line.content,
      editingCommentId: existingComment?.id,
    });
    setEditorText(existingComment?.comment ?? "");
    setReviewError(null);
  };

  const handleSaveInlineComment = () => {
    if (!activeEditor) return;
    const trimmed = editorText.trim();
    if (!trimmed) return;

    if (activeEditor.editingCommentId) {
      const targetId = activeEditor.editingCommentId;
      setComments((prev) =>
        prev.map((item) =>
          item.id === targetId ? { ...item, comment: trimmed } : item,
        ),
      );
    } else {
      const id = `diff-comment-${nextIdRef.current++}`;
      const created: DiffInlineComment = {
        id,
        filePath: activeEditor.filePath,
        hunkHeader: activeEditor.hunkHeader,
        oldLine: activeEditor.oldLine,
        newLine: activeEditor.newLine,
        side: activeEditor.side,
        lineContent: activeEditor.lineContent,
        comment: trimmed,
      };
      setComments((prev) => [...prev, created]);
      setDraftPanelOpen(true);
    }
    setActiveEditor(null);
    setEditorText("");
  };

  const handleDeleteComment = (commentId: string) => {
    setComments((prev) => prev.filter((item) => item.id !== commentId));
    if (activeEditor?.editingCommentId === commentId) {
      setActiveEditor(null);
      setEditorText("");
    }
  };

  const handleEditCommentFromDraft = (comment: DiffInlineComment) => {
    const lineKey = makeLineKey(
      comment.filePath,
      comment.side,
      comment.oldLine,
      comment.newLine,
    );
    setActiveEditor({
      lineKey,
      filePath: comment.filePath,
      hunkHeader: comment.hunkHeader,
      oldLine: comment.oldLine,
      newLine: comment.newLine,
      side: comment.side,
      lineContent: comment.lineContent,
      editingCommentId: comment.id,
    });
    setEditorText(comment.comment);
  };

  const handleSubmitReviewBatch = async () => {
    if (submittingReview) return;
    if (comments.length === 0 && !reviewSummary.trim()) return;

    const formattedPrompt = formatDiffReviewPrompt(comments, reviewSummary);
    const batch: DiffReviewBatchPayload = {
      summary: reviewSummary.trim(),
      comments: [...comments],
      formattedPrompt,
    };

    setSubmittingReview(true);
    setReviewError(null);
    setReviewStatus(null);
    try {
      let res: { queued?: boolean; status?: string } | unknown;
      if (onSubmitReview) {
        res = await onSubmitReview(batch);
      } else if (apiClient) {
        const targetId = (sessionId || agentName || "").trim();
        if (!targetId) {
          throw new Error("No active agent session selected to receive the review.");
        }
        res = await apiClient.sendSessionPrompt(targetId, {
          prompt: formattedPrompt,
        });
      }
      const typedRes = res as { queued?: boolean; status?: string } | undefined;
      const isQueued =
        typedRes?.queued === true || typedRes?.status === "queued";
      const countLabel = `${batch.comments.length} ${
        batch.comments.length === 1 ? "comment" : "comments"
      }`;
      setReviewStatus(
        isQueued
          ? `Queued review batch (${countLabel}) for busy session.`
          : `Dispatched review batch (${countLabel}) to agent session.`,
      );
      setComments([]);
      setReviewSummary("");
      setActiveEditor(null);
      setEditorText("");
    } catch (err) {
      setReviewError(
        err instanceof Error ? err.message : "Failed to submit diff review batch.",
      );
    } finally {
      setSubmittingReview(false);
    }
  };

  const renderInlineEditorAndComments = (
    filePath: string,
    hunkHeader: string,
    side: DiffCommentSide,
    oldLine: number | null,
    newLine: number | null,
    colSpan: number,
  ) => {
    const lineKey = makeLineKey(filePath, side, oldLine, newLine);
    const matchingComments = comments.filter(
      (c) =>
        makeLineKey(c.filePath, c.side, c.oldLine, c.newLine) === lineKey,
    );
    const isEditingHere = activeEditor?.lineKey === lineKey;

    if (matchingComments.length === 0 && !isEditingHere) {
      return null;
    }

    return (
      <>
        {matchingComments.map((item) => {
          const lineNum = item.newLine ?? item.oldLine ?? 1;
          const sideBadge =
            item.side === "new"
              ? `+L${lineNum}`
              : item.side === "old"
                ? `-L${lineNum}`
                : `L${lineNum}`;
          return (
            <tr
              key={item.id}
              data-testid={`diff-inline-comment-card-${item.id}`}
              style={{
                backgroundColor: "var(--bg-surface)",
                borderTop: "1px solid var(--border-subtle)",
                borderBottom: "1px solid var(--border-subtle)",
              }}
            >
              <td colSpan={colSpan} style={{ padding: "8px 12px" }}>
                <div
                  style={{
                    display: "flex",
                    alignItems: "flex-start",
                    justifyContent: "space-between",
                    gap: "8px",
                    flexWrap: "wrap",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      gap: "4px",
                      minWidth: 0,
                      flex: 1,
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
                      <span className="fd-badge fd-mono" style={{ fontSize: "10.5px" }}>
                        {item.filePath} {sideBadge}
                      </span>
                    </div>
                    <div
                      style={{
                        fontSize: "12px",
                        color: "var(--text-primary)",
                        whiteSpace: "pre-wrap",
                        overflowWrap: "anywhere",
                      }}
                    >
                      {item.comment}
                    </div>
                  </div>
                  <div style={{ display: "flex", alignItems: "center", gap: "4px" }}>
                    <button
                      type="button"
                      className="fd-btn"
                      data-testid={`diff-comment-edit-btn-${item.id}`}
                      onClick={() => handleEditCommentFromDraft(item)}
                      style={{ padding: "2px 8px", fontSize: "11px" }}
                    >
                      Edit
                    </button>
                    <button
                      type="button"
                      className="fd-btn"
                      data-testid={`diff-comment-delete-btn-${item.id}`}
                      onClick={() => handleDeleteComment(item.id)}
                      style={{ padding: "2px 8px", fontSize: "11px" }}
                    >
                      Delete
                    </button>
                  </div>
                </div>
              </td>
            </tr>
          );
        })}
        {isEditingHere && activeEditor && (
          <tr
            data-testid="diff-inline-comment-editor"
            style={{
              backgroundColor: "var(--bg-elevated)",
              borderTop: "1px solid var(--border-accent)",
              borderBottom: "1px solid var(--border-accent)",
            }}
          >
            <td colSpan={colSpan} style={{ padding: "10px 12px" }}>
              <div
                style={{
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
                  }}
                >
                  <span className="fd-badge fd-mono" style={{ fontSize: "11px" }}>
                    {filePath}{" "}
                    {activeEditor.side === "new"
                      ? `+L${activeEditor.newLine ?? 1}`
                      : activeEditor.side === "old"
                        ? `-L${activeEditor.oldLine ?? 1}`
                        : `L${activeEditor.newLine ?? activeEditor.oldLine ?? 1}`}
                  </span>
                  <span style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                    {hunkHeader}
                  </span>
                </div>
                <textarea
                  aria-label="Inline diff comment"
                  data-testid="diff-inline-comment-input"
                  className="fd-input"
                  rows={2}
                  placeholder="Write an inline review comment for this line..."
                  value={editorText}
                  onChange={(e) => setEditorText(e.target.value)}
                  style={{
                    width: "100%",
                    fontSize: "12px",
                    padding: "6px 8px",
                    boxSizing: "border-box",
                    resize: "vertical",
                  }}
                />
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "flex-end",
                    gap: "6px",
                  }}
                >
                  <button
                    type="button"
                    className="fd-btn"
                    data-testid="diff-inline-comment-cancel-btn"
                    onClick={() => {
                      setActiveEditor(null);
                      setEditorText("");
                    }}
                    style={{ padding: "4px 10px", fontSize: "11px" }}
                  >
                    Cancel
                  </button>
                  <button
                    type="button"
                    className="fd-btn fd-btn-primary"
                    data-testid="diff-inline-comment-save-btn"
                    disabled={!editorText.trim()}
                    onClick={handleSaveInlineComment}
                    style={{ padding: "4px 10px", fontSize: "11px" }}
                  >
                    {activeEditor.editingCommentId ? "Update Comment" : "Add to Review"}
                  </button>
                </div>
              </div>
            </td>
          </tr>
        )}
      </>
    );
  };

  return (
    <div
      data-testid="diff-viewer"
      style={{
        display: "flex",
        flexDirection: "column",
        gap: "12px",
        minWidth: 0,
        maxWidth: "100%",
        boxSizing: "border-box",
      }}
    >
      {/* DiffViewer Header Bar */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: "8px",
          flexWrap: "wrap",
          padding: "8px 12px",
          backgroundColor: "var(--bg-surface)",
          border: "1px solid var(--border-subtle)",
          borderRadius: "var(--radius-md)",
          minWidth: 0,
          maxWidth: "100%",
          boxSizing: "border-box",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap", minWidth: 0 }}>
          <span className="fd-badge">
            {visibleFiles.length} {visibleFiles.length === 1 ? "file" : "files"}
          </span>
          <span
            className="fd-badge"
            style={{
              color: "var(--diff-add-text)",
              borderColor: "var(--diff-add-border)",
              backgroundColor: "var(--diff-add-bg)",
            }}
          >
            +{totalAdditions}
          </span>
          <span
            className="fd-badge"
            style={{
              color: "var(--diff-del-text)",
              borderColor: "var(--diff-del-border)",
              backgroundColor: "var(--diff-del-bg)",
            }}
          >
            -{totalDeletions}
          </span>
          {comments.length > 0 && (
            <span
              className="fd-badge"
              data-testid="diff-review-count-badge"
              style={{
                backgroundColor: "var(--accent-subtle)",
                borderColor: "var(--border-accent)",
                color: "var(--accent-hover)",
              }}
            >
              {comments.length} {comments.length === 1 ? "comment" : "comments"}
            </span>
          )}
          {selectedFile && (
            <div style={{ display: "inline-flex", alignItems: "center", gap: "6px" }}>
              <span className="fd-badge fd-mono" data-testid="selected-file-pill">
                Filtered: {selectedFile}
              </span>
              {onSelectFile && (
                <button
                  type="button"
                  className="fd-btn"
                  onClick={() => onSelectFile(null)}
                  style={{ padding: "3px 8px", fontSize: "11px" }}
                >
                  All files
                </button>
              )}
            </div>
          )}
        </div>

        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "4px",
            flexWrap: "wrap",
            minWidth: 0,
          }}
        >
          <button
            type="button"
            data-testid="diff-review-toggle-btn"
            className={`fd-btn ${draftPanelOpen || comments.length > 0 ? "fd-btn-active" : ""}`}
            onClick={() => setDraftPanelOpen((prev) => !prev)}
            disabled={visibleFiles.length === 0 && comments.length === 0}
            style={{ padding: "4px 8px", fontSize: "11px" }}
          >
            Review Draft ({comments.length})
          </button>
          <button
            type="button"
            className={`fd-btn ${mode === "unified" ? "fd-btn-active" : ""}`}
            aria-pressed={mode === "unified"}
            onClick={() => setMode("unified")}
            disabled={visibleFiles.length === 0}
            style={{ padding: "4px 8px", fontSize: "11px" }}
          >
            Unified
          </button>
          <button
            type="button"
            className={`fd-btn ${mode === "split" ? "fd-btn-active" : ""}`}
            aria-pressed={mode === "split"}
            onClick={() => setMode("split")}
            disabled={visibleFiles.length === 0}
            style={{ padding: "4px 8px", fontSize: "11px" }}
          >
            Side-by-Side
          </button>
        </div>
      </div>

      {reviewStatus && (
        <div
          role="status"
          data-testid="diff-review-status-banner"
          className="fd-card"
          style={{
            padding: "8px 12px",
            fontSize: "12px",
            borderColor: "rgba(16, 185, 129, 0.45)",
            backgroundColor: "rgba(16, 185, 129, 0.12)",
            color: "var(--diff-add-text)",
          }}
        >
          {reviewStatus}
        </div>
      )}

      {reviewError && (
        <div
          role="alert"
          data-testid="diff-review-error-banner"
          className="fd-card"
          style={{
            padding: "8px 12px",
            fontSize: "12px",
            borderColor: "rgba(239, 68, 68, 0.45)",
            backgroundColor: "rgba(239, 68, 68, 0.12)",
            color: "#FCA5A5",
          }}
        >
          {reviewError}
        </div>
      )}

      {(draftPanelOpen || comments.length > 0) && (
        <div
          className="fd-card"
          data-testid="diff-review-draft-panel"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "10px",
            padding: "12px",
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
            <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
              <strong style={{ fontSize: "12px", color: "var(--text-primary)" }}>
                Pending Diff Review
              </strong>
              <span className="fd-badge" data-testid="diff-draft-panel-count">
                {comments.length} pending {comments.length === 1 ? "comment" : "comments"}
              </span>
            </div>
            {comments.length > 0 && (
              <button
                type="button"
                className="fd-btn"
                data-testid="diff-review-clear-btn"
                onClick={() => {
                  setComments([]);
                  setActiveEditor(null);
                  setEditorText("");
                }}
                style={{ padding: "3px 8px", fontSize: "11px" }}
              >
                Clear All
              </button>
            )}
          </div>

          {comments.length > 0 && (
            <div style={{ display: "flex", flexDirection: "column", gap: "6px" }}>
              {comments.map((item) => {
                const lineNum = item.newLine ?? item.oldLine ?? 1;
                const sideBadge =
                  item.side === "new"
                    ? `+L${lineNum}`
                    : item.side === "old"
                      ? `-L${lineNum}`
                      : `L${lineNum}`;
                return (
                  <div
                    key={item.id}
                    data-testid={`diff-draft-item-${item.id}`}
                    style={{
                      display: "flex",
                      alignItems: "flex-start",
                      justifyContent: "space-between",
                      gap: "8px",
                      padding: "6px 8px",
                      backgroundColor: "var(--bg-elevated)",
                      border: "1px solid var(--border-subtle)",
                      borderRadius: "var(--radius-sm)",
                    }}
                  >
                    <div
                      style={{
                        display: "flex",
                        flexDirection: "column",
                        gap: "2px",
                        minWidth: 0,
                        flex: 1,
                      }}
                    >
                      <span className="fd-mono" style={{ fontSize: "11px", fontWeight: 600 }}>
                        {item.filePath}:{lineNum} ({sideBadge})
                      </span>
                      <span
                        style={{
                          fontSize: "12px",
                          color: "var(--text-secondary)",
                          whiteSpace: "pre-wrap",
                          overflowWrap: "anywhere",
                        }}
                      >
                        {item.comment}
                      </span>
                    </div>
                    <div style={{ display: "flex", alignItems: "center", gap: "4px" }}>
                      <button
                        type="button"
                        className="fd-btn"
                        data-testid={`diff-draft-edit-btn-${item.id}`}
                        onClick={() => handleEditCommentFromDraft(item)}
                        style={{ padding: "2px 6px", fontSize: "11px" }}
                      >
                        Edit
                      </button>
                      <button
                        type="button"
                        className="fd-btn"
                        data-testid={`diff-draft-delete-btn-${item.id}`}
                        onClick={() => handleDeleteComment(item.id)}
                        style={{ padding: "2px 6px", fontSize: "11px" }}
                      >
                        Delete
                      </button>
                    </div>
                  </div>
                );
              })}
            </div>
          )}

          <div>
            <label
              htmlFor="diff-review-summary-input"
              style={{
                display: "block",
                fontSize: "11px",
                fontWeight: 600,
                color: "var(--text-muted)",
                marginBottom: "4px",
                textTransform: "uppercase",
              }}
            >
              Overall Review Summary (Optional)
            </label>
            <textarea
              id="diff-review-summary-input"
              aria-label="Overall review summary"
              data-testid="diff-review-summary-input"
              className="fd-input"
              rows={2}
              placeholder="Optional summary instructions for the agent..."
              value={reviewSummary}
              onChange={(e) => setReviewSummary(e.target.value)}
              style={{
                width: "100%",
                fontSize: "12px",
                padding: "6px 8px",
                boxSizing: "border-box",
                resize: "vertical",
              }}
            />
          </div>

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
              className="fd-btn fd-btn-primary"
              data-testid="diff-review-submit-btn"
              disabled={
                submittingReview ||
                (comments.length === 0 && !reviewSummary.trim())
              }
              onClick={() => void handleSubmitReviewBatch()}
              style={{ padding: "5px 12px", fontSize: "12px" }}
            >
              {submittingReview ? "Submitting Review..." : "Submit Review to Agent"}
            </button>
          </div>
        </div>
      )}

      {/* Diff Sections */}
      {visibleFiles.length === 0 ? (
        <div
          className="fd-card"
          data-testid="diff-empty-state"
          style={{
            textAlign: "center",
            padding: "24px 12px",
            color: "var(--text-muted)",
            minWidth: 0,
            maxWidth: "100%",
            boxSizing: "border-box",
            whiteSpace: "normal",
            overflowWrap: "anywhere",
          }}
        >
          {selectedFile
            ? `No diff hunks recorded for ${selectedFile}.`
            : "Working tree has no uncommitted diff against HEAD."}
        </div>
      ) : (
        visibleFiles.map((file) => {
          const fileAdditions = file.hunks.reduce(
            (acc, h) => acc + h.lines.filter((l) => l.type === "add").length,
            0,
          );
          const fileDeletions = file.hunks.reduce(
            (acc, h) => acc + h.lines.filter((l) => l.type === "del").length,
            0,
          );

          return (
            <div
              key={file.displayPath}
              data-testid={`diff-file-${file.displayPath}`}
              className="fd-card"
              style={{ padding: 0, overflow: "hidden" }}
            >
              {/* File Section Header */}
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  padding: "8px 12px",
                  backgroundColor: "var(--bg-elevated)",
                  borderBottom: "1px solid var(--border-subtle)",
                  gap: "8px",
                }}
              >
                <span className="fd-mono" style={{ fontWeight: 600, fontSize: "12px" }}>
                  {file.displayPath}
                </span>
                <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
                  <span
                    className="fd-mono"
                    style={{ fontSize: "11px", color: "var(--diff-add-text)" }}
                  >
                    +{fileAdditions}
                  </span>
                  <span
                    className="fd-mono"
                    style={{ fontSize: "11px", color: "var(--diff-del-text)" }}
                  >
                    -{fileDeletions}
                  </span>
                </div>
              </div>

              {/* Hunks */}
              <div style={{ overflowX: "auto" }}>
                {file.hunks.map((hunk, hunkIdx) => (
                  <div key={`${file.displayPath}-hunk-${hunkIdx}`}>
                    <div
                      className="fd-mono"
                      style={{
                        padding: "4px 12px",
                        fontSize: "11px",
                        color: "var(--status-info)",
                        backgroundColor: "rgba(56, 189, 248, 0.08)",
                        borderBottom: "1px solid var(--border-subtle)",
                        borderTop: hunkIdx > 0 ? "1px solid var(--border-subtle)" : "none",
                      }}
                    >
                      {hunk.header}
                    </div>

                    {mode === "unified" ? (
                      <table
                        data-testid="diff-unified-table"
                        className="fd-mono"
                        style={{
                          width: "100%",
                          borderCollapse: "collapse",
                          fontSize: "12px",
                          lineHeight: 1.45,
                        }}
                      >
                        <tbody>
                          {hunk.lines.map((line, lineIdx) => {
                            const bg =
                              line.type === "add"
                                ? "var(--diff-add-bg)"
                                : line.type === "del"
                                  ? "var(--diff-del-bg)"
                                  : "transparent";
                            const color =
                              line.type === "add"
                                ? "var(--diff-add-text)"
                                : line.type === "del"
                                  ? "var(--diff-del-text)"
                                  : "var(--text-secondary)";
                            const side = lineSideFromType(line.type);
                            const lineNum = line.newLine ?? line.oldLine ?? lineIdx + 1;

                            return (
                              <React.Fragment key={lineIdx}>
                                <tr
                                  data-testid={`diff-line-row-${file.displayPath}-${hunkIdx}-${lineIdx}`}
                                  onClick={() =>
                                    openLineCommentEditor(
                                      file.displayPath,
                                      hunk.header,
                                      line,
                                      side,
                                    )
                                  }
                                  style={{
                                    backgroundColor: bg,
                                    color,
                                    cursor: "pointer",
                                  }}
                                >
                                  <td
                                    style={{
                                      width: "26px",
                                      padding: "1px 4px",
                                      textAlign: "center",
                                      userSelect: "none",
                                      borderRight: "1px solid var(--border-subtle)",
                                    }}
                                  >
                                    <button
                                      type="button"
                                      aria-label={`Comment on ${file.displayPath} ${side} line ${lineNum}`}
                                      data-testid={`diff-line-comment-btn-${file.displayPath}-${side}-${lineNum}`}
                                      onClick={(e) => {
                                        e.stopPropagation();
                                        openLineCommentEditor(
                                          file.displayPath,
                                          hunk.header,
                                          line,
                                          side,
                                        );
                                      }}
                                      style={{
                                        border: "none",
                                        background: "transparent",
                                        color: "var(--text-muted)",
                                        cursor: "pointer",
                                        padding: "0 4px",
                                        fontSize: "11px",
                                        fontWeight: 700,
                                      }}
                                    >
                                      +
                                    </button>
                                  </td>
                                  <td
                                    style={{
                                      width: "44px",
                                      padding: "1px 8px",
                                      textAlign: "right",
                                      color: "var(--text-dim)",
                                      userSelect: "none",
                                      borderRight: "1px solid var(--border-subtle)",
                                    }}
                                  >
                                    {line.oldLine ?? ""}
                                  </td>
                                  <td
                                    style={{
                                      width: "44px",
                                      padding: "1px 8px",
                                      textAlign: "right",
                                      color: "var(--text-dim)",
                                      userSelect: "none",
                                      borderRight: "1px solid var(--border-subtle)",
                                    }}
                                  >
                                    {line.newLine ?? ""}
                                  </td>
                                  <td
                                    style={{
                                      padding: "1px 10px",
                                      whiteSpace: "pre",
                                    }}
                                  >
                                    {line.raw}
                                  </td>
                                </tr>
                                {renderInlineEditorAndComments(
                                  file.displayPath,
                                  hunk.header,
                                  side,
                                  line.oldLine,
                                  line.newLine,
                                  4,
                                )}
                              </React.Fragment>
                            );
                          })}
                        </tbody>
                      </table>
                    ) : (
                      <table
                        data-testid="diff-split-table"
                        className="fd-mono"
                        style={{
                          width: "100%",
                          borderCollapse: "collapse",
                          fontSize: "12px",
                          lineHeight: 1.45,
                        }}
                      >
                        <tbody>
                          {buildSplitPairs(hunk.lines).map((pair, pairIdx) => {
                            const leftBg =
                              pair.left?.type === "del" ? "var(--diff-del-bg)" : "transparent";
                            const leftColor =
                              pair.left?.type === "del"
                                ? "var(--diff-del-text)"
                                : "var(--text-secondary)";
                            const rightBg =
                              pair.right?.type === "add" ? "var(--diff-add-bg)" : "transparent";
                            const rightColor =
                              pair.right?.type === "add"
                                ? "var(--diff-add-text)"
                                : "var(--text-secondary)";
                            const targetLine = pair.right ?? pair.left;
                            const targetSide: DiffCommentSide = pair.right
                              ? lineSideFromType(pair.right.type)
                              : "old";

                            return (
                              <React.Fragment key={pairIdx}>
                                <tr
                                  data-testid={`diff-split-row-${file.displayPath}-${hunkIdx}-${pairIdx}`}
                                  onClick={() => {
                                    if (targetLine) {
                                      openLineCommentEditor(
                                        file.displayPath,
                                        hunk.header,
                                        targetLine,
                                        targetSide,
                                      );
                                    }
                                  }}
                                  style={{ cursor: targetLine ? "pointer" : "default" }}
                                >
                                  <td
                                    style={{
                                      width: "40px",
                                      padding: "1px 6px",
                                      textAlign: "right",
                                      color: "var(--text-dim)",
                                      userSelect: "none",
                                      backgroundColor: leftBg,
                                      borderRight: "1px solid var(--border-subtle)",
                                    }}
                                  >
                                    {pair.left?.oldLine ?? ""}
                                  </td>
                                  <td
                                    onClick={(e) => {
                                      if (pair.left) {
                                        e.stopPropagation();
                                        openLineCommentEditor(
                                          file.displayPath,
                                          hunk.header,
                                          pair.left,
                                          lineSideFromType(pair.left.type),
                                        );
                                      }
                                    }}
                                    style={{
                                      width: "50%",
                                      padding: "1px 8px",
                                      whiteSpace: "pre",
                                      backgroundColor: leftBg,
                                      color: leftColor,
                                      borderRight: "1px solid var(--border-subtle)",
                                    }}
                                  >
                                    {pair.left ? pair.left.raw : ""}
                                  </td>
                                  <td
                                    style={{
                                      width: "40px",
                                      padding: "1px 6px",
                                      textAlign: "right",
                                      color: "var(--text-dim)",
                                      userSelect: "none",
                                      backgroundColor: rightBg,
                                      borderRight: "1px solid var(--border-subtle)",
                                    }}
                                  >
                                    {pair.right?.newLine ?? ""}
                                  </td>
                                  <td
                                    onClick={(e) => {
                                      if (pair.right) {
                                        e.stopPropagation();
                                        openLineCommentEditor(
                                          file.displayPath,
                                          hunk.header,
                                          pair.right,
                                          lineSideFromType(pair.right.type),
                                        );
                                      }
                                    }}
                                    style={{
                                      width: "50%",
                                      padding: "1px 8px",
                                      whiteSpace: "pre",
                                      backgroundColor: rightBg,
                                      color: rightColor,
                                    }}
                                  >
                                    {pair.right ? pair.right.raw : ""}
                                  </td>
                                </tr>
                                {pair.left &&
                                  pair.left.type === "del" &&
                                  renderInlineEditorAndComments(
                                    file.displayPath,
                                    hunk.header,
                                    "old",
                                    pair.left.oldLine,
                                    pair.left.newLine,
                                    4,
                                  )}
                                {pair.right &&
                                  renderInlineEditorAndComments(
                                    file.displayPath,
                                    hunk.header,
                                    lineSideFromType(pair.right.type),
                                    pair.right.oldLine,
                                    pair.right.newLine,
                                    4,
                                  )}
                              </React.Fragment>
                            );
                          })}
                        </tbody>
                      </table>
                    )}
                  </div>
                ))}
              </div>
            </div>
          );
        })
      )}
    </div>
  );
};

export const DiffPane = DiffViewer;
