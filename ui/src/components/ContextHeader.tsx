import React from "react";
import type {
  AgentRecord,
  CockpitTab,
  DiscoveredSessionSummary,
  HubAuthority,
} from "../types/api";
import { formatRoleLabel, getAdapterBadgeClass } from "./RosterSidebar";

export type ContextTier = "agent" | "project" | "system";

export interface ContextHeaderProps {
  selectedAgent: AgentRecord | null;
  sessions: DiscoveredSessionSummary[];
  selectedSessionId: string | null;
  onSelectSession: (sessionId: string) => void;
  activeTab: CockpitTab;
  onSelectTab: (tab: CockpitTab) => void;
  splitView?: boolean;
  onToggleSplitView?: () => void;
  themeMode?: "dark" | "light";
  onToggleTheme?: () => void;
  authority?: HubAuthority | null;
  wsConnected?: boolean;
  onlineNodesCount?: number;
  onOpenNodesModal?: () => void;
  onOpenSettingsModal?: () => void;
  onOpenHelpModal?: (topic?: CockpitTab) => void;
  onOpenAgentSettings?: () => void;
  onOpenSpinUpModal?: () => void;
  onOpenPairingModal: () => void;
  onLogout?: () => void;
  onOpenMobileRoster?: () => void;
  mobileSidebarOpen?: boolean;
}

export const COCKPIT_TABS: Array<{ id: CockpitTab; label: string }> = [
  { id: "chat", label: "Chat" },
  { id: "artifacts", label: "Artifacts" },
  { id: "git", label: "Git & PRs" },
  { id: "board", label: "Records & Board" },
  { id: "mail", label: "Wire Mail" },
  { id: "terminal", label: "Terminal" },
  { id: "hub", label: "Hub & Arch" },
  { id: "gate", label: "Vault & Gate" },
  { id: "state", label: "FerroState" },
];

export const AGENT_TIER_TABS: Array<{ id: CockpitTab; label: string }> = [
  { id: "chat", label: "Chat" },
  { id: "mail", label: "Wire Mail" },
];

export const PROJECT_TIER_TABS: Array<{ id: CockpitTab; label: string }> = [
  { id: "artifacts", label: "Artifacts" },
  { id: "git", label: "Git & PRs" },
  { id: "board", label: "Records & Board" },
];

export const SYSTEM_TIER_TABS: Array<{ id: CockpitTab; label: string }> = [
  { id: "terminal", label: "Terminal" },
  { id: "hub", label: "Hub & Arch" },
  { id: "gate", label: "Vault & Gate" },
  { id: "state", label: "FerroState" },
];

export const AGENT_TABS = AGENT_TIER_TABS;
export const PROJECT_TABS = PROJECT_TIER_TABS;
export const SYSTEM_TABS = SYSTEM_TIER_TABS;

export const RIGHT_PANEL_TABS: Array<{
  id: Exclude<CockpitTab, "chat">;
  label: string;
  requiresAgent?: boolean;
}> = [
  { id: "mail", label: "Wire Mail", requiresAgent: true },
  { id: "artifacts", label: "Artifacts" },
  { id: "git", label: "Git & PRs" },
  { id: "board", label: "Records & Board" },
];

