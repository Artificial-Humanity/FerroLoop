import React, { useEffect, useMemo, useState } from "react";
import type { CockpitTab } from "../types/api";

export type HelpTopicId = CockpitTab | "lifecycle";

export const OPEN_HELP_EVENT = "ferrodeck:open-help";

export function dispatchOpenHelp(topic: HelpTopicId): void {
  if (typeof window !== "undefined") {
    window.dispatchEvent(
      new CustomEvent<{ topic: HelpTopicId }>(OPEN_HELP_EVENT, {
        detail: { topic },
      }),
    );
  }
}

export interface HelpTopicDoc {
  id: HelpTopicId;
  shortLabel: string;
  tier: "Agent" | "Project" | "System";
  title: string;
  summary: string;
  bullets: string[];
}

export const HELP_TOPICS: HelpTopicDoc[] = [
  {
    id: "chat",
    shortLabel: "Chat",
    tier: "Agent",
    title: "Agent Chat, Prompt Queue, Slash Commands (/) & Model Selector",
    summary:
      "Direct conversation and operator control for the selected agent session, including live streaming transcripts, queued prompts, and runtime model switching.",
    bullets: [
      "Prompt Queue: While an agent is busy, new operator prompts are queued automatically. Use Send Now, Edit, or Cancel on queued items, or click Interrupt (Stop) to halt the active turn.",
      "Forward-Slash (/) Commands: Type a leading '/' in the composer to open the autocomplete menu for built-in commands (/compact, /clear, /model, /goal, /plan, /boost) and shared skills.",
      "Agent Settings (⚙): Open per-agent settings to switch the active language model and thinking level, inspect context window and account quota usage, or run lifecycle actions.",
      "Collapsible Cards: Wire messages, tool outputs, and system notifications collapse by default into a single-line summary while keeping operator and directed assistant replies prominent.",
    ],
  },
  {
    id: "lifecycle",
    shortLabel: "Lifecycle",
    tier: "Agent",
    title: "Session Clear vs. Reforge & Agent Spin-Up / Decommission",
    summary:
      "Manage agent session state cleanly across context resets, fresh session creation, and graceful decommissioning.",
    bullets: [
      "Clear: Clears the agent's conversation context window and leaves the session blank without dispatching a startup prompt.",
      "Reforge: Clears the agent's context window and immediately re-primes the session with its registered persona identity, project assignment, and role instructions.",
      "Prepare & Decommission vs. Tear Down: Prepare & Decommission dispatches a pre-decommission hygiene handshake so the agent checkpoints STATE.md/FerroState and cleans worktrees before exit; Tear Down immediately stops the runtime session and unregisters the route.",
      "New Agent (+ New Agent): Launches a new tmux-backed or hosted agent session for a persona, project, role, vendor adapter, and target node.",
    ],
  },
  {
    id: "mail",
    shortLabel: "Wire Mail",
    tier: "Agent",
    title: "FerroWire Inter-Agent Mail & Broadcasts",
    summary:
      "Durable NATS JetStream and Hub-routed messaging between project Leads, Assisting Engineers, and the Lab Manager.",
    bullets: [
      "Agent & Broadcast Addressing: Send point-to-point messages to any registered persona (e.g. ferris, jarvis, kayla) or broadcast to all active agents.",
      "Delivery Receipts & Acknowledgment: Track pending, delivered, and acknowledged receipts per message and acknowledge unread inbox items directly from the cockpit.",
      "Urgency & Threading: Filter messages by recipient, unread state, or thread ID, and compose replies preserving thread correlation.",
    ],
  },
  {
    id: "artifacts",
    shortLabel: "Artifacts",
    tier: "Project",
    title: "Project Artifacts, Mermaid Diagrams & Dead-Path Pruning",
    summary:
      "Inspect, render, zoom, and download project documents, architecture diagrams, images, PDFs, and source files.",
    bullets: [
      "Rich Rendering: Preview Markdown documents with embedded Mermaid diagrams, interactive SVG/image zoom (50%–200% or Fit), PDF iframes, and line-numbered source code.",
      "User Documents vs. All Files: Filter the quick-switcher to user-facing documents and media (.md, .mmd, .png, .svg, .pdf) or expand to all discovered source paths.",
      "Dead-Path Pruning & Archived Artifacts: Click 'Prune Dead Paths' to clean up stale references from removed worktrees; artifacts preserved in the hub archive display an 'Archived' badge.",
    ],
  },
  {
    id: "git",
    shortLabel: "Git & PRs",
    tier: "Project",
    title: "Git Worktrees, Diffs & Author-Owned PR Governance",
    summary:
      "Monitor active git worktrees, inspect unified diffs, and execute role-governed pull-request reviews and author-owned merges.",
    bullets: [
      "Worktree Inspector: View branch heads, clean/dirty status, and per-file unified diffs across all active project worktrees.",
      "PR Review & Merge Governance: Subordinate agents request review from their Project Lead; Project Leads request review from the Lab Manager (jarvis).",
      "Author-Owned Merges: Once approved via the Reviewer GitHub App and all CI checks pass, the PR author merges their own pull request and cleans up the worktree.",
    ],
  },
  {
    id: "board",
    shortLabel: "Records & Board",
    tier: "Project",
    title: "Project Records, Kanban Swimlanes & Velocity Strip",
    summary:
      "Manage project records across Todo, Doing, and Done Kanban swimlanes with gated state transitions and real-time velocity metrics.",
    bullets: [
      "3-Column Kanban Swimlanes: Inspect records grouped by state (Todo, Doing, Done) with display handles (#id), area tags, and open findings badges.",
      "Drag-and-Drop & Quick-Move Transitions: Drag record cards between swimlanes or click direct quick-move buttons (→ Todo, → Doing, → Done) to execute fl record move transitions.",
      "Velocity & Completion Metrics Strip: Track Todo/Doing/Done counts, overall completion percentage with a stacked progress bar, open findings, and execution attempts per project.",
    ],
  },
  {
    id: "terminal",
    shortLabel: "Terminal",
    tier: "System",
    title: "Interactive PTY Terminal & Quick Commands",
    summary:
      "WebSocket-streamed pseudo-terminal connected to the hub host or active worktree directory.",
    bullets: [
      "Live PTY Session: Execute shell commands, inspect logs, or check git and service status directly from the browser.",
      "Quick Commands & Mobile Keys: Dispatch one-click diagnostic commands (git status, fl status, agent-msg roster) and special keys (Ctrl+C, Tab, arrows) on mobile devices.",
    ],
  },
  {
    id: "hub",
    shortLabel: "Hub & Arch",
    tier: "System",
    title: "Hub Telemetry, Enrolled Nodes & Architecture Conformance",
    summary:
      "System-wide orchestration telemetry, multi-step workflow execution, regret audits, and LikeC4 architecture conformance.",
    bullets: [
      "Workflows & Step Runs: Start, monitor, approve, or reject gated multi-agent workflow runs and inspect per-step token and cost telemetry.",
      "Architecture Conformance: Verify crate/package boundaries and dependency edges against the declared system model.",
      "Enrolled Nodes: Generate short-lived node enrollment tokens and monitor online/stale worker nodes across the lab.",
    ],
  },
  {
    id: "gate",
    shortLabel: "Vault & Gate",
    tier: "System",
    title: "FerroGate Zero-Knowledge Vault, Proxy & Tiered MCP Registry",
    summary:
      "Zero-knowledge credential vault and outbound proxy that injects secrets at the network boundary without exposing plaintext to agents.",
    bullets: [
      "Zero-Knowledge Secret Metadata: Manage secret host allowlists, HTTP method restrictions, agent ACLs, and lease TTLs without ever returning plaintext values to the UI.",
      "Connectors & Bridges: Inspect configured outbound API connectors, tool schemas, and inbound webhook/chat bridges.",
    ],
  },
  {
    id: "state",
    shortLabel: "FerroState",
    tier: "System",
    title: "FerroState Memory Search, Checkpoints & Code Index",
    summary:
      "Git-backed structured memory, handoffs, runtime state entries, and tree-sitter symbol search with token-gain tracking.",
    bullets: [
      "Scoped Memories & Handoffs: Browse, search, and upsert workspace-, project-, and agent-scoped memory and handoff entries.",
      "State Checkpoints: Create point-in-time state checkpoints before context compaction or session handoff.",
      "Code Symbol Index: Run definition, search, and outline queries across indexed repositories and track cumulative tokens saved.",
    ],
  },
];

