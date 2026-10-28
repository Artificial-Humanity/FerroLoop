import React from "react";

export interface RichMarkdownProps {
  text: string;
}

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

export function renderInlineMarkdown(text: string, keyPrefix = "inline"): React.ReactNode[] {
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
          <span key={key} data-testid="file-reference-chip" title={url} style={FILE_CHIP_STYLE}>
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
        nodes.push(
          <a
            key={key}
            href={url}
            target="_blank"
            rel="noopener noreferrer"
            style={{
              color: "var(--accent-sky, #38BDF8)",
              textDecoration: "underline",
            }}
          >
            {renderInlineMarkdown(cleanedLabel, `${key}-lbl`)}
          </a>,
        );
      } else {
        nodes.push(
          <span key={key}>{renderInlineMarkdown(cleanedLabel, `${key}-lbl`)}</span>,
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
          {renderInlineMarkdown(inner, `${key}-b`)}
        </strong>,
      );
    } else if (token.startsWith("~~") && token.endsWith("~~") && token.length > 4) {
      const inner = token.slice(2, -2);
      nodes.push(<del key={key}>{renderInlineMarkdown(inner, `${key}-s`)}</del>);
    } else if (token.startsWith("*") && token.endsWith("*") && token.length > 2) {
      const inner = token.slice(1, -1);
      nodes.push(<em key={key}>{renderInlineMarkdown(inner, `${key}-i`)}</em>);
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

function renderListGroup(group: ParsedListGroup, key: string): React.ReactNode {
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
          {renderInlineMarkdown(item.content, `${key}-li-${idx}`)}
          {item.children.map((childGroup, cIdx) =>
            renderListGroup(childGroup, `${key}-li-${idx}-c-${cIdx}`),
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

export const RichMarkdown: React.FC<RichMarkdownProps> = ({ text }) => {
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
          {renderInlineMarkdown(headingText, `h-${i}`)}
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
                    {renderInlineMarkdown(header, `tbl-${i}-th-${hIdx}`)}
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
                      {renderInlineMarkdown(cell, `tbl-${i}-tr-${rIdx}-td-${cIdx}`)}
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
              {renderInlineMarkdown(ql, `bq-${startQuote}-${qIdx}`)}
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
        blocks.push(renderListGroup(group, `list-${i}-${gIdx}`));
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
            {pIdx > 0 && <br />}
            {renderInlineMarkdown(pl, `p-${startPara}-${pIdx}`)}
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
