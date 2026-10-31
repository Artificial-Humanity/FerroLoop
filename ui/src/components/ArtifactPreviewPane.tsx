import React, { useCallback, useEffect, useMemo, useState } from "react";
import type { HubApiClient } from "../api/client";
import type {
  ArtifactKind,
  ArtifactPreviewResponse,
  ArtifactSummaryItem,
  ArtifactVersionInfo,
  DocxPreview,
  OfficePreview,
  PptxPreview,
  TranscriptTurn,
  XlsxPreview,
} from "../types/api";
import {
  MermaidDiagram,
  normalizeArtifactTarget,
  renderMermaidToSvgString,
  RichMarkdown,
} from "./RichMarkdown";
import { extractTurnAttachments } from "./TurnCard";
import { canAttachTokenTo, isSafeLinkUrl } from "../lib/safeUrl";

export interface ArtifactPreviewPaneProps {
  apiClient: HubApiClient;
  activePath?: string | null;
  recentPaths?: string[];
  sessionId?: string | null;
  turns?: TranscriptTurn[];
  /**
   * A structured .docx/.xlsx/.pptx preview supplied directly. When set, the pane renders it
   * and does not ask the hub for one.
   */
  officePreview?: OfficePreview | null;
  onSelectPath?: (path: string) => void;
  onClose?: () => void;
}

export type ArtifactFilterMode = "user-docs" | "all";

const USER_DOC_EXTENSIONS = new Set([
  ".md",
  ".markdown",
  ".mmd",
  ".mermaid",
  ".png",
  ".jpg",
  ".jpeg",
  ".webp",
  ".gif",
  ".svg",
  ".pdf",
  ".docx",
  ".xlsx",
  ".pptx",
]);

export function isUserFacingArtifactPath(rawPath: string): boolean {
  const clean = rawPath.trim().split(/[?#]/)[0].toLowerCase();
  const dotIdx = clean.lastIndexOf(".");
  if (dotIdx < 0) {
    return false;
  }
  return USER_DOC_EXTENSIONS.has(clean.slice(dotIdx));
}

export function formatArtifactKindLabel(kind: ArtifactKind): string {
  switch (kind) {
    case "markdown":
      return "Markdown";
    case "mermaid":
      return "Mermaid";
    case "svg":
      return "SVG";
    case "image":
      return "Image";
    case "pdf":
      return "PDF";
    case "code":
      return "Code";
    case "binary":
    default:
      return "Binary";
  }
}

export function formatByteSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "0 B";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
}

export function discoverArtifactsFromTurns(turns: TranscriptTurn[] = []): string[] {
  const discovered: string[] = [];
  const seen = new Set<string>();
  const add = (raw: string) => {
    const clean = normalizeArtifactTarget(raw);
    if (clean && !seen.has(clean)) {
      seen.add(clean);
      discovered.push(clean);
    }
  };

  const fileUrlRegex = /file:\/\/([^\s)\]`"'>]+)/gi;
  for (let i = turns.length - 1; i >= 0; i--) {
    const content = turns[i].content || "";
    const { attachments } = extractTurnAttachments(content);
    for (const att of attachments) {
      if (att.path) add(att.path);
      else if (att.url) add(att.url);
    }
    let m: RegExpExecArray | null = fileUrlRegex.exec(content);
    while (m !== null) {
      add(`file://${m[1]}`);
      m = fileUrlRegex.exec(content);
    }
  }
  return discovered;
}

/* ------------------------------------------------------------------ */
/* Office documents (.docx / .xlsx / .pptx)                            */
/* ------------------------------------------------------------------ */

const TEXT_ARTIFACT_KINDS = new Set<ArtifactKind>(["markdown", "mermaid", "svg", "code"]);

export type OfficeExt = "docx" | "xlsx" | "pptx";

export function officeExtOf(ext: string | undefined, name?: string): OfficeExt | null {
  const fromExt = (ext ?? "").trim().toLowerCase().replace(/^\./, "");
  const fromName = (name ?? "").toLowerCase().split(".").pop() ?? "";
  const e = fromExt || fromName;
  return e === "docx" || e === "xlsx" || e === "pptx" ? e : null;
}

const OFFICE_LABELS: Record<OfficeExt, string> = {
  docx: "Word",
  xlsx: "Excel",
  pptx: "PowerPoint",
};

/** Spreadsheet column letters: 0 -> A, 25 -> Z, 26 -> AA. */
export function spreadsheetColumnLabel(index: number): string {
  let n = index + 1;
  let out = "";
  while (n > 0) {
    const rem = (n - 1) % 26;
    out = String.fromCharCode(65 + rem) + out;
    n = Math.floor((n - 1) / 26);
  }
  return out;
}

const docxHeadingSizes = [26, 22, 19, 17, 15, 14];