export interface HelpDrawerProps {
  isOpen: boolean;
  activeTopic?: HelpTopicId;
  onSelectTopic?: (topic: HelpTopicId) => void;
  onClose: () => void;
}

export const HelpDrawer: React.FC<HelpDrawerProps> = ({
  isOpen,
  activeTopic = "chat",
  onSelectTopic,
  onClose,
}) => {
  const [selectedTopic, setSelectedTopic] = useState<HelpTopicId>(activeTopic);
  const [searchQuery, setSearchQuery] = useState<string>("");

  useEffect(() => {
    if (activeTopic) {
      setSelectedTopic(activeTopic);
    }
  }, [activeTopic, isOpen]);

  useEffect(() => {
    if (!isOpen) {
      setSearchQuery("");
      return;
    }
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        onClose();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [isOpen, onClose]);

  const filteredTopics = useMemo(() => {
    const q = searchQuery.trim().toLowerCase();
    if (!q) {
      return HELP_TOPICS;
    }
    return HELP_TOPICS.filter(
      (topic) =>
        topic.title.toLowerCase().includes(q) ||
        topic.shortLabel.toLowerCase().includes(q) ||
        topic.summary.toLowerCase().includes(q) ||
        topic.bullets.some((b) => b.toLowerCase().includes(q)),
    );
  }, [searchQuery]);

  if (!isOpen) {
    return null;
  }

  const currentDoc =
    HELP_TOPICS.find((t) => t.id === selectedTopic) ?? HELP_TOPICS[0];

  const handleChooseTopic = (topicId: HelpTopicId) => {
    setSelectedTopic(topicId);
    setSearchQuery("");
    onSelectTopic?.(topicId);
  };

  return (
    <div className="fd-modal-backdrop" onClick={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        aria-label="FerroDeck Documentation & Help"
        data-testid="contextual-help-drawer"
        className="fd-modal"
        onClick={(e) => e.stopPropagation()}
        style={{
          maxWidth: "680px",
          maxHeight: "86vh",
          display: "flex",
          flexDirection: "column",
          gap: "12px",
          overflow: "hidden",
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: "8px",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
            <h2 style={{ margin: 0, fontSize: "16px", fontWeight: 700 }}>
              FerroDeck Documentation &amp; Contextual Help
            </h2>
            <span className="fd-badge">{currentDoc.tier} Tier</span>
          </div>
          <button
            type="button"
            data-testid="close-help-drawer-btn"
            className="fd-btn"
            onClick={onClose}
            aria-label="Close Contextual Help"
          >
            ✕
          </button>
        </div>

        <input
          type="search"
          data-testid="help-search-input"
          aria-label="Search documentation topics"
          placeholder="Search help topics, slash commands, Clear vs. Reforge, PR workflow, FerroGate..."
          className="fd-input"
          value={searchQuery}
          onChange={(e) => setSearchQuery(e.target.value)}
          style={{ width: "100%", fontSize: "12.5px" }}
        />

        <div
          role="navigation"
          aria-label="Help Topics"
          style={{
            display: "flex",
            alignItems: "center",
            gap: "6px",
            flexWrap: "wrap",
          }}
        >
          {HELP_TOPICS.map((topic) => {
            const isSelected = selectedTopic === topic.id && !searchQuery.trim();
            return (
              <button
                key={topic.id}
                type="button"
                data-testid={`help-topic-${topic.id}`}
                aria-pressed={isSelected}
                className={`fd-btn${isSelected ? " fd-btn-active" : ""}`}
                onClick={() => handleChooseTopic(topic.id)}
                style={{ fontSize: "11.5px", padding: "4px 9px" }}
              >
                {topic.shortLabel}
              </button>
            );
          })}
        </div>

        <div
          className="fd-scroll"
          style={{
            padding: "4px 2px",
            display: "flex",
            flexDirection: "column",
            gap: "10px",
            overflowY: "auto",
          }}
        >
          {searchQuery.trim() ? (
            filteredTopics.length > 0 ? (
              filteredTopics.map((topic) => (
                <article
                  key={topic.id}
                  data-testid={`help-section-${topic.id}`}
                  className="fd-card"
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
                    }}
                  >
                    <h3 style={{ margin: 0, fontSize: "14px", fontWeight: 700 }}>
                      {topic.title}
                    </h3>
                    <span className="fd-badge">{topic.tier}</span>
                  </div>
                  <p
                    style={{
                      margin: 0,
                      fontSize: "12.5px",
                      color: "var(--text-secondary)",
                    }}
                  >
                    {topic.summary}
                  </p>
                  <ul
                    style={{
                      margin: 0,
                      paddingLeft: "18px",
                      display: "flex",
                      flexDirection: "column",
                      gap: "5px",
                      fontSize: "12px",
                      color: "var(--text-primary)",
                    }}
                  >
                    {topic.bullets.map((bullet, idx) => (
                      <li key={idx}>{bullet}</li>
                    ))}
                  </ul>
                </article>
              ))
            ) : (
              <div
                data-testid="help-search-empty"
                className="fd-card"
                style={{
                  textAlign: "center",
                  color: "var(--text-muted)",
                  fontSize: "12.5px",
                  padding: "20px",
                }}
              >
                No documentation topics matched &ldquo;{searchQuery}&rdquo;.
              </div>
            )
          ) : (
            <article
              data-testid="help-active-topic-content"
              data-topic={currentDoc.id}
              className="fd-card"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "10px",
                borderColor: "var(--border-accent)",
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
                <h3 style={{ margin: 0, fontSize: "15px", fontWeight: 700 }}>
                  {currentDoc.title}
                </h3>
                <span className="fd-badge">{currentDoc.tier} Context</span>
              </div>
              <p
                style={{
                  margin: 0,
                  fontSize: "13px",
                  color: "var(--text-secondary)",
                  lineHeight: 1.5,
                }}
              >
                {currentDoc.summary}
              </p>
              <ul
                style={{
                  margin: 0,
                  paddingLeft: "18px",
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
                  fontSize: "12.5px",
                  lineHeight: 1.5,
                  color: "var(--text-primary)",
                }}
              >
                {currentDoc.bullets.map((bullet, idx) => (
                  <li key={idx}>{bullet}</li>
                ))}
              </ul>
            </article>
          )}
        </div>
      </div>
    </div>
  );
};