function renderRightPanelTabIcon(tabId: Exclude<CockpitTab, "chat">): React.ReactNode {
  switch (tabId) {
    case "mail":
      return (
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <rect
            x="1.5"
            y="3"
            width="13"
            height="10"
            rx="1.8"
            stroke="currentColor"
            strokeWidth="1.4"
          />
          <path
            d="M2.2 4L8 8.2L13.8 4"
            stroke="currentColor"
            strokeWidth="1.4"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
      );
    case "artifacts":
      return (
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <path
            d="M4 1.75H9.5L12.5 4.75V13.25C12.5 13.8 12.05 14.25 11.5 14.25H4C3.45 14.25 3 13.8 3 13.25V2.75C3 2.2 3.45 1.75 4 1.75Z"
            stroke="currentColor"
            strokeWidth="1.4"
            strokeLinejoin="round"
          />
          <path
            d="M9.25 1.75V5H12.5"
            stroke="currentColor"
            strokeWidth="1.4"
            strokeLinejoin="round"
          />
          <line
            x1="5.25"
            y1="8"
            x2="10.25"
            y2="8"
            stroke="currentColor"
            strokeWidth="1.3"
            strokeLinecap="round"
          />
          <line
            x1="5.25"
            y1="10.75"
            x2="8.75"
            y2="10.75"
            stroke="currentColor"
            strokeWidth="1.3"
            strokeLinecap="round"
          />
        </svg>
      );
    case "git":
      return (
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <circle cx="4.25" cy="3.75" r="1.75" stroke="currentColor" strokeWidth="1.4" />
          <circle cx="4.25" cy="12.25" r="1.75" stroke="currentColor" strokeWidth="1.4" />
          <circle cx="11.75" cy="12.25" r="1.75" stroke="currentColor" strokeWidth="1.4" />
          <line x1="4.25" y1="5.5" x2="4.25" y2="10.5" stroke="currentColor" strokeWidth="1.4" />
          <path
            d="M8.5 3.75H10.25C11.08 3.75 11.75 4.42 11.75 5.25V10.5"
            stroke="currentColor"
            strokeWidth="1.4"
            strokeLinecap="round"
          />
          <path
            d="M9.75 2.25L8.25 3.75L9.75 5.25"
            stroke="currentColor"
            strokeWidth="1.4"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
      );
    case "board":
    default:
      return (
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
          <rect
            x="2"
            y="2.25"
            width="3.2"
            height="11.5"
            rx="1"
            stroke="currentColor"
            strokeWidth="1.35"
          />
          <rect
            x="6.4"
            y="2.25"
            width="3.2"
            height="7.5"
            rx="1"
            stroke="currentColor"
            strokeWidth="1.35"
          />
          <rect
            x="10.8"
            y="2.25"
            width="3.2"
            height="9.5"
            rx="1"
            stroke="currentColor"
            strokeWidth="1.35"
          />
        </svg>
      );
  }
}

export interface RightPanelHeaderProps {
  selectedAgent: AgentRecord | null;
  rightSplitTab: Exclude<CockpitTab, "chat">;
  onSelectTab: (tab: Exclude<CockpitTab, "chat">) => void;
  onClose?: () => void;
}

