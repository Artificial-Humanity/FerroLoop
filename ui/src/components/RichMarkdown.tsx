import React from "react";

export const OPEN_ARTIFACT_EVENT = "ferrodeck:open-artifact";

export function normalizeArtifactTarget(rawPathOrUrl: string): string {
  const trimmed = rawPathOrUrl.trim().replace(/^`+|`+$/g, "");
  if (trimmed.toLowerCase().startsWith("file://")) {
    const withoutScheme = trimmed.slice("file://".length);
    return withoutScheme.replace(/#L?\d+(?:-L?\d+)?$/i, "").replace(/#.*$/, "");
  }
  return trimmed;
}

export function dispatchOpenArtifact(
  rawPathOrUrl: string,
  onOpenArtifact?: (pathOrUrl: string) => void,
): void {
  const normalized = normalizeArtifactTarget(rawPathOrUrl);
  if (!normalized) {
    return;
  }
  if (onOpenArtifact) {
    onOpenArtifact(normalized);
  } else if (typeof window !== "undefined" && typeof window.dispatchEvent === "function") {
    window.dispatchEvent(
      new CustomEvent(OPEN_ARTIFACT_EVENT, {
        detail: { path: normalized, raw: rawPathOrUrl },
      }),
    );
  }
}

export interface RichMarkdownProps {
  text: string;
  onOpenArtifact?: (pathOrUrl: string) => void;
}

export interface MermaidDiagramProps {
  code: string;
  title?: string;
  onOpenArtifact?: (pathOrUrl: string) => void;
  showPreviewInArtifactsButton?: boolean;
}

interface ParsedMermaidNode {
  id: string;
  label: string;
}

interface ParsedMermaidEdge {
  from: string;
  to: string;
  label?: string;
}

export interface ParsedMermaidGraph {
  kind: "flowchart" | "sequence" | "state" | "other";
  direction: "TD" | "LR";
  nodes: ParsedMermaidNode[];
  edges: ParsedMermaidEdge[];
}

function cleanMermaidLabel(raw: string): string {
  return raw
    .trim()
    .replace(/^["'`]+|["'`]+$/g, "")
    .replace(/^\[\[(.+)\]\]$/, "$1")
    .replace(/^\[[(](.+)[)\]]$/, "$1")
    .replace(/^\[(.+)\]$/, "$1")
    .replace(/^\((.+)\)$/, "$1")
    .replace(/^\{(.+)\}$/, "$1")
    .trim();
}

function parseNodeToken(token: string): ParsedMermaidNode | null {
  const trimmed = token.trim();
  if (!trimmed) return null;
  const match = trimmed.match(
    /^([A-Za-z0-9_.-]+)\s*(?:\[\[([^\]]+)\]\]|\[\(([^)]+)\)\]|\[([^\]]+)\]|\(\(([^)]+)\)\)|\(([^)]+)\)|\{([^}]+)\})?$/,
  );
  if (match) {
    const id = match[1];
    const rawLabel =
      match[2] ?? match[3] ?? match[4] ?? match[5] ?? match[6] ?? match[7] ?? id;
    return { id, label: cleanMermaidLabel(rawLabel) || id };
  }
  return { id: trimmed, label: cleanMermaidLabel(trimmed) || trimmed };
}

