import React, { useCallback, useEffect, useMemo, useState } from "react";
import type { HubApiClient } from "../api/client";
import type {
  ArtifactKind,
  ArtifactPreviewResponse,
  ArtifactSummaryItem,
  TranscriptTurn,
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
  onSelectPath?: (path: string) => void;
  onClose?: () => void;
}

type ZoomPreset = "fit" | 50 | 100 | 150 | 200;
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

export const ArtifactPreviewPane: React.FC<ArtifactPreviewPaneProps> = ({
  apiClient,
  activePath = null,
  recentPaths = [],
  sessionId = null,
  turns = [],
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
  const [zoom, setZoom] = useState<ZoomPreset>("fit");
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
      setZoom("fit");
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
                  {formatArtifactKindLabel(artifact.kind)}
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
              <MermaidDiagram
                code={artifact.content ?? ""}
                title={artifact.name}
                onOpenArtifact={handleSelectPath}
                showPreviewInArtifactsButton={false}
              />
            )}

            {(artifact.kind === "svg" || artifact.kind === "image") && (
              viewMode === "source" && artifact.kind === "svg" && artifact.content ? (
                renderCodeLines(artifact.content)
              ) : (
                <div
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "8px",
                    flex: 1,
                  }}
                >
                  <div
                    data-testid="artifact-zoom-toolbar"
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: "6px",
                      flexWrap: "wrap",
                    }}
                  >
                    <span className="fd-mono" style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                      Zoom:
                    </span>
                    {([50, 100, 150, 200, "fit"] as ZoomPreset[]).map((preset) => (
                      <button
                        key={String(preset)}
                        type="button"
                        data-testid={`artifact-zoom-${preset}`}
                        className={`fd-btn${zoom === preset ? " fd-btn-active" : ""}`}
                        onClick={() => setZoom(preset)}
                        style={{ fontSize: "11px", padding: "3px 8px" }}
                      >
                        {preset === "fit" ? "Fit" : `${preset}%`}
                      </button>
                    ))}
                  </div>

                  <div
                    data-testid="artifact-image-canvas"
                    className="fd-card"
                    style={{
                      flex: 1,
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "center",
                      overflow: "auto",
                      padding: "16px",
                      backgroundColor: "var(--bg-input)",
                      minHeight: "280px",
                    }}
                  >
                    <img
                      data-testid="artifact-image-preview"
                      src={imagePreviewSrc}
                      alt={artifact.name}
                      style={
                        zoom === "fit"
                          ? {
                              maxWidth: "100%",
                              maxHeight: "72vh",
                              objectFit: "contain",
                            }
                          : {
                              width: `${zoom}%`,
                              height: "auto",
                            }
                      }
                    />
                  </div>
                </div>
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

            {artifact.kind === "binary" && (
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
      </div>
    </section>
  );
};
