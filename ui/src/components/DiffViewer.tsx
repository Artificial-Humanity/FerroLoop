import React from "react";

export type DiffLayoutMode = "unified" | "split";

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

export const DiffViewer: React.FC<DiffViewerProps> = ({
  diff,
  selectedFile = null,
  onSelectFile,
  defaultMode = "unified",
}) => {
  const [mode, setMode] = React.useState<DiffLayoutMode>(defaultMode);
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

  return (
    <div data-testid="diff-viewer" style={{ display: "flex", flexDirection: "column", gap: "12px" }}>
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
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
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

        <div style={{ display: "inline-flex", alignItems: "center", gap: "4px" }}>
          <button
            type="button"
            className={`fd-btn ${mode === "unified" ? "fd-btn-active" : ""}`}
            aria-pressed={mode === "unified"}
            onClick={() => setMode("unified")}
            disabled={visibleFiles.length === 0}
          >
            Unified
          </button>
          <button
            type="button"
            className={`fd-btn ${mode === "split" ? "fd-btn-active" : ""}`}
            aria-pressed={mode === "split"}
            onClick={() => setMode("split")}
            disabled={visibleFiles.length === 0}
          >
            Side-by-Side
          </button>
        </div>
      </div>

      {/* Diff Sections */}
      {visibleFiles.length === 0 ? (
        <div
          className="fd-card"
          data-testid="diff-empty-state"
          style={{
            textAlign: "center",
            padding: "28px 16px",
            color: "var(--text-muted)",
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

                            return (
                              <tr
                                key={lineIdx}
                                style={{
                                  backgroundColor: bg,
                                  color,
                                }}
                              >
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

                            return (
                              <tr key={pairIdx}>
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
