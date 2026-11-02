import React, { useEffect, useMemo, useState } from "react";
import {
  DEFAULT_WORKSPACE_AGENTS,
  resolveLivenessState,
  type AgentLivenessItem,
  type AgentRecord,
  type DiscoveredSessionSummary,
  type TranscriptTurn,
  type WireInboxItem,
  type WireMessage,
} from "../types/api";

export const COLLAPSED_PROJECTS_STORAGE_KEY = "fd_collapsed_projects";
export const PINNED_PROJECTS_STORAGE_KEY = "fd_pinned_projects";
export const PINNED_AGENTS_STORAGE_KEY = "fd_pinned_agents";

export type UnreadSeverity = "normal" | "question" | "important";

export interface AgentUnreadSummary {
  count: number;
  severity: UnreadSeverity;
}

export function mergeUnreadSeverity(
  a: UnreadSeverity,
  b: UnreadSeverity,
): UnreadSeverity {
  if (a === "important" || b === "important") return "important";
  if (a === "question" || b === "question") return "question";
  return "normal";
}

export function formatUnreadBadgeLabel(
  count: number,
  severity: UnreadSeverity = "normal",
): string {
  if (severity === "important") return `${count}!`;
  if (severity === "question") return `${count}?`;
  return `${count}`;
}

export function getUnreadBadgeClass(
  severity: UnreadSeverity = "normal",
): string {
  return `fd-badge fd-unread-badge fd-unread-badge-${severity}`;
}

export function getUnreadBadgeStyle(
  severity: UnreadSeverity = "normal",
  compact = false,
): React.CSSProperties {
  const base: React.CSSProperties = compact
    ? { fontWeight: 700, fontSize: "10px", padding: "1px 5px" }
    : { fontWeight: 700 };
  if (severity === "important") {
    return {
      ...base,
      backgroundColor: "var(--status-danger)",
      color: "#FFFFFF",
      borderColor: "var(--status-danger)",
    };
  }
  if (severity === "question") {
    return {
      ...base,
      backgroundColor: "var(--status-info)",
      color: "#0A0D12",
      borderColor: "var(--status-info)",
    };
  }
  return {
    ...base,
    backgroundColor: "var(--accent-primary)",
    color: "#0A0D12",
    borderColor: "var(--accent-primary)",
  };
}

export function classifyTextUnreadSeverity(text: string): UnreadSeverity {
  const trimmed = (text || "").trim();
  if (!trimmed) return "normal";
  const withoutNegated = trimmed.replace(
    /\b(no|zero|0|without)\s+(errors?|failures?|blockers?|failing|blocked)\b/gi,
    "",
  );
  if (
    /\b(blocker|blocked|urgent|error|failed|failure|fatal|panic)\b/i.test(
      withoutNegated,
    )
  ) {
    return "important";
  }
  const paragraphs = trimmed
    .split(/\n\s*\n/)
    .map((p) => p.trim())
    .filter(Boolean);
  const lastParagraph = paragraphs[paragraphs.length - 1] ?? trimmed;
  if (trimmed.endsWith("?") || lastParagraph.includes("?")) {
    return "question";
  }
  return "normal";
}

export function classifyTurnUnreadSeverity(
  turn: TranscriptTurn,
): UnreadSeverity {
  let severity: UnreadSeverity = "normal";
  for (const tc of turn.tool_calls ?? []) {
    const name = (tc.name || "").trim();
    if (/ask_question|AskUserQuestion|question/i.test(name)) {
      severity = mergeUnreadSeverity(severity, "question");
    }
  }
  return mergeUnreadSeverity(
    severity,
    classifyTextUnreadSeverity(turn.content || ""),
  );
}

export function classifyWireUnreadSeverity(
  item: WireInboxItem | Partial<WireMessage>,
): UnreadSeverity {
  const msg = "message" in item && item.message ? item.message : (item as Partial<WireMessage>);
  const urgency = (msg.urgency || "").toLowerCase();
  const kind = (msg.kind || "").toLowerCase();
  if (urgency === "urgent" || kind === "approval_request") {
    return "important";
  }
  const textSeverity = mergeUnreadSeverity(
    classifyTextUnreadSeverity(msg.subject || ""),
    classifyTextUnreadSeverity(msg.body || ""),
  );
  if (textSeverity === "important") {
    return "important";
  }
  if (kind === "ask" || textSeverity === "question") {
    return "question";
  }
  return "normal";
}

const pinListeners = new Set<() => void>();