export function parseMermaidGraph(code: string): ParsedMermaidGraph {
  const lines = (code || "")
    .replace(/\r\n/g, "\n")
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0 && !l.startsWith("%%"));

  const first = lines[0] ?? "";
  let kind: ParsedMermaidGraph["kind"] = "flowchart";
  let direction: "TD" | "LR" = "TD";

  if (/^sequenceDiagram\b/i.test(first)) {
    kind = "sequence";
    direction = "LR";
  } else if (/^stateDiagram(?:-v2)?\b/i.test(first)) {
    kind = "state";
  } else if (/^(?:flowchart|graph)\b/i.test(first)) {
    kind = "flowchart";
    if (/\b(?:LR|RL)\b/i.test(first)) {
      direction = "LR";
    }
  }

  const nodeMap = new Map<string, ParsedMermaidNode>();
  const edges: ParsedMermaidEdge[] = [];

  const ensureNode = (node: ParsedMermaidNode) => {
    const existing = nodeMap.get(node.id);
    if (!existing || (existing.label === existing.id && node.label !== node.id)) {
      nodeMap.set(node.id, node);
    }
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].replace(/;$/, "").trim();
    if (
      i === 0 &&
      /^(?:flowchart|graph|sequenceDiagram|stateDiagram(?:-v2)?|classDiagram|erDiagram)\b/i.test(
        line,
      )
    ) {
      continue;
    }
    if (
      /^(?:subgraph|end|style|classDef|class|click|linkStyle|direction)\b/i.test(line)
    ) {
      continue;
    }

    if (kind === "sequence") {
      const partMatch = line.match(
        /^(?:participant|actor)\s+([A-Za-z0-9_.-]+)(?:\s+as\s+(.+))?$/i,
      );
      if (partMatch) {
        const id = partMatch[1];
        const label = cleanMermaidLabel(partMatch[2] ?? id);
        ensureNode({ id, label });
        continue;
      }
      const seqMatch = line.match(
        /^([A-Za-z0-9_.-]+)\s*(?:-+>>?|--+>>?|-+\)|--+\))\s*([A-Za-z0-9_.-]+)\s*:\s*(.+)$/,
      );
      if (seqMatch) {
        const fromId = seqMatch[1];
        const toId = seqMatch[2];
        const label = cleanMermaidLabel(seqMatch[3]);
        ensureNode({ id: fromId, label: fromId });
        ensureNode({ id: toId, label: toId });
        edges.push({ from: fromId, to: toId, label });
        continue;
      }
    }

    // Flowchart / State edge with optional |label| or : label
    const edgeMatch = line.match(
      /^(.+?)\s*(?:--+>|==+>|-\.+->|--+\|([^|]+)\|-->|--+([^-|][^>-]*?)\s*-->)\s*(.+)$/,
    );
    if (edgeMatch) {
      const leftNode = parseNodeToken(edgeMatch[1]);
      const edgeLabel = cleanMermaidLabel(edgeMatch[2] ?? edgeMatch[3] ?? "");
      let rightRaw = edgeMatch[4].trim();
      let trailingLabel = edgeLabel;
      if (!trailingLabel && rightRaw.includes(":")) {
        const colonIdx = rightRaw.indexOf(":");
        const beforeColon = rightRaw.slice(0, colonIdx).trim();
        const afterColon = rightRaw.slice(colonIdx + 1).trim();
        if (beforeColon && afterColon && !beforeColon.includes("[")) {
          rightRaw = beforeColon;
          trailingLabel = cleanMermaidLabel(afterColon);
        }
      }
      const rightNode = parseNodeToken(rightRaw);
      if (leftNode && rightNode) {
        const normFrom = leftNode.id === "[*]" ? "start_state" : leftNode.id;
        const normTo = rightNode.id === "[*]" ? "end_state" : rightNode.id;
        ensureNode({
          id: normFrom,
          label: leftNode.id === "[*]" ? "●" : leftNode.label,
        });
        ensureNode({
          id: normTo,
          label: rightNode.id === "[*]" ? "◉" : rightNode.label,
        });
        edges.push({
          from: normFrom,
          to: normTo,
          label: trailingLabel || undefined,
        });
        continue;
      }
    }

    const standalone = parseNodeToken(line);
    if (standalone && standalone.label !== line) {
      ensureNode(standalone);
    }
  }

  if (nodeMap.size === 0 && lines.length > 0) {
    lines.slice(0, 8).forEach((l, idx) => {
      nodeMap.set(`step_${idx}`, { id: `step_${idx}`, label: cleanMermaidLabel(l) });
      if (idx > 0) {
        edges.push({ from: `step_${idx - 1}`, to: `step_${idx}` });
      }
    });
  }

  return {
    kind,
    direction,
    nodes: Array.from(nodeMap.values()),
    edges,
  };
}

export function renderMermaidToSvgString(code: string): string {
  const graph = parseMermaidGraph(code);
  const nodes = graph.nodes;
  const edges = graph.edges;
  const isHorizontal = graph.direction === "LR";

  const nodeW = 168;
  const nodeH = 44;
  const gapX = isHorizontal ? 76 : 44;
  const gapY = isHorizontal ? 54 : 56;
  const padX = 28;
  const padY = 28;

  const positions = new Map<string, { x: number; y: number }>();
  nodes.forEach((node, idx) => {
    const x = isHorizontal ? padX + idx * (nodeW + gapX) : padX;
    const y = isHorizontal ? padY : padY + idx * (nodeH + gapY);
    positions.set(node.id, { x, y });
  });

  const width = isHorizontal
    ? Math.max(320, padX * 2 + nodes.length * nodeW + Math.max(0, nodes.length - 1) * gapX)
    : Math.max(360, padX * 2 + nodeW + 140);
  const height = isHorizontal
    ? Math.max(140, padY * 2 + nodeH + (edges.length > 0 ? 60 : 20))
    : Math.max(140, padY * 2 + nodes.length * nodeH + Math.max(0, nodes.length - 1) * gapY);

  const esc = (s: string) =>
    s
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");

  const edgeEls = edges
    .map((edge) => {
      const fromPos = positions.get(edge.from);
      const toPos = positions.get(edge.to);
      if (!fromPos || !toPos) return "";
      const x1 = isHorizontal ? fromPos.x + nodeW : fromPos.x + nodeW / 2;
      const y1 = isHorizontal ? fromPos.y + nodeH / 2 : fromPos.y + nodeH;
      const x2 = isHorizontal ? toPos.x : toPos.x + nodeW / 2;
      const y2 = isHorizontal ? toPos.y + nodeH / 2 : toPos.y;
      const midX = (x1 + x2) / 2;
      const midY = (y1 + y2) / 2;
      const labelMarkup = edge.label
        ? `<text x="${midX + (isHorizontal ? 0 : 10)}" y="${midY - 4}" fill="#94A3B8" font-family="JetBrains Mono, monospace" font-size="10" text-anchor="${isHorizontal ? "middle" : "start"}">${esc(edge.label)}</text>`
        : "";
      return `<g><line x1="${x1}" y1="${y1}" x2="${x2}" y2="${y2}" stroke="#38BDF8" stroke-width="1.6" marker-end="url(#fd-mermaid-arrow)" />${labelMarkup}</g>`;
    })
    .join("");

  const nodeEls = nodes
    .map((node) => {
      const pos = positions.get(node.id) ?? { x: padX, y: padY };
      const truncated =
        node.label.length > 24 ? `${node.label.slice(0, 22)}…` : node.label;
      return `<g transform="translate(${pos.x},${pos.y})"><rect width="${nodeW}" height="${nodeH}" rx="6" fill="#171E29" stroke="#38BDF8" stroke-opacity="0.55" stroke-width="1.4" /><text x="${nodeW / 2}" y="${nodeH / 2 + 4}" fill="#F1F5F9" font-family="Inter, sans-serif" font-size="12" font-weight="500" text-anchor="middle">${esc(truncated)}</text></g>`;
    })
    .join("");

  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${width} ${height}" width="${width}" height="${height}"><defs><marker id="fd-mermaid-arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M 0 1 L 10 5 L 0 9 z" fill="#38BDF8" /></marker></defs>${edgeEls}${nodeEls}</svg>`;
}