export const OfficeDocxView: React.FC<{ preview: DocxPreview }> = ({ preview }) => (
  <div
    data-testid="artifact-office-docx"
    style={{
      background: "#ffffff",
      color: "#1f2328",
      fontFamily: 'Georgia, "Times New Roman", serif',
      fontSize: "14px",
      lineHeight: 1.65,
      padding: "40px 52px",
      borderRadius: "var(--radius-md)",
      boxShadow: "0 2px 14px rgba(0,0,0,0.35)",
      maxWidth: "820px",
      width: "100%",
      margin: "0 auto",
    }}
  >
    {preview.blocks.length === 0 && (
      <div style={{ color: "#6b7280", fontStyle: "italic" }}>This document has no text.</div>
    )}
    {preview.blocks.map((block, i) => {
      if (block.type === "heading") {
        const level = Math.min(6, Math.max(1, block.level));
        const Tag = `h${level}` as "h1";
        return (
          <Tag
            key={i}
            data-testid="artifact-docx-heading"
            data-level={level}
            style={{
              fontSize: `${docxHeadingSizes[level - 1]}px`,
              margin: "1.1em 0 0.4em",
              lineHeight: 1.25,
              fontWeight: 700,
            }}
          >
            {block.text}
          </Tag>
        );
      }
      if (block.type === "table") {
        return (
          <table
            key={i}
            data-testid="artifact-docx-table"
            style={{ borderCollapse: "collapse", margin: "12px 0", width: "100%" }}
          >
            <tbody>
              {block.rows.map((row, r) => (
                <tr key={r}>
                  {row.map((cell, c) => (
                    <td
                      key={c}
                      style={{
                        border: "1px solid #c9ced6",
                        padding: "6px 10px",
                        verticalAlign: "top",
                        whiteSpace: "pre-wrap",
                      }}
                    >
                      {cell}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        );
      }
      return (
        <p
          key={i}
          data-testid="artifact-docx-paragraph"
          style={{ margin: "0 0 0.8em", whiteSpace: "pre-wrap" }}
        >
          {block.text}
        </p>
      );
    })}
    {preview.truncated && (
      <div style={{ color: "#6b7280", fontSize: "12px" }}>Preview truncated.</div>
    )}
  </div>
);

export const OfficeXlsxView: React.FC<{ preview: XlsxPreview }> = ({ preview }) => {
  const [active, setActive] = useState(0);
  const sheets = preview.sheets;
  const sheet = sheets[Math.min(active, Math.max(0, sheets.length - 1))];
  const cols = sheet
    ? Math.max(sheet.col_count ?? 0, ...sheet.rows.map((r) => r.length), 1)
    : 1;
  const cellStyle: React.CSSProperties = {
    border: "1px solid var(--border-subtle)",
    padding: "3px 8px",
    whiteSpace: "nowrap",
    fontSize: "12px",
    maxWidth: "320px",
    overflow: "hidden",
    textOverflow: "ellipsis",
  };
  const headStyle: React.CSSProperties = {
    ...cellStyle,
    background: "var(--bg-surface)",
    color: "var(--text-muted)",
    textAlign: "center",
    position: "sticky",
    top: 0,
    fontWeight: 600,
  };
  return (
    <div
      data-testid="artifact-office-xlsx"
      style={{ display: "flex", flexDirection: "column", gap: "8px", minHeight: 0 }}
    >
      <div role="tablist" style={{ display: "flex", gap: "4px", flexWrap: "wrap" }}>
        {sheets.map((s, i) => (
          <button
            key={`${s.name}-${i}`}
            type="button"
            role="tab"
            aria-selected={i === active}
            data-testid={`artifact-xlsx-tab-${i}`}
            className={`fd-btn${i === active ? " fd-btn-active" : ""}`}
            onClick={() => setActive(i)}
            style={{ fontSize: "11px", padding: "3px 10px" }}
          >
            {s.name || `Sheet${i + 1}`}
          </button>
        ))}
      </div>
      {sheet ? (
        <div
          className="fd-card"
          style={{ padding: 0, overflow: "auto", maxHeight: "68vh", background: "var(--bg-input)" }}
        >
          <table data-testid="artifact-xlsx-grid" style={{ borderCollapse: "collapse" }}>
            <thead>
              <tr>
                <th style={{ ...headStyle, left: 0, zIndex: 2 }} />
                {Array.from({ length: cols }, (_, c) => (
                  <th key={c} data-testid="artifact-xlsx-col-header" style={headStyle}>
                    {spreadsheetColumnLabel(c)}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {sheet.rows.map((row, r) => (
                <tr key={r}>
                  <th
                    data-testid="artifact-xlsx-row-header"
                    style={{ ...headStyle, position: "sticky", left: 0, top: "auto" }}
                  >
                    {r + 1}
                  </th>
                  {Array.from({ length: cols }, (_, c) => (
                    <td key={c} style={cellStyle} title={row[c] || undefined}>
                      {row[c] ?? ""}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
          {sheet.rows.length === 0 && (
            <div style={{ padding: "16px", color: "var(--text-muted)", fontSize: "12px" }}>
              This sheet is empty.
            </div>
          )}
        </div>
      ) : (
        <div className="fd-card" style={{ fontSize: "12px", color: "var(--text-muted)" }}>
          This workbook has no sheets.
        </div>
      )}
      {(sheet?.truncated || preview.truncated) && (
        <div style={{ fontSize: "11px", color: "var(--text-muted)" }}>
          Preview truncated: only the first rows and columns are shown.
        </div>
      )}
    </div>
  );
};

export const OfficePptxView: React.FC<{ preview: PptxPreview }> = ({ preview }) => {
  const [current, setCurrent] = useState(0);
  const slides = preview.slides;
  const count = slides.length;
  const idx = Math.min(current, Math.max(0, count - 1));
  const slide = slides[idx];
  if (!slide) {
    return (
      <div data-testid="artifact-office-pptx" className="fd-card" style={{ fontSize: "12px" }}>
        This presentation has no slides.
      </div>
    );
  }
  return (
    <div
      data-testid="artifact-office-pptx"
      style={{ display: "flex", gap: "12px", alignItems: "flex-start", flexWrap: "wrap" }}
    >
      <div
        data-testid="artifact-pptx-thumbs"
        style={{
          display: "flex",
          flexDirection: "column",
          gap: "6px",
          width: "132px",
          maxHeight: "68vh",
          overflowY: "auto",
        }}
      >
        {slides.map((s, i) => (
          <button
            key={i}
            type="button"
            data-testid={`artifact-pptx-thumb-${i}`}
            aria-label={`Go to slide ${i + 1}`}
            aria-current={i === idx ? "true" : undefined}
            className={`fd-btn${i === idx ? " fd-btn-active" : ""}`}
            onClick={() => setCurrent(i)}
            style={{
              textAlign: "left",
              padding: "6px",
              fontSize: "10px",
              lineHeight: 1.3,
              display: "block",
              aspectRatio: "16 / 9",
              overflow: "hidden",
            }}
          >
            <strong style={{ display: "block" }}>{i + 1}</strong>
            {s.title || `Slide ${i + 1}`}
          </button>
        ))}
      </div>
      <div style={{ flex: 1, minWidth: "260px", display: "flex", flexDirection: "column", gap: "8px" }}>
        <div
          data-testid="artifact-pptx-slide"
          style={{
            background: "#ffffff",
            color: "#1f2328",
            borderRadius: "var(--radius-md)",
            boxShadow: "0 2px 14px rgba(0,0,0,0.35)",
            aspectRatio: "16 / 9",
            padding: "5% 6%",
            overflow: "auto",
          }}
        >
          <h2 data-testid="artifact-pptx-title" style={{ margin: "0 0 0.6em", fontSize: "26px" }}>
            {slide.title || `Slide ${slide.index || idx + 1}`}
          </h2>
          <ul style={{ margin: 0, paddingLeft: "1.3em", fontSize: "16px", lineHeight: 1.5 }}>
            {slide.bullets.map((b, i) => (
              <li key={i} data-testid="artifact-pptx-bullet" style={{ whiteSpace: "pre-wrap" }}>
                {b}
              </li>
            ))}
          </ul>
        </div>
        <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
          <button
            type="button"
            data-testid="artifact-pptx-prev"
            className="fd-btn"
            disabled={idx === 0}
            onClick={() => setCurrent(idx - 1)}
          >
            ← Prev
          </button>
          <span data-testid="artifact-pptx-counter" className="fd-mono" style={{ fontSize: "12px" }}>
            Slide {idx + 1} / {count}
          </span>
          <button
            type="button"
            data-testid="artifact-pptx-next"
            className="fd-btn"
            disabled={idx >= count - 1}
            onClick={() => setCurrent(idx + 1)}
          >
            Next →
          </button>
        </div>
        <div
          data-testid="artifact-pptx-notes"
          className="fd-card"
          style={{ fontSize: "12px", whiteSpace: "pre-wrap" }}
        >
          <div style={{ fontWeight: 600, marginBottom: "4px" }}>Speaker notes</div>
          {slide.notes ? slide.notes : <span style={{ color: "var(--text-muted)" }}>No notes.</span>}
        </div>
      </div>
    </div>
  );
};

export const OfficePreviewView: React.FC<{ preview: OfficePreview }> = ({ preview }) => {
  switch (preview.kind) {
    case "docx":
      return <OfficeDocxView preview={preview} />;
    case "xlsx":
      return <OfficeXlsxView preview={preview} />;
    case "pptx":
      return <OfficePptxView preview={preview} />;
    default:
      return null;
  }
};

/* ------------------------------------------------------------------ */
/* Zoom / pan canvas                                                   */
/* ------------------------------------------------------------------ */

export const ZOOM_MIN = 25;
export const ZOOM_MAX = 400;
const ZOOM_STEPS = [25, 50, 75, 100, 125, 150, 200, 300, 400];
const ZOOM_PRESETS = [50, 100, 150, 200];

export function clampZoom(percent: number): number {
  if (!Number.isFinite(percent)) return 100;
  return Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, Math.round(percent)));
}

export function nextZoomStep(current: number, direction: 1 | -1): number {
  if (direction > 0) {
    return ZOOM_STEPS.find((s) => s > current) ?? ZOOM_MAX;
  }
  for (let i = ZOOM_STEPS.length - 1; i >= 0; i--) {
    if (ZOOM_STEPS[i] < current) return ZOOM_STEPS[i];
  }
  return ZOOM_MIN;
}

/**
 * Zoom and pan wrapper for images, SVGs and diagrams. The content is laid out at its fit size
 * and scaled with a CSS transform, so 100% is the fitted view; dragging pans.
 */
export const ZoomPanCanvas: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const [scale, setScale] = useState(100);
  const [offset, setOffset] = useState({ x: 0, y: 0 });
  const [dragging, setDragging] = useState(false);
  const dragRef = React.useRef<{ x: number; y: number; ox: number; oy: number } | null>(null);

  const zoomTo = (next: number) => setScale(clampZoom(next));

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: "8px", flex: 1 }}>
      <div
        data-testid="artifact-zoom-toolbar"
        style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}
      >
        <button
          type="button"
          data-testid="artifact-zoom-out"
          aria-label="Zoom Out"
          className="fd-btn"
          disabled={scale <= ZOOM_MIN}
          onClick={() => zoomTo(nextZoomStep(scale, -1))}
          style={{ fontSize: "11px", padding: "3px 8px" }}
        >
          Zoom Out
        </button>
        <button
          type="button"
          data-testid="artifact-zoom-in"
          aria-label="Zoom In"
          className="fd-btn"
          disabled={scale >= ZOOM_MAX}
          onClick={() => zoomTo(nextZoomStep(scale, 1))}
          style={{ fontSize: "11px", padding: "3px 8px" }}
        >
          Zoom In
        </button>
        {ZOOM_PRESETS.map((preset) => (
          <button
            key={preset}
            type="button"
            data-testid={`artifact-zoom-${preset}`}
            className={`fd-btn${scale === preset ? " fd-btn-active" : ""}`}
            onClick={() => zoomTo(preset)}
            style={{ fontSize: "11px", padding: "3px 8px" }}
          >
            {preset}%
          </button>
        ))}
        <button
          type="button"
          data-testid="artifact-zoom-fit"
          className="fd-btn"
          onClick={() => {
            setScale(100);
            setOffset({ x: 0, y: 0 });
          }}
          style={{ fontSize: "11px", padding: "3px 8px" }}
        >
          Fit
        </button>
        <span
          data-testid="artifact-zoom-badge"
          className="fd-badge fd-mono"
          aria-live="polite"
          title="Zoom level"
        >
          {scale}%
        </span>
      </div>
      <div
        data-testid="artifact-zoom-canvas"
        data-scale={scale}
        data-offset-x={offset.x}
        data-offset-y={offset.y}
        className="fd-card"
        onMouseDown={(e) => {
          if (e.button !== 0) return;
          dragRef.current = { x: e.clientX, y: e.clientY, ox: offset.x, oy: offset.y };
          setDragging(true);
        }}
        onMouseMove={(e) => {
          const d = dragRef.current;
          if (!d) return;
          setOffset({ x: d.ox + (e.clientX - d.x), y: d.oy + (e.clientY - d.y) });
        }}
        onMouseUp={() => {
          dragRef.current = null;
          setDragging(false);
        }}
        onMouseLeave={() => {
          dragRef.current = null;
          setDragging(false);
        }}
        onWheel={(e) => {
          if (e.ctrlKey || e.metaKey) {
            zoomTo(nextZoomStep(scale, e.deltaY < 0 ? 1 : -1));
          }
        }}
        style={{
          flex: 1,
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          overflow: "hidden",
          padding: "16px",
          backgroundColor: "var(--bg-input)",
          minHeight: "280px",
          cursor: dragging ? "grabbing" : "grab",
          userSelect: "none",
        }}
      >
        <div
          data-testid="artifact-zoom-content"
          style={{
            transform: `translate(${offset.x}px, ${offset.y}px) scale(${scale / 100})`,
            transformOrigin: "center center",
            transition: dragging ? "none" : "transform 80ms ease-out",
            maxWidth: "100%",
          }}
        >
          {children}
        </div>
      </div>
    </div>
  );
};

/* ------------------------------------------------------------------ */
/* Version history                                                     */
/* ------------------------------------------------------------------ */

export interface DiffRow {
  kind: "same" | "add" | "del" | "change";
  left: string | null;
  right: string | null;
  leftNo: number | null;
  rightNo: number | null;
}

function splitDiffLines(text: string): string[] {
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  if (lines.length > 0 && lines[lines.length - 1] === "") lines.pop();
  return lines;
}

const MAX_DIFF_CELLS = 4_000_000;

/** Line diff of two texts, aligned for a side-by-side view. */
export function diffLines(before: string, after: string): DiffRow[] {
  const a = splitDiffLines(before);
  const b = splitDiffLines(after);
  let start = 0;
  while (start < a.length && start < b.length && a[start] === b[start]) start++;
  let endA = a.length;
  let endB = b.length;
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA--;
    endB--;
  }
  const midA = a.slice(start, endA);
  const midB = b.slice(start, endB);
  const n = midA.length;
  const m = midB.length;

  type Op = { t: "=" | "-" | "+"; line: string };
  const ops: Op[] = [];
  if (n * m > MAX_DIFF_CELLS) {
    midA.forEach((line) => ops.push({ t: "-", line }));
    midB.forEach((line) => ops.push({ t: "+", line }));
  } else {
    const width = m + 1;
    const lcs = new Uint32Array((n + 1) * width);
    for (let i = n - 1; i >= 0; i--) {
      for (let j = m - 1; j >= 0; j--) {
        lcs[i * width + j] =
          midA[i] === midB[j]
            ? lcs[(i + 1) * width + j + 1] + 1
            : Math.max(lcs[(i + 1) * width + j], lcs[i * width + j + 1]);
      }
    }
    let i = 0;
    let j = 0;
    while (i < n && j < m) {
      if (midA[i] === midB[j]) {
        ops.push({ t: "=", line: midA[i] });
        i++;
        j++;
      } else if (lcs[(i + 1) * width + j] >= lcs[i * width + j + 1]) {
        ops.push({ t: "-", line: midA[i++] });
      } else {
        ops.push({ t: "+", line: midB[j++] });
      }
    }
    while (i < n) ops.push({ t: "-", line: midA[i++] });
    while (j < m) ops.push({ t: "+", line: midB[j++] });
  }

  const rows: DiffRow[] = [];
  let leftNo = 0;
  let rightNo = 0;
  const same = (line: string) => {
    leftNo++;
    rightNo++;
    rows.push({ kind: "same", left: line, right: line, leftNo, rightNo });
  };
  while (leftNo < start) same(a[leftNo]);
  for (let k = 0; k < ops.length; ) {
    if (ops[k].t === "=") {
      same(ops[k].line);
      k++;
      continue;
    }
    const dels: string[] = [];
    const adds: string[] = [];
    while (k < ops.length && ops[k].t !== "=") {
      (ops[k].t === "-" ? dels : adds).push(ops[k].line);
      k++;
    }
    for (let r = 0; r < Math.max(dels.length, adds.length); r++) {
      const left = r < dels.length ? dels[r] : null;
      const right = r < adds.length ? adds[r] : null;
      rows.push({
        kind: left !== null && right !== null ? "change" : left !== null ? "del" : "add",
        left,
        right,
        leftNo: left !== null ? ++leftNo : null,
        rightNo: right !== null ? ++rightNo : null,
      });
    }
  }
  for (let t = endA; t < a.length; t++) same(a[t]);
  return rows;
}

export function shortSha(sha: string): string {
  return sha.slice(0, 8);
}

export function formatVersionTime(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "unknown time";
  return new Date(ms).toISOString().replace("T", " ").replace(/\.\d+Z$/, " UTC");
}

const DIFF_COLORS = {
  del: "rgba(239, 68, 68, 0.18)",
  add: "rgba(34, 197, 94, 0.18)",
  change: "rgba(234, 179, 8, 0.16)",
} as const;

export const ArtifactDiffView: React.FC<{
  before: string;
  after: string;
  beforeLabel: string;
  afterLabel: string;
}> = ({ before, after, beforeLabel, afterLabel }) => {
  const rows = useMemo(() => diffLines(before, after), [before, after]);
  const changed = rows.filter((r) => r.kind !== "same").length;
  const cell = (
    no: number | null,
    text: string | null,
    bg: string | undefined,
  ): React.ReactNode[] => [
    <td
      key="n"
      style={{
        userSelect: "none",
        textAlign: "right",
        padding: "1px 8px",
        color: "var(--text-dim)",
        width: "1%",
        backgroundColor: bg,
      }}
    >
      {no ?? ""}
    </td>,
    <td
      key="t"
      style={{
        padding: "1px 10px",
        whiteSpace: "pre-wrap",
        wordBreak: "break-word",
        width: "49%",
        backgroundColor: bg,
        borderRight: "1px solid var(--border-subtle)",
      }}
    >
      {text ?? ""}
    </td>,
  ];
  return (
    <div data-testid="artifact-diff-view" className="fd-card fd-mono" style={{ padding: 0, overflowX: "auto", fontSize: "12px", lineHeight: 1.5 }}>
      <div
        data-testid="artifact-diff-summary"
        style={{ padding: "6px 10px", borderBottom: "1px solid var(--border-subtle)", color: "var(--text-muted)" }}
      >
        {changed === 0 ? "No differences" : `${changed} changed ${changed === 1 ? "line" : "lines"}`}
      </div>
      <table style={{ borderCollapse: "collapse", width: "100%", tableLayout: "fixed" }}>
        <thead>
          <tr>
            <th colSpan={2} style={{ textAlign: "left", padding: "4px 10px" }} data-testid="artifact-diff-left-label">
              {beforeLabel}
            </th>
            <th colSpan={2} style={{ textAlign: "left", padding: "4px 10px" }} data-testid="artifact-diff-right-label">
              {afterLabel}
            </th>
          </tr>
        </thead>
        <tbody>
          {rows.map((row, i) => (
            <tr key={i} data-testid="artifact-diff-row" data-kind={row.kind}>
              {cell(row.leftNo, row.left, row.kind === "del" || row.kind === "change" ? (row.kind === "del" ? DIFF_COLORS.del : DIFF_COLORS.change) : undefined)}
              {cell(row.rightNo, row.right, row.kind === "add" || row.kind === "change" ? (row.kind === "add" ? DIFF_COLORS.add : DIFF_COLORS.change) : undefined)}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
};

export const ArtifactPreviewPane: React.FC<ArtifactPreviewPaneProps> = ({
  apiClient,
  activePath = null,
  recentPaths = [],
  sessionId = null,
  turns = [],
  officePreview: officePreviewProp = null,
  onSelectPath,
  onClose,
}) => {
  const [pathInput, setPathInput] = useState<string>(activePath ?? "");
  const [selectedPath, setSelectedPath] = useState<string | null>(activePath);
  const [artifact, setArtifact] = useState<ArtifactPreviewResponse | null>(null);
  const [sessionArtifacts, setSessionArtifacts] = useState<ArtifactSummaryItem[]>([]);
  const [loading, setLoading] = useState<boolean>(false);
  const [error, setError] = useState<string | null>(null);
  const [viewMode, setViewMode] = useState<"rendered" | "source">("rendered");
  const [artifactFilter, setArtifactFilter] = useState<ArtifactFilterMode>("user-docs");
  const [pruning, setPruning] = useState<boolean>(false);
  const [pruneStatus, setPruneStatus] = useState<string | null>(null);

  useEffect(() => {
    if (activePath !== undefined && activePath !== null) {
      setSelectedPath(activePath);
      setPathInput(activePath.startsWith("mermaid://") ? "Inline Mermaid Diagram" : activePath);
    }
  }, [activePath]);

  const handleSelectPath = useCallback(
    (nextPath: string) => {
      const normalized = nextPath.startsWith("mermaid://")
        ? nextPath
        : normalizeArtifactTarget(nextPath);
      if (!normalized) return;
      setSelectedPath(normalized);
      setPathInput(
        normalized.startsWith("mermaid://") ? "Inline Mermaid Diagram" : normalized,
      );
      setViewMode("rendered");
      if (onSelectPath) {
        onSelectPath(normalized);
      }
    },
    [onSelectPath],
  );

  const sessionIdRef = React.useRef<string | null>(sessionId);
  sessionIdRef.current = sessionId;

  const refreshSessionArtifacts = useCallback(async () => {
    const requestedSession = sessionId;
    try {
      const res = await apiClient.listSessionArtifacts(requestedSession);
      // Ignore a response for a session the operator has since left.
      if (sessionIdRef.current !== requestedSession) {
        return;
      }
      if (Array.isArray(res?.artifacts)) {
        setSessionArtifacts(res.artifacts);
      }
    } catch {
      // Ignore optional discovery errors
    }
  }, [apiClient, sessionId]);

  // Discover session artifacts from backend when sessionId changes
  useEffect(() => {
    let cancelled = false;
    setSessionArtifacts([]);
    apiClient
      .listSessionArtifacts(sessionId)
      .then((res) => {
        if (!cancelled && Array.isArray(res?.artifacts)) {
          setSessionArtifacts(res.artifacts);
        }
      })
      .catch(() => {
        // Ignore optional discovery errors
      });
    return () => {
      cancelled = true;
    };
  }, [apiClient, sessionId]);

  const handlePruneDeadPaths = async () => {
    if (pruning) return;
    setPruning(true);
    setPruneStatus(null);
    try {
      const res = await apiClient.pruneArtifacts(
        sessionId ? { session_id: sessionId } : {},
      );
      const count =
        res?.pruned_count ??
        res?.pruned ??
        (Array.isArray(res?.pruned_paths) ? res.pruned_paths.length : 0);
      setPruneStatus(
        `Pruned ${count} dead ${count === 1 ? "path" : "paths"}`,
      );
      await refreshSessionArtifacts();
    } catch (err) {
      setPruneStatus(
        err instanceof Error ? err.message : "Failed to prune dead paths",
      );
    } finally {
      setPruning(false);
    }
  };

  const archivedItems = useMemo(
    () => sessionArtifacts.filter((item) => item.status === "archived"),
    [sessionArtifacts],
  );

  const archivedPathSet = useMemo(
    () => new Set(archivedItems.map((item) => item.path.trim())),
    [archivedItems],
  );

  const turnPaths = useMemo(() => discoverArtifactsFromTurns(turns), [turns]);

  const allDiscoveredPaths = useMemo(() => {
    const all: string[] = [];
    const seen = new Set<string>();
    const add = (p: string) => {
      const clean = p.trim();
      if (clean && !clean.startsWith("mermaid://") && !seen.has(clean)) {
        seen.add(clean);
        all.push(clean);
      }
    };
    if (selectedPath) add(selectedPath);
    recentPaths.forEach(add);
    turnPaths.forEach(add);
    sessionArtifacts.forEach((item) => add(item.path));
    return all;
  }, [selectedPath, recentPaths, turnPaths, sessionArtifacts]);

  const quickSwitchPaths = useMemo(() => {
    if (artifactFilter === "all") {
      return allDiscoveredPaths;
    }
    return allDiscoveredPaths.filter((p) => isUserFacingArtifactPath(p));
  }, [allDiscoveredPaths, artifactFilter]);

  // Auto-select the most recent discovered artifact if none is selected yet
  useEffect(() => {
    if (!selectedPath && quickSwitchPaths.length > 0) {
      const first = quickSwitchPaths[0];
      setSelectedPath(first);
      setPathInput(first);
    }
  }, [selectedPath, quickSwitchPaths]);

  // Load the active artifact preview
  useEffect(() => {
    if (!selectedPath) {
      setArtifact(null);
      setError(null);
      return;
    }

    if (selectedPath.startsWith("mermaid://")) {
      const rawSlice = selectedPath.slice("mermaid://".length);
      let code = rawSlice;
      try {
        code = decodeURIComponent(rawSlice);
      } catch {
        code = rawSlice;
      }
      const svgString = renderMermaidToSvgString(code);
      const svgDataUrl = `data:image/svg+xml;utf8,${encodeURIComponent(svgString)}`;
      const rawDataUrl = `data:text/plain;utf8,${encodeURIComponent(code)}`;
      setArtifact({
        path: "inline://diagram.mmd",
        name: "diagram.mmd",
        ext: "mmd",
        mime_type: "text/plain; charset=utf-8",
        size_bytes: code.length,
        kind: "mermaid",
        content: code,
        raw_url: rawDataUrl,
        download_url: svgDataUrl,
      });
      setError(null);
      setLoading(false);
      return;
    }

    let cancelled = false;
    // Drop the previous artifact right away: leaving it mounted while the new
    // one loads shows (and offers Open Raw / Download for) a file that does
    // not match the path now in the input.
    setArtifact(null);
    setLoading(true);
    setError(null);
    apiClient
      .getArtifactPreview(selectedPath)
      .then((res) => {
        if (!cancelled) {
          setArtifact(res);
          setViewMode("rendered");
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setArtifact(null);
          setError(err instanceof Error ? err.message : "Failed to load artifact preview");
        }
      })
      .finally(() => {
        if (!cancelled) {
          setLoading(false);
        }
      });

    return () => {
      cancelled = true;
    };
  }, [apiClient, selectedPath]);

  // Office documents: a structured preview from the hub, unless one was supplied directly.
  const officeExt = artifact ? officeExtOf(artifact.ext, artifact.name) : null;
  const [officeFetched, setOfficeFetched] = useState<OfficePreview | null>(null);
  const [officeLoading, setOfficeLoading] = useState<boolean>(false);
  const [officeError, setOfficeError] = useState<string | null>(null);

  useEffect(() => {
    setOfficeFetched(null);
    setOfficeError(null);
    setOfficeLoading(false);
    if (!artifact || !officeExt) return;
    const direct = officePreviewProp ?? artifact.office_preview ?? null;
    if (direct) {
      setOfficeFetched(direct);
      return;
    }
    if (typeof apiClient.getOfficePreview !== "function") return;
    let cancelled = false;
    setOfficeLoading(true);
    apiClient
      .getOfficePreview(artifact.path)
      .then((res) => {
        if (!cancelled) {
          if (res?.preview) setOfficeFetched(res.preview);
          else setOfficeError("The hub returned no document preview");
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setOfficeError(err instanceof Error ? err.message : "Failed to load document preview");
        }
      })
      .finally(() => {
        if (!cancelled) setOfficeLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [apiClient, artifact, officeExt, officePreviewProp]);

  // Version history of the loaded artifact.
  type VersionView =
    | { mode: "current" }
    | { mode: "version"; version: number }
    | { mode: "diff"; before: number; after: number };
  const versionPath =
    artifact && !artifact.path.startsWith("inline://") ? artifact.path : null;
  const [versions, setVersions] = useState<ArtifactVersionInfo[] | null>(null);
  const [historyOpen, setHistoryOpen] = useState<boolean>(false);
  const [versionView, setVersionView] = useState<VersionView>({ mode: "current" });
  const [compareSel, setCompareSel] = useState<number[]>([]);
  const [versionTexts, setVersionTexts] = useState<Record<number, string>>({});
  const [versionError, setVersionError] = useState<string | null>(null);
  const [versionLoading, setVersionLoading] = useState<boolean>(false);

  useEffect(() => {
    setVersions(null);
    setHistoryOpen(false);
    setVersionView({ mode: "current" });
    setCompareSel([]);
    setVersionTexts({});
    setVersionError(null);
    if (!versionPath || typeof apiClient.getArtifactVersions !== "function") return;
    let cancelled = false;
    apiClient
      .getArtifactVersions(versionPath)
      .then((res) => {
        if (!cancelled && Array.isArray(res?.versions)) setVersions(res.versions);
      })
      .catch(() => {
        // History is optional: an older hub has no such endpoint.
      });
    return () => {
      cancelled = true;
    };
  }, [apiClient, versionPath]);

  const toggleHistory = () => {
    const next = !historyOpen;
    setHistoryOpen(next);
    if (next && versionPath) {
      // Pick up revisions recorded since the artifact was loaded.
      apiClient
        .getArtifactVersions(versionPath)
        .then((res) => {
          if (Array.isArray(res?.versions)) setVersions(res.versions);
        })
        .catch(() => undefined);
    }
  };

  const wantedVersions = useMemo(
    () =>
      versionView.mode === "version"
        ? [versionView.version]
        : versionView.mode === "diff"
          ? [versionView.before, versionView.after]
          : [],
    [versionView],
  );

  useEffect(() => {
    // Only text artifacts are fetched as text; other kinds link to the raw revision.
    if (!versionPath || wantedVersions.length === 0) return;
    if (!artifact || !TEXT_ARTIFACT_KINDS.has(artifact.kind)) return;
    const missing = wantedVersions.filter((v) => versionTexts[v] === undefined);
    if (missing.length === 0) return;
    let cancelled = false;
    setVersionLoading(true);
    setVersionError(null);
    Promise.all(
      missing.map((v) =>
        apiClient.getArtifactVersionContent(versionPath, v).then((text) => [v, text] as const),
      ),
    )
      .then((pairs) => {
        if (!cancelled) {
          setVersionTexts((prev) => ({ ...prev, ...Object.fromEntries(pairs) }));
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setVersionError(err instanceof Error ? err.message : "Failed to load version content");
        }
      })
      .finally(() => {
        if (!cancelled) setVersionLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [apiClient, artifact, versionPath, wantedVersions, versionTexts]);

  const currentVersionNo = useMemo(
    () => versions?.find((v) => v.is_current)?.version ?? null,
    [versions],
  );

  const toggleCompare = (version: number) =>
    setCompareSel((prev) =>
      prev.includes(version)
        ? prev.filter((v) => v !== version)
        : prev.length >= 2
          ? [prev[1], version]
          : [...prev, version],
    );

  const startDiff = () => {
    if (compareSel.length === 2) {
      const [x, y] = [...compareSel].sort((p, q) => p - q);
      setVersionView({ mode: "diff", before: x, after: y });
    } else if (compareSel.length === 1 && currentVersionNo !== null && compareSel[0] !== currentVersionNo) {
      const [x, y] = [compareSel[0], currentVersionNo].sort((p, q) => p - q);
      setVersionView({ mode: "diff", before: x, after: y });
    }
  };

  const diffEnabled =
    compareSel.length === 2 ||
    (compareSel.length === 1 && currentVersionNo !== null && compareSel[0] !== currentVersionNo);

  /**
   * Resolve a hub-supplied artifact URL for use in href/src/iframe. Locally
   * generated `data:` URLs pass through. Anything that is not http(s) or
   * same-origin relative is dropped (`javascript:` in an iframe `src` would
   * run in the FerroDeck origin). The session token is appended only for the
   * hub's own origin: attaching it to a third-party URL would hand that host
   * a bearer credential.
   */
  const resolveHubUrl = useCallback(
    (candidate: string | undefined, fallback: () => string): string => {
      if (candidate && candidate.startsWith("data:")) return candidate;
      const base = candidate || fallback();
      if (!isSafeLinkUrl(base)) return "";
      const tok = apiClient.getToken();
      const hubBase =
        typeof apiClient.getBaseUrl === "function" ? apiClient.getBaseUrl() : "";
      if (tok && !base.includes("token=") && canAttachTokenTo(base, hubBase)) {
        const sep = base.includes("?") ? "&" : "?";
        return `${base}${sep}token=${encodeURIComponent(tok)}`;
      }
      return base;
    },
    [apiClient],
  );

  const rawUrl = useMemo(() => {
    if (!artifact) return "";
    return resolveHubUrl(artifact.raw_url, () =>
      apiClient.getArtifactRawUrl(artifact.path, false),
    );
  }, [artifact, apiClient, resolveHubUrl]);

  const downloadUrl = useMemo(() => {
    if (!artifact) return "";
    return resolveHubUrl(artifact.download_url, () =>
      apiClient.getArtifactRawUrl(artifact.path, true),
    );
  }, [artifact, apiClient, resolveHubUrl]);

  const imagePreviewSrc = useMemo(() => {
    if (!artifact) return "";
    if (artifact.kind === "svg" && artifact.content) {
      return `data:image/svg+xml;utf8,${encodeURIComponent(artifact.content)}`;
    }
    return rawUrl;
  }, [artifact, rawUrl]);

  const handleFormSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    if (pathInput.trim()) {
      handleSelectPath(pathInput.trim());
    }
  };

  const renderCodeLines = (content: string) => {
    const lines = content.replace(/\r\n/g, "\n").split("\n");
    return (
      <div
        data-testid="artifact-code-viewer"
        className="fd-card fd-mono"
        style={{
          padding: 0,
          overflowX: "auto",
          backgroundColor: "var(--bg-input)",
          fontSize: "12px",
          lineHeight: 1.5,
        }}
      >
        <table style={{ borderCollapse: "collapse", width: "100%" }}>
          <tbody>
            {lines.map((line, idx) => (
              <tr key={idx}>
                <td
                  style={{
                    userSelect: "none",
                    textAlign: "right",
                    padding: "1px 10px",
                    color: "var(--text-dim)",
                    borderRight: "1px solid var(--border-subtle)",
                    width: "1%",
                    whiteSpace: "nowrap",
                  }}
                >
                  {idx + 1}
                </td>
                <td
                  style={{
                    padding: "1px 12px",
                    color: "var(--text-primary)",
                    whiteSpace: "pre-wrap",
                    wordBreak: "break-word",
                  }}
                >
                  {line || " "}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    );
  };

  return (
    <section className="fd-pane" data-testid="artifact-preview-pane">
      {/* Top Bar: Path Input, Quick Switcher, Metadata & Download */}
      <div
        style={{
          display: "flex",
          flexDirection: "column",
          gap: "8px",
          padding: "10px 16px",
          borderBottom: "1px solid var(--border-subtle)",
          backgroundColor: "var(--bg-surface)",
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
          <div style={{ display: "flex", alignItems: "center", gap: "8px", minWidth: 0, flexWrap: "wrap" }}>
            <span
              data-testid="artifact-filename"
              style={{ fontWeight: 700, fontSize: "14px", color: "var(--text-primary)" }}
            >
              {artifact ? artifact.name : "Artifacts & Documents"}
            </span>
            {artifact && (
              <>
                <span
                  data-testid="artifact-kind-badge"
                  className="fd-badge"
                  style={{
                    color: "var(--status-info)",
                    borderColor: "var(--badge-assistant-border)",
                    backgroundColor: "var(--badge-assistant-bg)",
                  }}
                >
                  {officeExt ? OFFICE_LABELS[officeExt] : formatArtifactKindLabel(artifact.kind)}
                </span>
                <span className="fd-badge fd-mono">
                  {formatByteSize(artifact.size_bytes)}
                </span>
                <span
                  data-testid="artifact-path-badge"
                  className="fd-badge fd-mono"
                  title={artifact.path}
                  style={{
                    color: "var(--text-muted)",
                    maxWidth: "340px",
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                    whiteSpace: "nowrap",
                  }}
                >
                  {artifact.path}
                </span>
              </>
            )}
          </div>

          <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap", marginLeft: "auto" }}>
            {artifact && (
              <>
                {versions !== null && (
                  <button
                    type="button"
                    data-testid="artifact-history-toggle"
                    className={`fd-btn${historyOpen ? " fd-btn-active" : ""}`}
                    aria-expanded={historyOpen}
                    onClick={toggleHistory}
                    title="Show the recorded revisions of this artifact"
                  >
                    History ({versions.length})
                  </button>
                )}
                {(artifact.kind === "markdown" || artifact.kind === "svg") &&
                  typeof artifact.content === "string" && (
                    <div style={{ display: "inline-flex", gap: "4px" }}>
                      <button
                        type="button"
                        data-testid="artifact-view-rendered"
                        className={`fd-btn${viewMode === "rendered" ? " fd-btn-active" : ""}`}
                        onClick={() => setViewMode("rendered")}
                      >
                        Rendered
                      </button>
                      <button
                        type="button"
                        data-testid="artifact-view-source"
                        className={`fd-btn${viewMode === "source" ? " fd-btn-active" : ""}`}
                        onClick={() => setViewMode("source")}
                      >
                        Source
                      </button>
                    </div>
                  )}

                <a
                  href={rawUrl || undefined}
                  aria-disabled={!rawUrl}
                  target="_blank"
                  rel="noopener noreferrer"
                  data-testid="artifact-open-raw-btn"
                  className="fd-btn"
                  style={{ textDecoration: "none" }}
                >
                  Open Raw
                </a>

                <a
                  href={downloadUrl || undefined}
                  aria-disabled={!downloadUrl}
                  download={artifact.kind === "mermaid" && downloadUrl.startsWith("data:image/svg") ? "diagram.svg" : artifact.name}
                  data-testid="artifact-download-btn"
                  className="fd-btn fd-btn-primary"
                  style={{ textDecoration: "none" }}
                >
                  ↓ Download
                </a>
              </>
            )}
            {onClose && (
              <button
                type="button"
                data-testid="artifact-close-panel-btn"
                className="fd-btn"
                onClick={onClose}
                title="Close side panel"
                aria-label="Close side panel"
              >
                ✕
              </button>
            )}
          </div>
        </div>

        <form
          onSubmit={handleFormSubmit}
          style={{
            display: "flex",
            alignItems: "center",
            gap: "6px",
            flexWrap: "wrap",
          }}
        >
          <input
            type="text"
            data-testid="artifact-path-input"
            aria-label="Artifact file path"
            placeholder="Enter workspace or artifact file path (e.g. docs/plan.md, diagram.mmd, report.pdf)..."
            className="fd-input fd-mono"
            value={pathInput}
            onChange={(e) => setPathInput(e.target.value)}
            style={{ flex: 1, minWidth: "200px", fontSize: "12px", padding: "5px 9px" }}
          />
          <button
            type="submit"
            data-testid="artifact-load-btn"
            className="fd-btn"
            disabled={!pathInput.trim() || loading}
          >
            {loading ? "Loading..." : "Load"}
          </button>

          <select
            aria-label="Artifact filter mode"
            data-testid="artifact-filter-select"
            className="fd-select fd-mono"
            value={artifactFilter}
            onChange={(e) => setArtifactFilter(e.target.value as ArtifactFilterMode)}
            style={{ fontSize: "11px", padding: "5px 8px" }}
          >
            <option value="user-docs">User Documents &amp; Media</option>
            <option value="all">All Files</option>
          </select>

          <button
            type="button"
            data-testid="prune-artifacts-btn"
            className="fd-btn"
            disabled={pruning}
            onClick={() => void handlePruneDeadPaths()}
            title="Prune dead worktree artifact references and archive surviving copies"
          >
            {pruning ? "Pruning..." : "Prune Dead Paths"}
          </button>

          {pruneStatus && (
            <span
              role="status"
              data-testid="prune-artifacts-status"
              className="fd-badge fd-mono"
            >
              {pruneStatus}
            </span>
          )}

          {quickSwitchPaths.length > 0 && (
            <select
              aria-label="Recent and discovered session artifacts"
              data-testid="artifact-quick-switcher"
              className="fd-select fd-mono"
              value={
                selectedPath &&
                !selectedPath.startsWith("mermaid://") &&
                quickSwitchPaths.includes(selectedPath)
                  ? selectedPath
                  : ""
              }
              onChange={(e) => {
                if (e.target.value) {
                  handleSelectPath(e.target.value);
                }
              }}
              style={{ maxWidth: "260px", fontSize: "11px", padding: "5px 8px" }}
            >
              <option value="" disabled>
                Switch artifact ({quickSwitchPaths.length})...
              </option>
              {quickSwitchPaths.map((p) => {
                const label = p.split("/").pop() || p;
                const isArchived = archivedPathSet.has(p);
                return (
                  <option key={p} value={p}>
                    {label}
                    {isArchived ? " [Archived]" : ""} — {p}
                  </option>
                );
              })}
            </select>
          )}
        </form>

        {archivedItems.length > 0 && (
          <div
            style={{
              display: "flex",
              alignItems: "center",
              gap: "6px",
              flexWrap: "wrap",
            }}
          >
            {archivedItems.map((item) => (
              <button
                key={item.path}
                type="button"
                className={`fd-btn${selectedPath === item.path ? " fd-btn-active" : ""}`}
                onClick={() => handleSelectPath(item.path)}
                title={
                  item.original_path
                    ? `Preserved from removed worktree (${item.original_path})`
                    : `Preserved artifact (${item.path})`
                }
                style={{ fontSize: "11px", padding: "3px 8px" }}
              >
                <span className="fd-mono">{item.name}</span>
                <span
                  className="fd-badge"
                  data-testid="artifact-archived-badge"
                  style={{
                    borderColor: "var(--border-accent)",
                    backgroundColor: "var(--accent-subtle)",
                    color: "var(--accent-hover)",
                  }}
                >
                  Archived
                </span>
              </button>
            ))}
          </div>
        )}
      </div>

      {/* Main Preview Body */}
      <div
        className="fd-scroll"
        style={{
          display: "flex",
          flexDirection: "column",
          gap: "12px",
          flex: 1,
        }}
      >
        {error && (
          <div
            role="alert"
            data-testid="artifact-error-banner"
            className="fd-card"
            style={{
              borderColor: "rgba(239, 68, 68, 0.45)",
              backgroundColor: "rgba(239, 68, 68, 0.12)",
              color: "#FCA5A5",
              fontSize: "12px",
            }}
          >
            {error}
          </div>
        )}

        {!artifact && !loading && !error && (
          <div
            data-testid="artifact-empty-state"
            className="fd-card"
            style={{
              textAlign: "center",
              padding: "36px 20px",
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
              No artifact selected
            </div>
            <div style={{ fontSize: "12px", maxWidth: "460px", margin: "0 auto" }}>
              Click any file chip or attachment in Chat, select a discovered session artifact above,
              or enter a file path to preview Markdown, Mermaid diagrams, SVGs, images, PDFs, or source files.
            </div>
          </div>
        )}

        {artifact && (
          <>
            {historyOpen && versions && (
              <div
                data-testid="artifact-version-history"
                className="fd-card"
                style={{ display: "flex", flexDirection: "column", gap: "8px", fontSize: "12px" }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                  <strong>Version history</strong>
                  <button
                    type="button"
                    data-testid="artifact-version-current-btn"
                    className={`fd-btn${versionView.mode === "current" ? " fd-btn-active" : ""}`}
                    onClick={() => setVersionView({ mode: "current" })}
                  >
                    Current
                  </button>
                  <button
                    type="button"
                    data-testid="artifact-version-diff-btn"
                    className={`fd-btn${versionView.mode === "diff" ? " fd-btn-active" : ""}`}
                    disabled={!diffEnabled}
                    onClick={startDiff}
                    title="Select two versions (or one, to compare with the current version)"
                  >
                    Diff
                  </button>
                </div>
                {versions.length === 0 && (
                  <div style={{ color: "var(--text-muted)" }}>No versions recorded yet.</div>
                )}
                <ol
                  data-testid="artifact-version-list"
                  style={{ listStyle: "none", margin: 0, padding: 0, display: "flex", flexDirection: "column", gap: "4px" }}
                >
                  {[...versions].reverse().map((v) => (
                    <li
                      key={v.version}
                      data-testid={`artifact-version-row-${v.version}`}
                      style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}
                    >
                      <input
                        type="checkbox"
                        data-testid={`artifact-version-compare-${v.version}`}
                        aria-label={`Compare v${v.version}`}
                        checked={compareSel.includes(v.version)}
                        onChange={() => toggleCompare(v.version)}
                      />
                      <span className="fd-mono" style={{ fontWeight: 700 }}>
                        v{v.version}
                      </span>
                      {v.is_current && (
                        <span className="fd-badge" data-testid="artifact-version-current-badge">
                          Current
                        </span>
                      )}
                      <span className="fd-mono" title={v.sha256} style={{ color: "var(--text-muted)" }}>
                        {shortSha(v.sha256)}
                      </span>
                      <span className="fd-mono" style={{ color: "var(--text-muted)" }}>
                        {formatByteSize(v.size_bytes)}
                      </span>
                      <span className="fd-mono" style={{ color: "var(--text-dim)" }}>
                        {formatVersionTime(v.recorded_at_ms)}
                      </span>
                      <button
                        type="button"
                        data-testid={`artifact-version-view-${v.version}`}
                        className={`fd-btn${versionView.mode === "version" && versionView.version === v.version ? " fd-btn-active" : ""}`}
                        onClick={() => setVersionView({ mode: "version", version: v.version })}
                        style={{ fontSize: "11px", padding: "2px 8px" }}
                      >
                        View
                      </button>
                    </li>
                  ))}
                </ol>
              </div>
            )}

            {versionView.mode !== "current" && (
              <div
                data-testid="artifact-version-panel"
                style={{ display: "flex", flexDirection: "column", gap: "8px", flex: 1 }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                  <span className="fd-badge fd-mono" data-testid="artifact-version-banner">
                    {versionView.mode === "version"
                      ? `Viewing v${versionView.version}`
                      : `Comparing v${versionView.before} and v${versionView.after}`}
                  </span>
                  <button
                    type="button"
                    data-testid="artifact-version-back-btn"
                    className="fd-btn"
                    onClick={() => setVersionView({ mode: "current" })}
                  >
                    Back to Current
                  </button>
                </div>
                {versionError && (
                  <div role="alert" data-testid="artifact-version-error" className="fd-card" style={{ color: "#FCA5A5", fontSize: "12px" }}>
                    {versionError}
                  </div>
                )}
                {versionLoading && !versionError && (
                  <div data-testid="artifact-version-loading" style={{ fontSize: "12px", color: "var(--text-muted)" }}>
                    Loading version…
                  </div>
                )}
                {!versionError && versionView.mode === "version" && (() => {
                  const n = versionView.version;
                  if (TEXT_ARTIFACT_KINDS.has(artifact.kind)) {
                    const text = versionTexts[n];
                    return text === undefined ? null : (
                      <div data-testid="artifact-version-content">{renderCodeLines(text)}</div>
                    );
                  }
                  const rawVersionUrl = apiClient.getArtifactVersionRawUrl(artifact.path, n);
                  return (
                    <div data-testid="artifact-version-content" className="fd-card" style={{ display: "flex", flexDirection: "column", gap: "8px", alignItems: "center" }}>
                      {artifact.kind === "image" && (
                        <img
                          data-testid="artifact-version-image"
                          src={rawVersionUrl}
                          alt={`${artifact.name} v${n}`}
                          style={{ maxWidth: "100%", maxHeight: "60vh" }}
                        />
                      )}
                      <a href={rawVersionUrl} target="_blank" rel="noopener noreferrer" className="fd-btn" style={{ textDecoration: "none" }}>
                        Open raw v{n}
                      </a>
                    </div>
                  );
                })()}
                {!versionError && versionView.mode === "diff" && (() => {
                  if (!TEXT_ARTIFACT_KINDS.has(artifact.kind)) {
                    return (
                      <div data-testid="artifact-diff-unsupported" className="fd-card" style={{ fontSize: "12px" }}>
                        Diff is available for text artifacts only. Open each version to inspect it.
                      </div>
                    );
                  }
                  const before = versionTexts[versionView.before];
                  const after = versionTexts[versionView.after];
                  return before === undefined || after === undefined ? null : (
                    <ArtifactDiffView
                      before={before}
                      after={after}
                      beforeLabel={`v${versionView.before}`}
                      afterLabel={`v${versionView.after}`}
                    />
                  );
                })()}
              </div>
            )}

            {versionView.mode === "current" && (
          <>
            {artifact.kind === "markdown" && (
              viewMode === "source" ? (
                renderCodeLines(artifact.content ?? "")
              ) : (
                <div
                  data-testid="artifact-markdown-body"
                  className="fd-card"
                  style={{
                    padding: "16px 20px",
                    fontSize: "13.5px",
                    lineHeight: 1.6,
                  }}
                >
                  <RichMarkdown
                    text={artifact.content ?? ""}
                    onOpenArtifact={handleSelectPath}
                  />
                </div>
              )
            )}

            {artifact.kind === "mermaid" && (
              <ZoomPanCanvas key={artifact.path}>
                <MermaidDiagram
                  code={artifact.content ?? ""}
                  title={artifact.name}
                  onOpenArtifact={handleSelectPath}
                  showPreviewInArtifactsButton={false}
                />
              </ZoomPanCanvas>
            )}

            {(artifact.kind === "svg" || artifact.kind === "image") && (
              viewMode === "source" && artifact.kind === "svg" && artifact.content ? (
                renderCodeLines(artifact.content)
              ) : (
                <ZoomPanCanvas key={artifact.path}>
                  <img
                    data-testid="artifact-image-preview"
                    src={imagePreviewSrc}
                    alt={artifact.name}
                    draggable={false}
                    style={{
                      display: "block",
                      maxWidth: "100%",
                      maxHeight: "72vh",
                      objectFit: "contain",
                    }}
                  />
                </ZoomPanCanvas>
              )
            )}

            {artifact.kind === "pdf" && (
              <div
                style={{
                  display: "flex",
                  flexDirection: "column",
                  flex: 1,
                  minHeight: "520px",
                  borderRadius: "var(--radius-md)",
                  border: "1px solid var(--border-subtle)",
                  overflow: "hidden",
                  backgroundColor: "var(--bg-input)",
                }}
              >
{rawUrl ? (
                <iframe
                    data-testid="artifact-pdf-frame"
                    src={rawUrl}
                    title={artifact.name}
                    style={{
                      width: "100%",
                      height: "100%",
                      minHeight: "520px",
                      border: "none",
                      flex: 1,
                    }}
                  />
                ) : (
                  <div
                    role="alert"
                    data-testid="artifact-unsafe-url"
                    className="fd-card"
                    style={{ margin: "auto", fontSize: "12px", color: "var(--text-muted)" }}
                  >
                    This PDF cannot be previewed: the hub returned a URL that is not http(s) or same-origin.
                  </div>
                )}
              </div>
            )}

            {artifact.kind === "code" && renderCodeLines(artifact.content ?? "")}

            {officeExt && officeLoading && (
              <div data-testid="artifact-office-loading" className="fd-card" style={{ fontSize: "12px", color: "var(--text-muted)" }}>
                Loading document preview…
              </div>
            )}

            {officeExt && officeFetched && <OfficePreviewView preview={officeFetched} />}

            {officeExt && officeError && (
              <div role="alert" data-testid="artifact-office-error" className="fd-card" style={{ color: "#FCA5A5", fontSize: "12px" }}>
                {officeError}
              </div>
            )}

            {artifact.kind === "binary" && !(officeExt && (officeLoading || officeFetched)) && (
              <div
                data-testid="artifact-binary-card"
                className="fd-card"
                style={{
                  textAlign: "center",
                  padding: "32px 20px",
                  display: "flex",
                  flexDirection: "column",
                  alignItems: "center",
                  gap: "10px",
                }}
              >
                <div style={{ fontWeight: 600, fontSize: "14px" }}>{artifact.name}</div>
                <div className="fd-mono" style={{ fontSize: "12px", color: "var(--text-muted)" }}>
                  {artifact.mime_type} · {formatByteSize(artifact.size_bytes)}
                </div>
                <a
                  href={downloadUrl || undefined}
                  aria-disabled={!downloadUrl}
                  download={artifact.name}
                  className="fd-btn fd-btn-primary"
                  style={{ textDecoration: "none" }}
                >
                  ↓ Download {artifact.name}
                </a>
              </div>
            )}
          </>
            )}
          </>
        )}
      </div>
    </section>
  );
};