function readStorageStringSet(storageKey: string): Set<string> {
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      const raw = window.localStorage.getItem(storageKey);
      if (raw) {
        const parsed = JSON.parse(raw);
        if (Array.isArray(parsed)) {
          return new Set(parsed.map((item) => String(item).trim().toLowerCase()).filter(Boolean));
        }
      }
    }
  } catch {
    // Ignore storage errors
  }
  return new Set();
}

function writeStorageStringSet(storageKey: string, values: Set<string>): void {
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      window.localStorage.setItem(storageKey, JSON.stringify(Array.from(values)));
    }
  } catch {
    // Ignore storage errors
  }
  for (const listener of pinListeners) {
    listener();
  }
}

export function readPinnedProjects(): Set<string> {
  return readStorageStringSet(PINNED_PROJECTS_STORAGE_KEY);
}

export function readPinnedAgents(): Set<string> {
  return readStorageStringSet(PINNED_AGENTS_STORAGE_KEY);
}

export function togglePinnedProject(project: string): boolean {
  const key = project.trim().toLowerCase();
  if (!key) return false;
  const next = readPinnedProjects();
  const pinned = !next.has(key);
  if (pinned) {
    next.add(key);
  } else {
    next.delete(key);
  }
  writeStorageStringSet(PINNED_PROJECTS_STORAGE_KEY, next);
  return pinned;
}

export function togglePinnedAgent(agentKey: string): boolean {
  const key = agentKey.trim().toLowerCase();
  if (!key) return false;
  const next = readPinnedAgents();
  const pinned = !next.has(key);
  if (pinned) {
    next.add(key);
  } else {
    next.delete(key);
  }
  writeStorageStringSet(PINNED_AGENTS_STORAGE_KEY, next);
  return pinned;
}

export interface PinIconProps {
  pinned?: boolean;
  circled?: boolean;
  size?: number;
}

export const PinIcon: React.FC<PinIconProps> = ({
  pinned = false,
  circled = true,
  size = 16,
}) => (
  <svg
    width={size}
    height={size}
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    strokeWidth="2"
    strokeLinecap="round"
    strokeLinejoin="round"
    aria-hidden="true"
    style={{ display: "inline-block", verticalAlign: "middle", flexShrink: 0 }}
  >
    {circled && <circle cx="12" cy="12" r="10" />}
    <path d="M9 7h6l-1 4 2 2v1H8v-1l2-2-1-4z" />
    <line x1="12" y1="14" x2="12" y2="18" />
    {pinned && (
      <line
        x1="4.93"
        y1="4.93"
        x2="19.07"
        y2="19.07"
        data-testid="pin-icon-slash"
      />
    )}
  </svg>
);

export const ProjectSettingsIcon: React.FC<{ size?: number }> = ({
  size = 17,
}) => (
  <svg
    width={size}
    height={size}
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    strokeWidth="2"
    strokeLinecap="round"
    strokeLinejoin="round"
    aria-hidden="true"
    data-testid="project-settings-icon"
    style={{ display: "inline-block", verticalAlign: "middle", flexShrink: 0 }}
  >
    {/* Distinct hexagonal project-level nut shell + inner precision cog ring */}
    <polygon
      points="12 2 20.66 7 20.66 17 12 22 3.34 17 3.34 7"
      fill="currentColor"
      fillOpacity="0.14"
    />
    <circle cx="12" cy="12" r="3.5" />
    <line x1="12" y1="6.5" x2="12" y2="8.5" />
    <line x1="12" y1="15.5" x2="12" y2="17.5" />
    <line x1="7.2" y1="9.2" x2="8.9" y2="10.2" />
    <line x1="15.1" y1="13.8" x2="16.8" y2="14.8" />
    <line x1="16.8" y1="9.2" x2="15.1" y2="10.2" />
    <line x1="8.9" y1="13.8" x2="7.2" y2="14.8" />
  </svg>
);

export function usePinnedState(): {
  pinnedProjects: Set<string>;
  pinnedAgents: Set<string>;
} {
  const [, setTick] = useState(0);
  useEffect(() => {
    const onChange = () => setTick((t) => t + 1);
    pinListeners.add(onChange);
    return () => {
      pinListeners.delete(onChange);
    };
  }, []);
  return {
    pinnedProjects: readPinnedProjects(),
    pinnedAgents: readPinnedAgents(),
  };
}

