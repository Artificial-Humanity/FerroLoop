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
        {onOpenMobileRoster && (
          <button
            type="button"
            className={`fd-btn fd-mobile-sidebar-toggle${mobileSidebarOpen ? " fd-btn-active" : ""}`}
            onClick={onOpenMobileRoster}
            aria-label="Toggle Agent Roster"
            aria-expanded={mobileSidebarOpen}
            data-testid="mobile-sidebar-toggle-btn"
          >
            ☰ Roster
          </button>
        )}

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

        {/* Tier 1: Agent Context */}
        <div
          className={`fd-tier-group fd-tier-agent${activeTier === "agent" ? " is-active-tier" : ""}`}
          data-testid="tier-agent-nav"
          data-active-tier={activeTier === "agent" ? "true" : undefined}
        >
          <span className="fd-tier-label">Agent</span>

          {selectedAgent ? (
            <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
              <span style={{ fontWeight: 700, fontSize: "14px" }}>
                {selectedAgent.display_name}
              </span>
              <span className="fd-badge">{formatRoleLabel(selectedAgent.role)}</span>
            </div>
          ) : (
            <span style={{ fontWeight: 600, color: "var(--text-muted)" }}>No Agent Selected</span>
          )}

          {agentSessions.length > 0 && (
            <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
              {agentSessions.length > 1 && (
                <select
                  aria-label="Active Session"
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
                <span className={getAdapterBadgeClass(activeSession.adapter)}>
                  {activeSession.adapter}
                </span>
              )}
            </div>
          )}

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

          <div style={{ display: "flex", alignItems: "center", gap: "4px" }}>
            {renderTabButton({ id: "mail", label: "Wire Mail" })}
          </div>
        </div>

        <span className="fd-tier-divider" aria-hidden="true" />

        {/* Tier 2: Project Context */}
        <div
          className={`fd-tier-group fd-tier-project${activeTier === "project" ? " is-active-tier" : ""}`}
          data-testid="tier-project-nav"
          data-active-tier={activeTier === "project" ? "true" : undefined}
        >
          <span className="fd-tier-label">Project</span>

          <span
            className="fd-badge"
            data-testid="active-project-badge"
            title={selectedAgent?.project ? `Project: ${selectedAgent.project}` : "Workspace"}
          >
            {selectedAgent?.project ?? "Workspace"}
          </span>

          <div style={{ display: "flex", alignItems: "center", gap: "4px" }}>
            {PROJECT_TIER_TABS.map(renderTabButton)}
            <button
              type="button"
              data-testid="split-view-toggle"
              className={`fd-btn${splitView ? " fd-btn-active" : ""}`}
              onClick={() => onToggleSplitView?.()}
              title="Toggle Desktop Split-View (Chat + Secondary Pane)"
              aria-label={splitView ? "Close right split panel" : "Open right split panel"}
              aria-pressed={splitView}
            >
              <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                <rect x="1.5" y="2.5" width="13" height="11" rx="2" stroke="currentColor" strokeWidth="1.4" />
                <line x1="9.5" y1="2.5" x2="9.5" y2="13.5" stroke="currentColor" strokeWidth="1.4" />
                <rect x="10.2" y="3.2" width="3.6" height="9.6" rx="0.8" fill="currentColor" fillOpacity={splitView ? 0.45 : 0.12} />
              </svg>
            </button>
          </div>
        </div>

        {/* Tier 3: System Context (consoles, Nodes, Spin Up, and Auth live inside SettingsModal; global controls live in RosterSidebar & ChatPane) */}
        <div
          className={`fd-tier-group fd-tier-system${activeTier === "system" ? " is-active-tier" : ""}`}
          data-testid="tier-system-nav"
          data-active-tier={activeTier === "system" ? "true" : undefined}
          style={
            activeTier === "system"
              ? { marginLeft: "auto", position: "relative" }
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
      </div>
    </header>
  );
};