export const MermaidDiagram: React.FC<MermaidDiagramProps> = ({
  code,
  title = "Mermaid Diagram",
  onOpenArtifact,
  showPreviewInArtifactsButton = true,
}) => {
  const [showSource, setShowSource] = React.useState(false);
  const graph = React.useMemo(() => parseMermaidGraph(code), [code]);
  const svgString = React.useMemo(() => renderMermaidToSvgString(code), [code]);
  const svgDataUrl = React.useMemo(
    () => `data:image/svg+xml;utf8,${encodeURIComponent(svgString)}`,
    [svgString],
  );

  const isHorizontal = graph.direction === "LR";
  const nodeW = 168;
  const nodeH = 44;
  const gapX = isHorizontal ? 76 : 44;
  const gapY = isHorizontal ? 54 : 56;
  const padX = 28;
  const padY = 28;

  const positions = React.useMemo(() => {
    const map = new Map<string, { x: number; y: number }>();
    graph.nodes.forEach((node, idx) => {
      const x = isHorizontal ? padX + idx * (nodeW + gapX) : padX;
      const y = isHorizontal ? padY : padY + idx * (nodeH + gapY);
      map.set(node.id, { x, y });
    });
    return map;
  }, [graph.nodes, isHorizontal, gapX, gapY]);

  const width = isHorizontal
    ? Math.max(
        320,
        padX * 2 +
          graph.nodes.length * nodeW +
          Math.max(0, graph.nodes.length - 1) * gapX,
      )
    : Math.max(360, padX * 2 + nodeW + 140);
  const height = isHorizontal
    ? Math.max(140, padY * 2 + nodeH + (graph.edges.length > 0 ? 60 : 20))
    : Math.max(
        140,
        padY * 2 +
          graph.nodes.length * nodeH +
          Math.max(0, graph.nodes.length - 1) * gapY,
      );

  return (
    <div
      data-testid="mermaid-diagram-card"
      style={{
        borderRadius: "var(--radius-md)",
        border: "1px solid var(--border-subtle)",
        backgroundColor: "var(--bg-input)",
        overflow: "hidden",
        margin: "6px 0",
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: "8px",
          padding: "6px 10px",
          borderBottom: "1px solid var(--border-subtle)",
          backgroundColor: "var(--bg-elevated)",
          flexWrap: "wrap",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
          <span className="fd-badge" style={{ color: "var(--status-info)" }}>
            ◇ {title}
          </span>
          <span className="fd-mono" style={{ fontSize: "11px", color: "var(--text-muted)" }}>
            {graph.nodes.length} nodes · {graph.edges.length} edges
          </span>
        </div>
        <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
          <button
            type="button"
            className="fd-btn"
            data-testid="mermaid-source-toggle"
            onClick={() => setShowSource((prev) => !prev)}
            style={{ fontSize: "11px", padding: "3px 8px" }}
          >
            {showSource ? "Hide Source" : "Show Source"}
          </button>
          <a
            href={svgDataUrl}
            download="diagram.svg"
            className="fd-btn"
            data-testid="mermaid-download-svg"
            style={{ fontSize: "11px", padding: "3px 8px", textDecoration: "none" }}
          >
            ↓ SVG
          </a>
          {showPreviewInArtifactsButton && (
            <button
              type="button"
              className="fd-btn"
              data-testid="mermaid-preview-in-artifacts-btn"
              onClick={() =>
                dispatchOpenArtifact(`mermaid://${encodeURIComponent(code)}`, onOpenArtifact)
              }
              style={{ fontSize: "11px", padding: "3px 8px" }}
            >
              Preview in Artifacts
            </button>
          )}
        </div>
      </div>

      <div
        style={{
          padding: "12px",
          overflowX: "auto",
          display: "flex",
          justifyContent: "center",
        }}
      >
        <svg
          data-testid="mermaid-svg"
          viewBox={`0 0 ${width} ${height}`}
          width={width}
          height={height}
          style={{ maxWidth: "100%", height: "auto" }}
        >
          <defs>
            <marker
              id="fd-mermaid-arrow"
              viewBox="0 0 10 10"
              refX="8"
              refY="5"
              markerWidth="6"
              markerHeight="6"
              orient="auto-start-reverse"
            >
              <path d="M 0 1 L 10 5 L 0 9 z" fill="var(--status-info)" />
            </marker>
          </defs>
          {graph.edges.map((edge, idx) => {
            const fromPos = positions.get(edge.from);
            const toPos = positions.get(edge.to);
            if (!fromPos || !toPos) return null;
            const x1 = isHorizontal ? fromPos.x + nodeW : fromPos.x + nodeW / 2;
            const y1 = isHorizontal ? fromPos.y + nodeH / 2 : fromPos.y + nodeH;
            const x2 = isHorizontal ? toPos.x : toPos.x + nodeW / 2;
            const y2 = isHorizontal ? toPos.y + nodeH / 2 : toPos.y;
            const midX = (x1 + x2) / 2;
            const midY = (y1 + y2) / 2;
            return (
              <g key={`edge-${idx}`}>
                <line
                  x1={x1}
                  y1={y1}
                  x2={x2}
                  y2={y2}
                  stroke="var(--status-info)"
                  strokeWidth="1.6"
                  markerEnd="url(#fd-mermaid-arrow)"
                />
                {edge.label && (
                  <text
                    x={midX + (isHorizontal ? 0 : 10)}
                    y={midY - 4}
                    fill="var(--text-muted)"
                    fontSize="10"
                    fontFamily="var(--font-mono)"
                    textAnchor={isHorizontal ? "middle" : "start"}
                  >
                    {edge.label}
                  </text>
                )}
              </g>
            );
          })}
          {graph.nodes.map((node) => {
            const pos = positions.get(node.id) ?? { x: padX, y: padY };
            return (
              <g key={node.id} transform={`translate(${pos.x},${pos.y})`}>
                <rect
                  width={nodeW}
                  height={nodeH}
                  rx="6"
                  fill="var(--bg-elevated)"
                  stroke="var(--status-info)"
                  strokeOpacity="0.55"
                  strokeWidth="1.4"
                />
                <text
                  x={nodeW / 2}
                  y={nodeH / 2 + 4}
                  fill="var(--text-primary)"
                  fontSize="12"
                  fontWeight="500"
                  textAnchor="middle"
                >
                  {node.label}
                </text>
              </g>
            );
          })}
        </svg>
      </div>

      {showSource && (
        <pre
          data-testid="mermaid-source-code"
          className="fd-mono"
          style={{
            margin: 0,
            padding: "10px 12px",
            borderTop: "1px solid var(--border-subtle)",
            fontSize: "11.5px",
            color: "var(--text-secondary)",
            whiteSpace: "pre-wrap",
          }}
        >
          <code>{code}</code>
        </pre>
      )}
    </div>
  );
};

interface FileBadgeSpec {
  badge: string;
  color: string;
  isMono: boolean;
  fontSize: string;
}

export function getFileBadgeSpec(urlOrLabel: string, fallbackLabel?: string): FileBadgeSpec {
  const stripAnchorAndQuery = (raw: string): string =>
    raw.replace(/#.*$/, "").replace(/\?.*$/, "").trim();

  const extractExt = (candidate: string): string => {
    const cleaned = stripAnchorAndQuery(candidate).replace(/^`+|`+$/g, "");
    const base = cleaned.split("/").pop() || cleaned;
    const dotIdx = base.lastIndexOf(".");
    if (dotIdx <= 0 || dotIdx === base.length - 1) {
      return "";
    }
    return base.slice(dotIdx).toLowerCase();
  };

  const ext = extractExt(urlOrLabel) || (fallbackLabel ? extractExt(fallbackLabel) : "");

  switch (ext) {
    case ".jsx":
    case ".tsx":
      return { badge: "⚛", color: "#38BDF8", isMono: false, fontSize: "12px" };
    case ".js":
    case ".mjs":
    case ".cjs":
      return { badge: "JS", color: "#FACC15", isMono: true, fontSize: "10px" };
    case ".ts":
      return { badge: "TS", color: "#60A5FA", isMono: true, fontSize: "10px" };
    case ".md":
    case ".mdx":
      return { badge: "M↓", color: "#38BDF8", isMono: true, fontSize: "10px" };
    case ".yml":
    case ".yaml":
      return { badge: "Y", color: "#C084FC", isMono: true, fontSize: "10px" };
    case ".rs":
      return { badge: "RS", color: "#FB923C", isMono: true, fontSize: "10px" };
    case ".py":
      return { badge: "PY", color: "#60A5FA", isMono: true, fontSize: "10px" };
    case ".css":
    case ".scss":
      return { badge: "#", color: "#A855F7", isMono: true, fontSize: "10px" };
    case ".html":
      return { badge: "<>", color: "#F97316", isMono: true, fontSize: "10px" };
    case ".sh":
    case ".bash":
    case ".zsh":
      return { badge: "$_", color: "#4ADE80", isMono: true, fontSize: "10px" };
    case ".json":
    case ".jsonc":
    case ".toml":
      return { badge: "{}", color: "#94A3B8", isMono: true, fontSize: "10px" };
    default:
      return { badge: "📄", color: "#94A3B8", isMono: false, fontSize: "11px" };
  }
}

const AMBER_CODE_PILL_STYLE: React.CSSProperties = {
  fontFamily: "var(--font-mono)",
  fontSize: "12.5px",
  padding: "1px 4px",
  borderRadius: "4px",
  backgroundColor: "var(--code-inline-bg)",
  border: "none",
  color: "var(--code-inline-text)",
};

const FILE_CHIP_STYLE: React.CSSProperties = {
  display: "inline-flex",
  alignItems: "center",
  gap: "5px",
  padding: "1px 6px",
  borderRadius: "5px",
  backgroundColor: "rgba(255, 255, 255, 0.05)",
  border: "1px solid var(--border-subtle)",
  color: "var(--text-primary)",
  fontWeight: 500,
  fontSize: "13px",
  verticalAlign: "baseline",
};

const INLINE_TOKEN_REGEX =
  /(\[(?:`[^`]+`|[^\]\n]+)\]\([^)\s]+\)|`[^`\n]+`|\*\*[^*\n]+?\*\*|~~[^~\n]+?~~|\*(?!\s)[^*\n]+?(?<!\s)\*|<https?:\/\/[^\s>]+>|https?:\/\/[^\s<>]+)/g;

/** Split trailing sentence punctuation and unmatched closers off a bare URL. */
function splitBareUrlSuffix(token: string): { cleanUrl: string; suffix: string } {
  let cleanUrl = token;
  let suffix = "";
  for (;;) {
    if (/[.,;:!?]$/.test(cleanUrl)) {
      suffix = cleanUrl.slice(-1) + suffix;
      cleanUrl = cleanUrl.slice(0, -1);
      continue;
    }
    if (cleanUrl.endsWith(")")) {
      const opens = (cleanUrl.match(/\(/g) ?? []).length;
      const closes = (cleanUrl.match(/\)/g) ?? []).length;
      if (closes > opens) {
        suffix = ")" + suffix;
        cleanUrl = cleanUrl.slice(0, -1);
        continue;
      }
    }
    break;
  }
  return { cleanUrl, suffix };
}

export function shouldShowLinkDomainBadge(
  cleanedLabel: string,
  targetHostname: string,
): boolean {
  const normalizedTarget = targetHostname.trim().toLowerCase();
  if (!normalizedTarget) {
    return false;
  }

  const urlTokens = cleanedLabel.match(/https?:\/\/[^\s<>()[\]"']+/gi);
  if (urlTokens && urlTokens.length > 0) {
    for (const rawUrl of urlTokens) {
      const { cleanUrl } = splitBareUrlSuffix(rawUrl);
      try {
        const parsedHost = new URL(cleanUrl).hostname.toLowerCase();
        if (parsedHost !== normalizedTarget) {
          return true;
        }
      } catch {
        return true;
      }
    }
    return false;
  }

  const escapedTarget = normalizedTarget.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const boundaryPattern = new RegExp(
    `(?:^|[^a-z0-9.-])(?:[a-z0-9-]+\\.)*${escapedTarget}(?=$|[^a-z0-9.-]|\\.(?![a-z0-9-]))`,
    "i",
  );
  return !boundaryPattern.test(cleanedLabel);
}

export function renderInlineMarkdown(
  text: string,
  keyPrefix = "inline",
  onOpenArtifact?: (pathOrUrl: string) => void,
): React.ReactNode[] {
  if (!text) {
    return [];
  }

  const nodes: React.ReactNode[] = [];
  let lastIndex = 0;
  let matchIndex = 0;
  const regex = new RegExp(INLINE_TOKEN_REGEX.source, "g");
  let match: RegExpExecArray | null = regex.exec(text);

  while (match !== null) {
    if (match.index > lastIndex) {
      nodes.push(text.slice(lastIndex, match.index));
    }

    const token = match[0];
    const key = `${keyPrefix}-${matchIndex++}`;

    if (token.startsWith("[") && token.includes("](") && token.endsWith(")")) {
      const closeBracketIdx = token.indexOf("](");
      const rawLabel = token.slice(1, closeBracketIdx);
      const url = token.slice(closeBracketIdx + 2, -1).trim();
      const cleanedLabel = rawLabel.replace(/^`(.+)`$/, "$1");

      if (url.toLowerCase().startsWith("file://")) {
        const badgeSpec = getFileBadgeSpec(url, cleanedLabel);
        nodes.push(
          <span
            key={key}
            role="button"
            tabIndex={0}
            data-testid="file-reference-chip"
            data-file-url={url}
            title={url}
            onClick={(e) => {
              e.preventDefault();
              dispatchOpenArtifact(url, onOpenArtifact);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                dispatchOpenArtifact(url, onOpenArtifact);
              }
            }}
            style={{ ...FILE_CHIP_STYLE, cursor: "pointer" }}
          >
            <span
              style={{
                color: badgeSpec.color,
                fontSize: badgeSpec.fontSize,
                fontWeight: badgeSpec.isMono ? 700 : 500,
                fontFamily: badgeSpec.isMono ? "var(--font-mono)" : undefined,
                lineHeight: 1,
              }}
            >
              {badgeSpec.badge}
            </span>
            <span>{cleanedLabel}</span>
          </span>,
        );
      } else if (/^https?:\/\//i.test(url)) {
        let targetHostname = "";
        try {
          targetHostname = new URL(url).hostname;
        } catch {
          targetHostname = "";
        }
        const showDomainBadge = shouldShowLinkDomainBadge(
          cleanedLabel,
          targetHostname,
        );
        nodes.push(
          <a
            key={key}
            href={url}
            // The label is attacker-controllable text; expose the real
            // destination so `[https://bank.example](https://evil.example)`
            // cannot masquerade as its label.
            title={url}
            target="_blank"
            rel="noopener noreferrer"
            style={{
              color: "var(--accent-sky, #38BDF8)",
              textDecoration: "underline",
            }}
          >
            {
              // A URL inside a link label would nest an <a> in an <a>
              // (invalid, and its inner target can differ from the outer
              // one), so URL-bearing labels render as plain text.
              /https?:\/\//i.test(cleanedLabel)
                ? cleanedLabel
                : renderInlineMarkdown(cleanedLabel, `${key}-lbl`, onOpenArtifact)
            }
          </a>,
        );
        if (showDomainBadge) {
          nodes.push(
            <span
              key={`${key}-domain`}
              data-testid="markdown-link-domain-badge"
              className="fd-mono"
              style={{
                marginLeft: "4px",
                fontSize: "10px",
                color: "var(--text-muted, #64748B)",
              }}
            >
              ({targetHostname})
            </span>,
          );
        }
      } else {
        nodes.push(
          <span key={key}>
            {renderInlineMarkdown(cleanedLabel, `${key}-lbl`, onOpenArtifact)}
          </span>,
        );
      }
    } else if (token.startsWith("<") && token.endsWith(">") && /^<https?:\/\//i.test(token)) {
      const { cleanUrl, suffix } = splitBareUrlSuffix(token.slice(1, -1));
      nodes.push(
        <a
          key={key}
          href={cleanUrl}
          target="_blank"
          rel="noopener noreferrer"
          style={{
            color: "var(--accent-sky, #38BDF8)",
            textDecoration: "underline",
          }}
        >
          {cleanUrl}
        </a>,
      );
      if (suffix) {
        nodes.push(suffix);
      }
    } else if (token.startsWith("`") && token.endsWith("`") && token.length >= 2) {
      const codeText = token.slice(1, -1);
      const trimmedCode = codeText
        .trim()
        .replace(/^<(\S+)>$/, "$1");
      const { cleanUrl, suffix } = splitBareUrlSuffix(trimmedCode);
      if (/^https?:\/\/\S+$/i.test(cleanUrl)) {
        nodes.push(
          <a
            key={key}
            href={cleanUrl}
            target="_blank"
            rel="noopener noreferrer"
            style={{
              ...AMBER_CODE_PILL_STYLE,
              textDecoration: "underline",
            }}
          >
            <code style={{ fontFamily: "inherit", fontSize: "inherit", color: "inherit" }}>
              {cleanUrl}
            </code>
          </a>,
        );
        if (suffix) {
          nodes.push(suffix);
        }
      } else {
        nodes.push(
          <code key={key} style={AMBER_CODE_PILL_STYLE}>
            {codeText}
          </code>,
        );
      }
    } else if (token.startsWith("**") && token.endsWith("**") && token.length > 4) {
      const inner = token.slice(2, -2);
      nodes.push(
        <strong key={key} style={{ color: "var(--text-primary)", fontWeight: 600 }}>
          {renderInlineMarkdown(inner, `${key}-b`, onOpenArtifact)}
        </strong>,
      );
    } else if (token.startsWith("~~") && token.endsWith("~~") && token.length > 4) {
      const inner = token.slice(2, -2);
      nodes.push(<del key={key}>{renderInlineMarkdown(inner, `${key}-s`, onOpenArtifact)}</del>);
    } else if (token.startsWith("*") && token.endsWith("*") && token.length > 2) {
      const inner = token.slice(1, -1);
      nodes.push(<em key={key}>{renderInlineMarkdown(inner, `${key}-i`, onOpenArtifact)}</em>);
    } else if (/^https?:\/\//i.test(token)) {
      const { cleanUrl, suffix: trailingPunct } = splitBareUrlSuffix(token);
      nodes.push(
        <a
          key={key}
          href={cleanUrl}
          target="_blank"
          rel="noopener noreferrer"
          style={{
            color: "var(--accent-sky, #38BDF8)",
            textDecoration: "underline",
          }}
        >
          {cleanUrl}
        </a>,
      );
      if (trailingPunct) {
        nodes.push(trailingPunct);
      }
    } else {
      nodes.push(token);
    }

    lastIndex = match.index + token.length;
    match = regex.exec(text);
  }

  if (lastIndex < text.length) {
    nodes.push(text.slice(lastIndex));
  }

  return nodes;
}

interface ParsedListItem {
  indent: number;
  ordered: boolean;
  content: string;
  children: ParsedListGroup[];
}

interface ParsedListGroup {
  ordered: boolean;
  indent: number;
  items: ParsedListItem[];
}

const LIST_LINE_REGEX = /^(\s*)(?:([-*+])|(\d+)\.)\s+(.*)$/;
const HEADING_LINE_REGEX = /^\s*(#{1,6})\s+(.*)$/;
const HR_LINE_REGEX = /^\s*(?:---+|___+|\*\*\*+)\s*$/;
const BLOCKQUOTE_LINE_REGEX = /^\s*>\s?(.*)$/;
const TABLE_ROW_REGEX = /^\s*\|(.+)\|\s*$/;
const TABLE_DIVIDER_REGEX = /^\s*\|(?:\s*:?-{2,}:?\s*\|)+\s*$/;
const CODE_FENCE_OPEN_REGEX = /^\s*```([a-zA-Z0-9_-]*)\s*$/;
const CODE_FENCE_CLOSE_REGEX = /^\s*```\s*$/;

function parseListBlock(lines: string[], startIdx: number): {
  groups: ParsedListGroup[];
  nextIdx: number;
} {
  const rawItems: { indent: number; ordered: boolean; content: string }[] = [];
  let idx = startIdx;

  while (idx < lines.length) {
    const line = lines[idx];
    if (line.trim() === "") {
      // Look ahead to see if next non-empty line continues the list
      let lookahead = idx + 1;
      while (lookahead < lines.length && lines[lookahead].trim() === "") {
        lookahead++;
      }
      if (lookahead < lines.length && LIST_LINE_REGEX.test(lines[lookahead])) {
        idx = lookahead;
        continue;
      }
      break;
    }

    const listMatch = line.match(LIST_LINE_REGEX);
    if (listMatch) {
      const indent = listMatch[1].replace(/\t/g, "  ").length;
      const ordered = Boolean(listMatch[3]);
      const content = listMatch[4];
      rawItems.push({ indent, ordered, content });
      idx++;
      continue;
    }

    // Continuation line for the previous list item if indented
    if (
      rawItems.length > 0 &&
      /^\s{2,}\S/.test(line) &&
      !HEADING_LINE_REGEX.test(line) &&
      !HR_LINE_REGEX.test(line) &&
      !BLOCKQUOTE_LINE_REGEX.test(line)
    ) {
      rawItems[rawItems.length - 1].content += " " + line.trim();
      idx++;
      continue;
    }

    break;
  }

  const rootGroups: ParsedListGroup[] = [];
  const stack: ParsedListGroup[] = [];

  for (const item of rawItems) {
    const node: ParsedListItem = {
      indent: item.indent,
      ordered: item.ordered,
      content: item.content,
      children: [],
    };

    while (stack.length > 0 && item.indent < stack[stack.length - 1].indent) {
      stack.pop();
    }

    if (stack.length === 0) {
      const lastRoot = rootGroups[rootGroups.length - 1];
      if (lastRoot && lastRoot.ordered === item.ordered && lastRoot.indent === item.indent) {
        lastRoot.items.push(node);
        stack.push(lastRoot);
      } else {
        const group: ParsedListGroup = {
          ordered: item.ordered,
          indent: item.indent,
          items: [node],
        };
        rootGroups.push(group);
        stack.push(group);
      }
    } else {
      const currentGroup = stack[stack.length - 1];
      if (item.indent > currentGroup.indent) {
        const parentItem = currentGroup.items[currentGroup.items.length - 1];
        const lastChildGroup = parentItem.children[parentItem.children.length - 1];
        if (lastChildGroup && lastChildGroup.ordered === item.ordered) {
          lastChildGroup.items.push(node);
          stack.push(lastChildGroup);
        } else {
          const newChildGroup: ParsedListGroup = {
            ordered: item.ordered,
            indent: item.indent,
            items: [node],
          };
          parentItem.children.push(newChildGroup);
          stack.push(newChildGroup);
        }
      } else if (currentGroup.ordered === item.ordered) {
        currentGroup.items.push(node);
      } else {
        stack.pop();
        const group: ParsedListGroup = {
          ordered: item.ordered,
          indent: item.indent,
          items: [node],
        };
        if (stack.length === 0) {
          rootGroups.push(group);
        } else {
          const parentGroup = stack[stack.length - 1];
          const parentItem = parentGroup.items[parentGroup.items.length - 1];
          parentItem.children.push(group);
        }
        stack.push(group);
      }
    }
  }

  return { groups: rootGroups, nextIdx: idx };
}

function renderListGroup(
  group: ParsedListGroup,
  key: string,
  onOpenArtifact?: (pathOrUrl: string) => void,
): React.ReactNode {
  const ListTag = group.ordered ? "ol" : "ul";
  return (
    <ListTag
      key={key}
      style={{
        paddingLeft: "20px",
        margin: "6px 0",
        display: "flex",
        flexDirection: "column",
        gap: "6px",
        lineHeight: 1.6,
      }}
    >
      {group.items.map((item, idx) => (
        <li key={`${key}-li-${idx}`}>
          {renderInlineMarkdown(item.content, `${key}-li-${idx}`, onOpenArtifact)}
          {item.children.map((childGroup, cIdx) =>
            renderListGroup(childGroup, `${key}-li-${idx}-c-${cIdx}`, onOpenArtifact),
          )}
        </li>
      ))}
    </ListTag>
  );
}

function parseTableCells(rowLine: string): string[] {
  const match = rowLine.match(TABLE_ROW_REGEX);
  if (!match) return [];
  return match[1].split("|").map((c) => c.trim());
}

export const RichMarkdown: React.FC<RichMarkdownProps> = ({ text, onOpenArtifact }) => {
  if (!text || !text.trim()) {
    return null;
  }

  const lines = text.replace(/\r\n/g, "\n").split("\n");
  const blocks: React.ReactNode[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i];

    if (line.trim() === "") {
      i++;
      continue;
    }

    // Fenced Code Block (including ```mermaid)
    const fenceMatch = line.match(CODE_FENCE_OPEN_REGEX);
    if (fenceMatch) {
      const lang = (fenceMatch[1] || "").trim().toLowerCase();
      const startFence = i;
      i++;
      const codeLines: string[] = [];
      while (i < lines.length && !CODE_FENCE_CLOSE_REGEX.test(lines[i])) {
        codeLines.push(lines[i]);
        i++;
      }
      if (i < lines.length && CODE_FENCE_CLOSE_REGEX.test(lines[i])) {
        i++;
      }
      const codeText = codeLines.join("\n");
      if (lang === "mermaid") {
        blocks.push(
          <MermaidDiagram
            key={`mermaid-${startFence}`}
            code={codeText}
            onOpenArtifact={onOpenArtifact}
          />,
        );
      } else {
        blocks.push(
          <div
            key={`fence-${startFence}`}
            style={{
              borderRadius: "var(--radius-sm)",
              border: "1px solid var(--border-subtle)",
              backgroundColor: "var(--bg-input)",
              overflow: "hidden",
              margin: "6px 0",
            }}
          >
            {lang && (
              <div
                className="fd-mono"
                style={{
                  padding: "4px 10px",
                  fontSize: "10px",
                  color: "var(--text-muted)",
                  borderBottom: "1px solid var(--border-subtle)",
                  backgroundColor: "rgba(23, 30, 41, 0.7)",
                }}
              >
                {lang}
              </div>
            )}
            <pre
              className="fd-mono"
              style={{
                margin: 0,
                padding: "10px 12px",
                fontSize: "12px",
                lineHeight: 1.45,
                color: "var(--text-secondary)",
                whiteSpace: "pre-wrap",
                wordBreak: "break-word",
                overflowX: "auto",
              }}
            >
              <code>{codeText}</code>
            </pre>
          </div>,
        );
      }
      continue;
    }

    // Horizontal Rule
    if (HR_LINE_REGEX.test(line)) {
      blocks.push(
        <hr
          key={`hr-${i}`}
          style={{
            border: "none",
            borderTop: "1px solid var(--border-subtle)",
            margin: "10px 0",
          }}
        />,
      );
      i++;
      continue;
    }

    // Heading
    const headingMatch = line.match(HEADING_LINE_REGEX);
    if (headingMatch) {
      const level = headingMatch[1].length;
      const headingText = headingMatch[2].trim();
      const HeadingTag = level <= 2 ? "h2" : level === 3 ? "h3" : "h4";
      const fontSize = level === 1 ? "16px" : level === 2 ? "15px" : level === 3 ? "14px" : "13.5px";
      blocks.push(
        <HeadingTag
          key={`h-${i}`}
          style={{
            fontWeight: 600,
            fontSize,
            color: "var(--text-primary)",
            margin: "12px 0 6px",
            lineHeight: 1.35,
          }}
        >
          {renderInlineMarkdown(headingText, `h-${i}`, onOpenArtifact)}
        </HeadingTag>,
      );
      i++;
      continue;
    }

    // Table
    if (
      TABLE_ROW_REGEX.test(line) &&
      i + 1 < lines.length &&
      TABLE_DIVIDER_REGEX.test(lines[i + 1])
    ) {
      const headers = parseTableCells(line);
      i += 2;
      const rows: string[][] = [];
      while (i < lines.length && TABLE_ROW_REGEX.test(lines[i])) {
        rows.push(parseTableCells(lines[i]));
        i++;
      }
      blocks.push(
        <div key={`tbl-wrap-${i}`} style={{ overflowX: "auto", margin: "8px 0" }}>
          <table
            style={{
              borderCollapse: "collapse",
              width: "100%",
              fontSize: "12.5px",
              border: "1px solid var(--border-subtle)",
            }}
          >
            <thead>
              <tr style={{ backgroundColor: "rgba(255, 255, 255, 0.04)" }}>
                {headers.map((header, hIdx) => (
                  <th
                    key={`th-${hIdx}`}
                    style={{
                      border: "1px solid var(--border-subtle)",
                      padding: "6px 10px",
                      textAlign: "left",
                      fontWeight: 600,
                      color: "var(--text-primary)",
                    }}
                  >
                    {renderInlineMarkdown(header, `tbl-${i}-th-${hIdx}`, onOpenArtifact)}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {rows.map((row, rIdx) => (
                <tr key={`tr-${rIdx}`}>
                  {row.map((cell, cIdx) => (
                    <td
                      key={`td-${cIdx}`}
                      style={{
                        border: "1px solid var(--border-subtle)",
                        padding: "6px 10px",
                        color: "var(--text-secondary)",
                      }}
                    >
                      {renderInlineMarkdown(
                        cell,
                        `tbl-${i}-tr-${rIdx}-td-${cIdx}`,
                        onOpenArtifact,
                      )}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>,
      );
      continue;
    }

    // Blockquote
    if (BLOCKQUOTE_LINE_REGEX.test(line)) {
      const quoteLines: string[] = [];
      const startQuote = i;
      while (i < lines.length && BLOCKQUOTE_LINE_REGEX.test(lines[i])) {
        const m = lines[i].match(BLOCKQUOTE_LINE_REGEX);
        quoteLines.push(m ? m[1] : "");
        i++;
      }
      blocks.push(
        <blockquote
          key={`bq-${startQuote}`}
          style={{
            margin: "6px 0",
            padding: "4px 12px",
            borderLeft: "3px solid var(--border-accent, #38BDF8)",
            color: "var(--text-secondary)",
            backgroundColor: "rgba(255, 255, 255, 0.02)",
            borderRadius: "0 4px 4px 0",
          }}
        >
          {quoteLines.map((ql, qIdx) => (
            <React.Fragment key={`bq-${startQuote}-${qIdx}`}>
              {qIdx > 0 && <br />}
              {renderInlineMarkdown(ql, `bq-${startQuote}-${qIdx}`, onOpenArtifact)}
            </React.Fragment>
          ))}
        </blockquote>,
      );
      continue;
    }

    // Bulleted or Numbered List
    if (LIST_LINE_REGEX.test(line)) {
      const { groups, nextIdx } = parseListBlock(lines, i);
      groups.forEach((group, gIdx) => {
        blocks.push(renderListGroup(group, `list-${i}-${gIdx}`, onOpenArtifact));
      });
      i = nextIdx > i ? nextIdx : i + 1;
      continue;
    }

    // Paragraph block
    const paraLines: string[] = [];
    const startPara = i;
    while (
      i < lines.length &&
      lines[i].trim() !== "" &&
      !CODE_FENCE_OPEN_REGEX.test(lines[i]) &&
      !HR_LINE_REGEX.test(lines[i]) &&
      !HEADING_LINE_REGEX.test(lines[i]) &&
      !BLOCKQUOTE_LINE_REGEX.test(lines[i]) &&
      !LIST_LINE_REGEX.test(lines[i]) &&
      !(
        TABLE_ROW_REGEX.test(lines[i]) &&
        i + 1 < lines.length &&
        TABLE_DIVIDER_REGEX.test(lines[i + 1])
      )
    ) {
      paraLines.push(lines[i]);
      i++;
    }

    blocks.push(
      <p
        key={`p-${startPara}`}
        style={{
          margin: "4px 0",
          lineHeight: 1.6,
        }}
      >
        {paraLines.map((pl, pIdx) => (
          <React.Fragment key={`p-${startPara}-${pIdx}`}>
            {qIdxOrZero(pIdx) && <br />}
            {renderInlineMarkdown(pl, `p-${startPara}-${pIdx}`, onOpenArtifact)}
          </React.Fragment>
        ))}
      </p>,
    );
  }

  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        gap: "2px",
        wordBreak: "break-word",
      }}
    >
      {blocks}
    </div>
  );
};

function qIdxOrZero(idx: number): boolean {
  return idx > 0;
}