export interface RosterSidebarProps {
  agents: AgentRecord[];
  routes: DiscoveredSessionSummary[];
  liveness: AgentLivenessItem[];
  unreadByAgent: Record<string, number | AgentUnreadSummary>;
  unreadSeverityByAgent?: Record<string, UnreadSeverity>;
  selectedAgentName: string | null;
  onSelectAgent: (nameKey: string) => void;
  onOpenAgentSettings?: (nameKey: string) => void;
  onOpenProjectSettings?: (project: string) => void;
  onOpenSettingsModal?: () => void;
  onOpenHelpModal?: () => void;
  themeMode?: "dark" | "light";
  onToggleTheme?: () => void;
  onOpenSpinUpModal?: () => void;
  isOpen?: boolean;
  isCollapsedDesktop?: boolean;
  onClose?: () => void;
  leftPanelTab?: LeftPanelTab;
  onSelectLeftPanelTab?: (tab: LeftPanelTab) => void;
  consoleContent?: React.ReactNode;
  width?: number;
  onResizeStart?: (e: React.MouseEvent<HTMLDivElement>) => void;
}

export type LeftPanelTab = "roster" | "terminal" | "hub" | "gate" | "state";

export const LEFT_PANEL_TABS: Array<{ id: LeftPanelTab; label: string }> = [
  { id: "roster", label: "Agent Roster" },
  { id: "terminal", label: "Terminal" },
  { id: "hub", label: "Hub & Arch" },
  { id: "gate", label: "Vault & Gate" },
  { id: "state", label: "FerroState" },
];

function renderLeftPanelTabIcon(tabId: LeftPanelTab): React.ReactNode {
  switch (tabId) {
    case "roster":
      return (
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <circle cx="6" cy="5.25" r="2.25" stroke="currentColor" strokeWidth="1.4" />
          <path
            d="M2 13C2 10.9 3.79 9.25 6 9.25C8.21 9.25 10 10.9 10 13"
            stroke="currentColor"
            strokeWidth="1.4"
            strokeLinecap="round"
          />
          <circle cx="11.5" cy="5.75" r="1.75" stroke="currentColor" strokeWidth="1.3" />
          <path
            d="M11.2 9.5C12.85 9.65 14.1 10.95 14.1 12.6"
            stroke="currentColor"
            strokeWidth="1.3"
            strokeLinecap="round"
          />
        </svg>
      );
    case "terminal":
      return (
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <rect
            x="1.75"
            y="2.5"
            width="12.5"
            height="11"
            rx="1.8"
            stroke="currentColor"
            strokeWidth="1.4"
          />
          <path
            d="M4.5 6L6.75 8L4.5 10"
            stroke="currentColor"
            strokeWidth="1.4"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
          <line
            x1="8"
            y1="10"
            x2="11"
            y2="10"
            stroke="currentColor"
            strokeWidth="1.4"
            strokeLinecap="round"
          />
        </svg>
      );
    case "hub":
      return (
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <polygon
            points="8 1.75 13.5 4.9 13.5 11.1 8 14.25 2.5 11.1 2.5 4.9"
            stroke="currentColor"
            strokeWidth="1.4"
            strokeLinejoin="round"
          />
          <circle cx="8" cy="8" r="1.8" stroke="currentColor" strokeWidth="1.3" />
        </svg>
      );
    case "gate":
      return (
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <rect
            x="3.25"
            y="7.25"
            width="9.5"
            height="6.5"
            rx="1.5"
            stroke="currentColor"
            strokeWidth="1.4"
          />
          <path
            d="M5.25 7.25V5.25C5.25 3.73 6.48 2.5 8 2.5C9.52 2.5 10.75 3.73 10.75 5.25V7.25"
            stroke="currentColor"
            strokeWidth="1.4"
            strokeLinecap="round"
          />
        </svg>
      );
    case "state":
    default:
      return (
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <ellipse cx="8" cy="4" rx="5.25" ry="2" stroke="currentColor" strokeWidth="1.4" />
          <path
            d="M2.75 4V8C2.75 9.1 5.1 10 8 10C10.9 10 13.25 9.1 13.25 8V4"
            stroke="currentColor"
            strokeWidth="1.4"
          />
          <path
            d="M2.75 8V12C2.75 13.1 5.1 14 8 14C10.9 14 13.25 13.1 13.25 12V8"
            stroke="currentColor"
            strokeWidth="1.4"
          />
        </svg>
      );
  }
}

const CANONICAL_PROJECTS = [
  "AI-Lab-AMD",
  "FerroLoop",
  "Kaggle-Gemma4",
  "Lucida",
  "Prosodia",
  "Sonora",
  "Website",
];

interface EnrichedAgent {
  record: AgentRecord;
  route: DiscoveredSessionSummary | null;
  livenessItem: AgentLivenessItem | null;
  livenessState: "alive" | "stale" | "dead";
  adapter: string | null;
  unreadCount: number;
  unreadSeverity: UnreadSeverity;
}