export const RightPanelHeader: React.FC<RightPanelHeaderProps> = ({
  selectedAgent,
  rightSplitTab,
  onSelectTab,
  onClose,
}) => {
  const effectiveTab: Exclude<CockpitTab, "chat"> =
    !selectedAgent && rightSplitTab === "mail" ? "artifacts" : rightSplitTab;
  const visibleTabs = React.useMemo(
    () => RIGHT_PANEL_TABS.filter((tab) => !tab.requiresAgent || Boolean(selectedAgent)),
    [selectedAgent],
  );
  const activeLabel =
    COCKPIT_TABS.find((t) => t.id === effectiveTab)?.label ?? effectiveTab;
  const contextSubtitle =
    effectiveTab === "mail"
      ? (selectedAgent?.display_name ?? "Agent")
      : getTabContextTier(effectiveTab) === "project"
        ? (selectedAgent?.project ?? "Workspace")
        : "AI-Lab-0";

  return (
    <div
      data-testid="right-panel-header"
      role="tablist"
      aria-label="Right Panel Views"
      style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "space-between",
        gap: "8px",
        padding: "6px 12px",
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
          gap: "8px",
          minWidth: 0,
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "4px",
            flexShrink: 0,
          }}
        >
          {visibleTabs.map((tab) => {
            const isActive = effectiveTab === tab.id;
            return (
              <button
                key={tab.id}
                type="button"
                role="tab"
                aria-selected={isActive}
                aria-label={tab.label}
                title={tab.label}
                data-testid={`right-panel-tab-${tab.id}`}
                className={`fd-btn${isActive ? " fd-btn-active" : ""}`}
                style={{ padding: "5px 7px", lineHeight: 1 }}
                onClick={() => onSelectTab(tab.id)}
              >
                {renderRightPanelTabIcon(tab.id)}
              </button>
            );
          })}
        </div>

        <span className="fd-tier-divider" style={{ height: "18px" }} aria-hidden="true" />

        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "6px",
            minWidth: 0,
            overflow: "hidden",
          }}
        >
          <span
            data-testid="right-panel-active-title"
            style={{
              fontWeight: 600,
              fontSize: "12.5px",
              color: "var(--text-primary)",
              whiteSpace: "nowrap",
              overflow: "hidden",
              textOverflow: "ellipsis",
            }}
          >
            {activeLabel}
          </span>
          <span
            className="fd-badge"
            style={{
              fontSize: "10.5px",
              whiteSpace: "nowrap",
              flexShrink: 0,
            }}
          >
            {contextSubtitle}
          </span>
        </div>
      </div>

      {onClose && (
        <button
          type="button"
          className="fd-btn"
          data-testid="right-panel-close-btn"
          aria-label="Close right split panel"
          title="Close right split panel"
          style={{ padding: "4px 7px", fontSize: "12px", lineHeight: 1, flexShrink: 0 }}
          onClick={onClose}
        >
          ✕
        </button>
      )}
    </div>
  );
};

export function getTabContextTier(tab: CockpitTab): ContextTier {
  switch (tab) {
    case "chat":
    case "mail":
      return "agent";
    case "artifacts":
    case "git":
    case "board":
      return "project";
    case "terminal":
    case "hub":
    case "gate":
    case "state":
    default:
      return "system";
  }
}