export function formatRoleLabel(role: string): string {
  const clean = role.trim();
  if (clean === "lab_manager") {
    return "lab_manager";
  }
  return clean || "member";
}

export function getAdapterBadgeClass(adapter: string | null | undefined): string {
  if (!adapter) {
    return "fd-badge";
  }
  const lower = adapter.toLowerCase();
  if (lower.includes("muse")) return "fd-badge fd-adapter-muse";
  if (lower.includes("antigravity") || lower === "agy") return "fd-badge fd-adapter-antigravity";
  if (lower.includes("codex")) return "fd-badge fd-adapter-codex";
  if (lower.includes("claude")) return "fd-badge fd-adapter-claude";
  if (lower.includes("grok")) return "fd-badge fd-adapter-grok";
  return "fd-badge";
}

export const RosterSidebar: React.FC<RosterSidebarProps> = ({
  agents,
  routes,
  liveness,
  unreadByAgent,
  unreadSeverityByAgent,
  selectedAgentName,
  onSelectAgent,
  onOpenAgentSettings,
  onOpenProjectSettings,
  onOpenSettingsModal,
  onOpenHelpModal,
  themeMode = "dark",
  onToggleTheme,
  onOpenSpinUpModal,
  isOpen = false,
  isCollapsedDesktop = false,
  onClose,
  leftPanelTab,
  onSelectLeftPanelTab,
  consoleContent,
  width,
  onResizeStart,
}) => {
  const [internalLeftTab, setInternalLeftTab] = useState<LeftPanelTab>("roster");
  const effectiveLeftTab: LeftPanelTab = leftPanelTab ?? internalLeftTab;
  const handleSelectLeftTab = (tab: LeftPanelTab) => {
    setInternalLeftTab(tab);
    onSelectLeftPanelTab?.(tab);
  };
  const activeLeftLabel =
    LEFT_PANEL_TABS.find((t) => t.id === effectiveLeftTab)?.label ?? "Agent Roster";
  const [searchQuery, setSearchQuery] = useState("");
  const { pinnedProjects, pinnedAgents } = usePinnedState();
  const [collapsedProjects, setCollapsedProjects] = useState<Set<string>>(() => {
    try {
      if (typeof window !== "undefined" && window.localStorage) {
        const raw = window.localStorage.getItem(COLLAPSED_PROJECTS_STORAGE_KEY);
        if (raw) {
          const parsed = JSON.parse(raw);
          if (Array.isArray(parsed)) {
            return new Set(parsed.map((p) => String(p).toLowerCase()));
          }
        }
      }
    } catch {
      // Ignore storage errors
    }
    return new Set();
  });

  const toggleProjectGroup = (project: string) => {
    const key = project.toLowerCase();
    setCollapsedProjects((prev) => {
      const next = new Set(prev);
      if (next.has(key)) {
        next.delete(key);
      } else {
        next.add(key);
      }
      try {
        if (typeof window !== "undefined" && window.localStorage) {
          window.localStorage.setItem(
            COLLAPSED_PROJECTS_STORAGE_KEY,
            JSON.stringify(Array.from(next)),
          );
        }
      } catch {
        // Ignore storage errors
      }
      return next;
    });
  };

  const enrichedAgents = useMemo<EnrichedAgent[]>(() => {
    const baseAgents = agents.length > 0 ? agents : DEFAULT_WORKSPACE_AGENTS;

    const routeMap = new Map<string, DiscoveredSessionSummary>();
    for (const r of routes) {
      const personaKey = (r.persona ?? "").trim().toLowerCase();
      if (personaKey && !routeMap.has(personaKey)) {
        routeMap.set(personaKey, r);
      }
    }

    const livenessMap = new Map<string, AgentLivenessItem>();
    for (const item of liveness) {
      const key = item.agent.trim().toLowerCase();
      if (key) {
        livenessMap.set(key, item);
      }
    }

    const seenKeys = new Set<string>();
    const merged: AgentRecord[] = [];
    for (const ag of baseAgents) {
      const key = ag.name_key.trim().toLowerCase();
      if (!seenKeys.has(key)) {
        seenKeys.add(key);
        merged.push(ag);
      }
    }

    // Also include any agents reported in liveness or routes that weren't in baseAgents
    for (const [key, item] of livenessMap.entries()) {
      if (!seenKeys.has(key)) {
        seenKeys.add(key);
        merged.push({
          id: `urn:fl:agent:${key}`,
          name_key: key,
          display_name: item.agent,
          project: item.project || "Workspace",
          role: item.role || "assistant",
          status: "active",
        });
      }
    }

    return merged.map((record) => {
      const key = record.name_key.trim().toLowerCase();
      const route = routeMap.get(key) ?? null;
      const livenessItem = livenessMap.get(key) ?? null;
      const hasRoute = Boolean(route || livenessItem?.has_route);
      const livenessState = resolveLivenessState(livenessItem?.verdict, hasRoute);
      const adapter = route?.adapter ?? livenessItem?.adapter ?? null;
      const rawUnread = unreadByAgent[key] ?? unreadByAgent[record.name_key];
      const unreadCount =
        typeof rawUnread === "object" && rawUnread !== null
          ? rawUnread.count
          : typeof rawUnread === "number"
            ? rawUnread
            : 0;
      const unreadSeverity: UnreadSeverity =
        typeof rawUnread === "object" && rawUnread !== null
          ? rawUnread.severity
          : (unreadSeverityByAgent?.[key] ??
            unreadSeverityByAgent?.[record.name_key] ??
            "normal");

      return {
        record,
        route,
        livenessItem,
        livenessState,
        adapter,
        unreadCount,
        unreadSeverity,
      };
    });
  }, [agents, routes, liveness, unreadByAgent, unreadSeverityByAgent]);

  const groupedByProject = useMemo(() => {
    const query = searchQuery.trim().toLowerCase();
    const filtered = enrichedAgents.filter((item) => {
      if (!query) return true;
      const { record, adapter } = item;
      return (
        record.name_key.toLowerCase().includes(query) ||
        record.display_name.toLowerCase().includes(query) ||
        record.project.toLowerCase().includes(query) ||
        record.role.toLowerCase().includes(query) ||
        (adapter && adapter.toLowerCase().includes(query))
      );
    });

    const groups = new Map<string, EnrichedAgent[]>();
    for (const proj of CANONICAL_PROJECTS) {
      groups.set(proj, []);
    }

    for (const item of filtered) {
      const rawProj = (item.record.project || "Workspace").trim();
      const canonical =
        CANONICAL_PROJECTS.find(
          (p) => p.toLowerCase() === rawProj.toLowerCase(),
        ) ?? rawProj;
      const list = groups.get(canonical) ?? [];
      list.push(item);
      groups.set(canonical, list);
    }

    const entries = Array.from(groups.entries())
      .filter(([, list]) => list.length > 0)
      .map(([proj, list]) => {
        const sortedList = [...list].sort((a, b) => {
          const aPinned = pinnedAgents.has(a.record.name_key.trim().toLowerCase());
          const bPinned = pinnedAgents.has(b.record.name_key.trim().toLowerCase());
          if (aPinned !== bPinned) return aPinned ? -1 : 1;
          return a.record.display_name.localeCompare(b.record.display_name);
        });
        return [proj, sortedList] as [string, EnrichedAgent[]];
      });

    entries.sort(([projA], [projB]) => {
      const aPinned = pinnedProjects.has(projA.trim().toLowerCase());
      const bPinned = pinnedProjects.has(projB.trim().toLowerCase());
      if (aPinned !== bPinned) return aPinned ? -1 : 1;
      return projA.localeCompare(projB);
    });

    return entries;
  }, [enrichedAgents, searchQuery, pinnedProjects, pinnedAgents]);

  const selectedKey = (selectedAgentName ?? "").trim().toLowerCase();

  return (
    <>
      {isOpen && (
        <div
          className="fd-sidebar-backdrop"
          data-testid="mobile-sidebar-backdrop"
          onClick={onClose}
          aria-hidden="true"
        />
      )}
      <aside
        className={`fd-sidebar${isOpen ? " is-open" : ""}${isCollapsedDesktop ? " is-desktop-collapsed" : ""}`}
        aria-label="Agent Roster"
        data-testid="roster-sidebar"
        data-mobile-open={isOpen ? "true" : "false"}
        data-desktop-collapsed={isCollapsedDesktop ? "true" : "false"}
        data-left-panel-tab={effectiveLeftTab}
        style={
          width
            ? { width: `${width}px`, minWidth: `${width}px`, position: "relative" }
            : { position: "relative" }
        }
      >
        <div
          data-testid="left-panel-header"
          role="tablist"
          aria-label="Left Panel Views"
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: "8px",
            padding: "6px 10px",
            borderBottom: "1px solid var(--border-subtle)",
            backgroundColor: "var(--bg-surface)",
            flexShrink: 0,
            minHeight: "40px",
          }}
        >
          <div
            style={{
              display: "flex",
              alignItems: "center",
              gap: "6px",
              minWidth: 0,
            }}
          >
            <div
              style={{
                display: "flex",
                alignItems: "center",
                gap: "3px",
                flexShrink: 0,
              }}
            >
              {LEFT_PANEL_TABS.map((tab) => {
                const isActive = effectiveLeftTab === tab.id;
                return (
                  <button
                    key={tab.id}
                    type="button"
                    role="tab"
                    aria-selected={isActive}
                    aria-label={tab.label}
                    title={tab.label}
                    data-testid={`left-panel-tab-${tab.id}`}
                    className={`fd-btn${isActive ? " fd-btn-active" : ""}`}
                    style={{ padding: "5px 6px", lineHeight: 1 }}
                    onClick={() => handleSelectLeftTab(tab.id)}
                  >
                    {renderLeftPanelTabIcon(tab.id)}
                  </button>
                );
              })}
            </div>

            <span className="fd-tier-divider" style={{ height: "18px" }} aria-hidden="true" />

            <span
              data-testid="left-panel-active-title"
              style={{
                fontWeight: 600,
                fontSize: "12px",
                color: "var(--text-primary)",
                whiteSpace: "nowrap",
                overflow: "hidden",
                textOverflow: "ellipsis",
              }}
            >
              {activeLeftLabel}
            </span>
          </div>

          <div style={{ display: "flex", alignItems: "center", gap: "6px", flexShrink: 0 }}>
            <div
              style={{
                position: "absolute",
                width: "1px",
                height: "1px",
                padding: 0,
                margin: "-1px",
                overflow: "hidden",
                clip: "rect(0, 0, 0, 0)",
                whiteSpace: "nowrap",
                border: 0,
              }}
            >
              <span>FerroDeck</span>
              <span className="fd-badge" title="Registered Workspace Agents">
                {enrichedAgents.length} agents
              </span>
              {onOpenSpinUpModal && (
                <button
                  type="button"
                  className="fd-btn fd-btn-primary"
                  data-testid="spin-up-agent-btn"
                  onClick={onOpenSpinUpModal}
                  style={{ padding: "3px 8px", fontSize: "11px" }}
                >
                  + New Agent
                </button>
              )}
            </div>
            {onClose && (
              <button
                type="button"
                className="fd-btn"
                onClick={onClose}
                aria-label="Close Roster"
                style={{ padding: "2px 6px" }}
              >
                ✕
              </button>
            )}
          </div>
        </div>

        {effectiveLeftTab === "roster" ? (
          <>
            <div
              style={{
                padding: "8px 12px",
                borderBottom: "1px solid var(--border-subtle)",
                flexShrink: 0,
              }}
            >
              <input
                type="search"
                className="fd-input"
                placeholder="Search agents, projects, adapters..."
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                style={{ width: "100%" }}
              />
            </div>

            <div className="fd-scroll" style={{ padding: "10px 8px" }}>
        {groupedByProject.length === 0 ? (
          <div
            style={{
              padding: "20px 12px",
              color: "var(--text-muted)",
              textAlign: "center",
              fontSize: "12px",
            }}
          >
            No matching agents found.
          </div>
        ) : (
          groupedByProject.map(([project, items]) => {
            const projectLower = project.toLowerCase();
            const isGroupCollapsed = collapsedProjects.has(projectLower);
            const isGroupPinned = pinnedProjects.has(projectLower);
            const projectUnread = items.reduce((sum, a) => sum + a.unreadCount, 0);
            const projectSeverity = items.reduce<UnreadSeverity>(
              (acc, a) =>
                a.unreadCount > 0
                  ? mergeUnreadSeverity(acc, a.unreadSeverity)
                  : acc,
              "normal",
            );

            return (
              <div
                key={project}
                data-testid={`project-group-${projectLower}`}
                data-pinned={isGroupPinned ? "true" : "false"}
                style={{ marginBottom: "14px" }}
              >
                <div
                  data-testid={`project-header-row-${projectLower}`}
                  style={{
                    padding: "4px 10px",
                    border: "1px solid transparent",
                    fontSize: "11px",
                    fontWeight: 700,
                    textTransform: "uppercase",
                    letterSpacing: "0.05em",
                    color: "var(--text-muted)",
                    display: "flex",
                    justifyContent: "space-between",
                    alignItems: "center",
                    gap: "6px",
                  }}
                >
                  <button
                    type="button"
                    data-testid={`project-group-toggle-${projectLower}`}
                    aria-expanded={!isGroupCollapsed}
                    onClick={() => toggleProjectGroup(project)}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: "6px",
                      background: "transparent",
                      border: "none",
                      padding: 0,
                      color: "inherit",
                      fontSize: "inherit",
                      fontWeight: "inherit",
                      textTransform: "inherit",
                      letterSpacing: "inherit",
                      cursor: "pointer",
                      textAlign: "left",
                      flex: 1,
                      minWidth: 0,
                    }}
                  >
                    <span style={{ fontSize: "10px", width: "10px", display: "inline-block" }}>
                      {isGroupCollapsed ? "▸" : "▾"}
                    </span>
                    {isGroupPinned && (
                      <span
                        data-testid={`project-pinned-indicator-${projectLower}`}
                        title="Pinned Project"
                        style={{
                          display: "inline-flex",
                          alignItems: "center",
                          color: "var(--accent-primary)",
                        }}
                      >
                        <PinIcon circled={false} size={12} />
                      </span>
                    )}
                    <span
                      style={{
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                      }}
                    >
                      {project}
                    </span>
                  </button>

                  <div style={{ display: "flex", alignItems: "center", gap: "4px", flexShrink: 0 }}>
                    {projectUnread > 0 && (
                      <span
                        data-testid={`project-unread-badge-${projectLower}`}
                        data-severity={projectSeverity}
                        className={getUnreadBadgeClass(projectSeverity)}
                        title={`${projectUnread} unread message${projectUnread === 1 ? "" : "s"} in ${project}`}
                        style={getUnreadBadgeStyle(projectSeverity, true)}
                      >
                        {formatUnreadBadgeLabel(projectUnread, projectSeverity)}
                      </span>
                    )}
                    <button
                      type="button"
                      className="fd-gear-btn fd-roster-project-gear-btn"
                      data-testid={`project-settings-btn-${projectLower}`}
                      title={`Project settings for ${project}`}
                      aria-label={`Project settings for ${project}`}
                      onClick={(e) => {
                        e.stopPropagation();
                        onOpenProjectSettings?.(project);
                      }}
                      style={{
                        border: "none",
                        background: "transparent",
                        width: "24px",
                        height: "24px",
                        padding: 0,
                        display: "inline-flex",
                        alignItems: "center",
                        justifyContent: "center",
                        lineHeight: 1,
                        color: "var(--accent-primary)",
                        cursor: "pointer",
                        flexShrink: 0,
                      }}
                    >
                      <ProjectSettingsIcon size={17} />
                    </button>
                  </div>
                </div>

                {!isGroupCollapsed && (
                  <div style={{ display: "flex", flexDirection: "column", gap: "3px" }}>
                    {items.map(({ record, livenessState, adapter, unreadCount, unreadSeverity }) => {
                      const key = record.name_key.toLowerCase();
                      const isSelected = key === selectedKey;
                      const isAgentPinned = pinnedAgents.has(key);
                      const dotClass =
                        livenessState === "alive"
                          ? "fd-dot fd-dot-alive"
                          : livenessState === "stale"
                            ? "fd-dot fd-dot-stale"
                            : "fd-dot fd-dot-dead";

                      return (
                        <div
                          key={record.id || record.name_key}
                          role="button"
                          tabIndex={0}
                          data-testid={`agent-row-${key}`}
                          data-pinned={isAgentPinned ? "true" : "false"}
                          onClick={() => {
                            onSelectAgent(record.name_key);
                            onClose?.();
                          }}
                          onKeyDown={(e) => {
                            if (e.key === "Enter" || e.key === " ") {
                              e.preventDefault();
                              onSelectAgent(record.name_key);
                              onClose?.();
                            }
                          }}
                          style={{
                            width: "100%",
                            textAlign: "left",
                            padding: "8px 10px",
                            borderRadius: "var(--radius-md)",
                            border: isSelected
                              ? "1px solid var(--border-accent)"
                              : "1px solid transparent",
                            backgroundColor: isSelected ? "var(--accent-subtle)" : "transparent",
                            color: "var(--text-primary)",
                            cursor: "pointer",
                            display: "flex",
                            flexDirection: "column",
                            gap: "5px",
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
                            <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                              <span
                                data-testid={`liveness-dot-${key}`}
                                className={dotClass}
                                title={`Liveness: ${livenessState}`}
                              />
                              <span style={{ fontWeight: 600, fontSize: "13px" }}>
                                {record.display_name}
                              </span>
                              {isAgentPinned && (
                                <span
                                  data-testid={`agent-pinned-indicator-${key}`}
                                  title="Pinned Agent"
                                  style={{
                                    display: "inline-flex",
                                    alignItems: "center",
                                    color: "var(--accent-primary)",
                                  }}
                                >
                                  <PinIcon circled={false} size={13} />
                                </span>
                              )}
                            </div>

                            <div style={{ display: "flex", alignItems: "center", gap: "4px" }}>
                              {unreadCount > 0 && (
                                <span
                                  data-testid={`unread-badge-${key}`}
                                  data-severity={unreadSeverity}
                                  className={getUnreadBadgeClass(unreadSeverity)}
                                  style={getUnreadBadgeStyle(unreadSeverity, false)}
                                >
                                  {formatUnreadBadgeLabel(unreadCount, unreadSeverity)}
                                </span>
                              )}
                              <button
                                type="button"
                                className="fd-gear-btn fd-roster-gear-btn"
                                data-testid={`agent-settings-btn-${key}`}
                                title={`Settings for ${record.display_name}`}
                                aria-label={`Settings for ${record.display_name}`}
                                onClick={(e) => {
                                  e.stopPropagation();
                                  onSelectAgent(record.name_key);
                                  onOpenAgentSettings?.(record.name_key);
                                  onClose?.();
                                }}
                                style={{
                                  border: "none",
                                  background: "transparent",
                                  width: "24px",
                                  height: "24px",
                                  padding: 0,
                                  display: "inline-flex",
                                  alignItems: "center",
                                  justifyContent: "center",
                                  fontSize: "14px",
                                  lineHeight: 1,
                                  color: "var(--text-muted)",
                                  cursor: "pointer",
                                  flexShrink: 0,
                                }}
                              >
                                ⚙
                              </button>
                            </div>
                          </div>

                          <div
                            style={{
                              display: "flex",
                              alignItems: "center",
                              gap: "6px",
                              paddingLeft: "16px",
                              flexWrap: "wrap",
                            }}
                          >
                            <span className="fd-badge" data-testid={`role-badge-${key}`}>
                              {formatRoleLabel(record.role)}
                            </span>
                            {adapter && (
                              <span
                                data-testid={`adapter-pill-${key}`}
                                className={getAdapterBadgeClass(adapter)}
                              >
                                {adapter}
                              </span>
                            )}
                          </div>
                        </div>
                      );
                    })}
                  </div>
                )}
              </div>
            );
          })
        )}
            </div>
          </>
        ) : (
          <div
            data-testid="left-panel-console-body"
            style={{
              flex: 1,
              minHeight: 0,
              display: "flex",
              flexDirection: "column",
              overflow: "hidden",
            }}
          >
            {consoleContent}
          </div>
        )}

        <div
          data-testid="roster-global-controls"
          style={{
            display: "flex",
            alignItems: "center",
            gap: "6px",
            padding: "8px 12px",
            borderTop: "1px solid var(--border-subtle)",
            backgroundColor: "var(--bg-surface)",
            flexShrink: 0,
          }}
        >
          <button
            type="button"
            data-testid="settings-modal-btn"
            className="fd-btn"
            onClick={() => onOpenSettingsModal?.()}
            title="Open Settings (Agents, System, Auth)"
            style={{ flex: 1, justifyContent: "center", fontSize: "11.5px", padding: "4px 8px" }}
          >
            <span className="fd-gear-icon" aria-hidden="true" style={{ fontSize: "15px", lineHeight: 1 }}>
              ⚙
            </span>{" "}
            Settings
          </button>
          <button
            type="button"
            data-testid="context-help-btn"
            aria-label="Open Contextual Help"
            title="Contextual Help for current view"
            className="fd-btn fd-help-icon-btn"
            onClick={() => onOpenHelpModal?.()}
            style={{ padding: "4px 8px", fontSize: "11.5px" }}
          >
            ?
          </button>
          <button
            type="button"
            className="fd-btn"
            data-testid="theme-toggle-btn"
            onClick={() => onToggleTheme?.()}
            title={themeMode === "dark" ? "Switch to light theme" : "Switch to dark theme"}
            aria-label={themeMode === "dark" ? "Switch to light theme" : "Switch to dark theme"}
            style={{ padding: "4px 8px", fontSize: "12px" }}
          >
            {themeMode === "dark" ? "☀" : "☾"}
          </button>
        </div>

        <div
          data-testid="left-panel-resize-handle"
          role="separator"
          aria-orientation="vertical"
          aria-label="Resize left panel"
          title="Drag left or right to resize left panel"
          onMouseDown={onResizeStart}
          style={{
            position: "absolute",
            top: 0,
            right: "-3px",
            bottom: 0,
            width: "6px",
            cursor: "col-resize",
            zIndex: 25,
            userSelect: "none",
          }}
        />
      </aside>
    </>
  );
};