export const ContextHeader: React.FC<ContextHeaderProps> = ({
  selectedAgent,
  sessions,
  selectedSessionId,
  onSelectSession,
  activeTab,
  onSelectTab,
  splitView = false,
  onToggleSplitView,
  themeMode = "dark",
  onToggleTheme,
  authority,
  wsConnected = false,
  onlineNodesCount = 0,
  onOpenNodesModal,
  onOpenSettingsModal,
  onOpenHelpModal,
  onOpenAgentSettings,
  onOpenSpinUpModal,
  onOpenPairingModal,
  onLogout,
  onOpenMobileRoster,
  mobileSidebarOpen = false,
}) => {
  const agentSessions = React.useMemo(() => {
    if (!selectedAgent) return sessions;
    const key = selectedAgent.name_key.toLowerCase();
    const matching = sessions.filter((s) => (s.persona ?? "").toLowerCase() === key);
    return matching.length > 0 ? matching : sessions;
  }, [selectedAgent, sessions]);

  const activeSession =
    agentSessions.find((s) => s.id === selectedSessionId) ?? agentSessions[0] ?? null;

  const activeTier = getTabContextTier(activeTab);

  const scopeBannerText = React.useMemo(() => {
    if (activeTier === "agent") {
      return `Scope: Agent · ${selectedAgent?.display_name ?? "Unassigned"}`;
    }
    if (activeTier === "project") {
      return `Scope: Project · ${selectedAgent?.project ?? "Workspace"}`;
    }
    return "Scope: System · AI-Lab-0";
  }, [activeTier, selectedAgent]);

  const authorityText = React.useMemo(() => {
    if (!authority) return "Unauthenticated";
    if (authority.tier === "owner") {
      return `Owner: ${authority.login}`;
    }
    return authority.agent ? `Peer: ${authority.agent}` : "Peer";
  }, [authority]);

  const renderTabButton = (tab: { id: CockpitTab; label: string }) => {
    const isActive =
      tab.id === "chat"
        ? activeTab === "chat" && !splitView
        : splitView && activeTab === tab.id;
    return (
      <button
        key={tab.id}
        type="button"
        role="tab"
        aria-selected={isActive}
        className={`fd-btn${isActive ? " fd-btn-active" : ""}`}
        onClick={() => onSelectTab(tab.id)}
      >
        {tab.label}
      </button>
    );
  };

  return (
    <header className="fd-header" data-testid="context-header">
      <div
        role="tablist"
        aria-label="Cockpit Modes"
        data-testid="cockpit-tab-bar"
        className="fd-tab-bar fd-three-tier-bar"
      >
        <span
          className={`fd-badge fd-scope-pill fd-scope-pill-${activeTier}`}
          data-testid="active-context-tier-badge"
          data-tier={activeTier}
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
          {scopeBannerText}
        </span>

        {onOpenMobileRoster && (
          <button
            type="button"
            className={`fd-btn fd-mobile-sidebar-toggle${mobileSidebarOpen ? " fd-btn-active" : ""}`}
            onClick={onOpenMobileRoster}
            aria-label="Toggle Agent Roster"
            title="Toggle Agent Roster"
            aria-expanded={mobileSidebarOpen}
            data-testid="mobile-sidebar-toggle-btn"
            style={{ flexShrink: 0 }}
          >
            <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
              <rect
                x="1.5"
                y="2.5"
                width="13"
                height="11"
                rx="2"
                stroke="currentColor"
                strokeWidth="1.4"
              />
              <line
                x1="6.5"
                y1="2.5"
                x2="6.5"
                y2="13.5"
                stroke="currentColor"
                strokeWidth="1.4"
              />
              <rect
                x="2.2"
                y="3.2"
                width="3.6"
                height="9.6"
                rx="0.8"
                fill="currentColor"
                fillOpacity={mobileSidebarOpen ? 0.45 : 0.12}
              />
            </svg>
          </button>
        )}

        {/* Tier 1: Agent Context */}
        <div
          className={`fd-tier-group fd-tier-agent${activeTier === "agent" || activeTier === "project" ? " is-active-tier" : ""}`}
          data-testid="tier-agent-nav"
          data-active-tier={activeTier === "agent" || activeTier === "project" ? "true" : undefined}
        >
          <span className="fd-tier-label" title="Active Agent">
            Agent
          </span>

          {selectedAgent ? (
            <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
              <span style={{ fontWeight: 700, fontSize: "14px" }} title="Active Agent Name">
                {selectedAgent.display_name}
              </span>
              <span className="fd-badge" title="Agent Role">
                {formatRoleLabel(selectedAgent.role)}
              </span>
            </div>
          ) : (
            <span
              style={{ fontWeight: 600, color: "var(--text-muted)" }}
              title="Active Agent Name"
            >
              No Agent Selected
            </span>
          )}

          {agentSessions.length > 0 && (
            <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
              {agentSessions.length > 1 && (
                <select
                  aria-label="Active Session"
                  title="Switch Active Session"
                  data-testid="session-selector"
                  className="fd-select fd-mono"
                  style={{ fontSize: "11px", padding: "4px 8px" }}
                  value={activeSession?.id ?? ""}
                  onChange={(e) => onSelectSession(e.target.value)}
                >
                  {agentSessions.map((sess) => (
                    <option key={sess.id} value={sess.id}>
                      [{sess.adapter}] {sess.persona || "session"} ({sess.id.slice(-8)})
                    </option>
                  ))}
                </select>
              )}

              {activeSession?.adapter && (
                <span
                  className={getAdapterBadgeClass(activeSession.adapter)}
                  title="Agent Harness / Adapter"
                >
                  {activeSession.adapter}
                </span>
              )}
            </div>
          )}

          <span
            className="fd-badge"
            data-testid="active-project-badge"
            title="Current Project"
          >
            {selectedAgent?.project ?? "Workspace"}
          </span>

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
            {renderTabButton({ id: "chat", label: "Chat" })}
          </div>
        </div>

        {/* Tier 3: System Context (consoles, Nodes, Spin Up, and Auth live inside SettingsModal; global controls live in RosterSidebar & ChatPane) */}
        <div
          className={`fd-tier-group fd-tier-system${activeTier === "system" ? " is-active-tier" : ""}`}
          data-testid="tier-system-nav"
          data-active-tier={activeTier === "system" ? "true" : undefined}
          style={
            activeTier === "system"
              ? { position: "relative" }
              : {
                  position: "absolute",
                  width: "1px",
                  height: "1px",
                  padding: 0,
                  margin: "-1px",
                  overflow: "hidden",
                  clip: "rect(0, 0, 0, 0)",
                  whiteSpace: "nowrap",
                  border: 0,
                }
          }
        >
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
            <span className="fd-tier-label">System</span>
            <div style={{ display: "flex", alignItems: "center", gap: "4px" }}>
              {SYSTEM_TIER_TABS.map(renderTabButton)}
            </div>
            <span
              className="fd-badge"
              title={wsConnected ? "WebSocket Connected" : "WebSocket Disconnected"}
            >
              <span className={`fd-dot ${wsConnected ? "fd-dot-alive" : "fd-dot-dead"}`} />
              WS
            </span>
            {onOpenNodesModal && (
              <button
                type="button"
                data-testid="nodes-modal-btn"
                className="fd-btn"
                onClick={onOpenNodesModal}
              >
                <span className={`fd-dot ${onlineNodesCount > 0 ? "fd-dot-alive" : "fd-dot-dead"}`} />
                <span>{onlineNodesCount > 0 ? `Nodes (${onlineNodesCount})` : "Nodes"}</span>
              </button>
            )}
            <button
              type="button"
              data-testid="header-spin-up-btn"
              className="fd-btn"
              onClick={() => onOpenSpinUpModal?.()}
              title="Create a new agent session"
            >
              + New Agent
            </button>
            <button
              type="button"
              data-testid="authority-status-btn"
              className="fd-btn"
              onClick={onOpenPairingModal}
            >
              <span
                className={`fd-dot ${
                  authority?.tier === "owner"
                    ? "fd-dot-alive"
                    : authority?.tier === "peer"
                      ? "fd-dot-stale"
                      : "fd-dot-dead"
                }`}
              />
              <span>{authorityText}</span>
            </button>
            {authority && onLogout && (
              <button type="button" className="fd-btn fd-btn-danger" onClick={onLogout}>
                Sign Out
              </button>
            )}
          </div>

          {activeTier === "system" && (
            <span
              className="fd-badge"
              data-testid="active-system-view-pill"
              style={{ fontWeight: 600 }}
            >
              System · {COCKPIT_TABS.find((t) => t.id === activeTab)?.label ?? activeTab}
            </span>
          )}
        </div>

        <button
          type="button"
          data-testid="split-view-toggle"
          className={`fd-btn${splitView ? " fd-btn-active" : ""}`}
          onClick={() => onToggleSplitView?.()}
          title="Toggle Desktop Split-View (Chat + Secondary Pane)"
          aria-label={splitView ? "Close right split panel" : "Open right split panel"}
          aria-pressed={splitView}
          style={{ marginLeft: "auto", flexShrink: 0 }}
        >
          <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
            <rect
              x="1.5"
              y="2.5"
              width="13"
              height="11"
              rx="2"
              stroke="currentColor"
              strokeWidth="1.4"
            />
            <line
              x1="9.5"
              y1="2.5"
              x2="9.5"
              y2="13.5"
              stroke="currentColor"
              strokeWidth="1.4"
            />
            <rect
              x="10.2"
              y="3.2"
              width="3.6"
              height="9.6"
              rx="0.8"
              fill="currentColor"
              fillOpacity={splitView ? 0.45 : 0.12}
            />
          </svg>
        </button>
      </div>
    </header>
  );
};
