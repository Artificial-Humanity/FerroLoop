import React from "react";
import type {
  AgentRecord,
  CockpitTab,
  DiscoveredSessionSummary,
  HubAuthority,
} from "../types/api";
import { formatRoleLabel, getAdapterBadgeClass } from "./RosterSidebar";

export interface ContextHeaderProps {
  selectedAgent: AgentRecord | null;
  sessions: DiscoveredSessionSummary[];
  selectedSessionId: string | null;
  onSelectSession: (sessionId: string) => void;
  activeTab: CockpitTab;
  onSelectTab: (tab: CockpitTab) => void;
  splitView: boolean;
  onToggleSplitView: () => void;
  themeMode?: "dark" | "light";
  onToggleTheme?: () => void;
  authority?: HubAuthority | null;
  wsConnected?: boolean;
  onOpenPairingModal: () => void;
  onLogout?: () => void;
  onOpenMobileRoster?: () => void;
}

export const COCKPIT_TABS: Array<{ id: CockpitTab; label: string }> = [
  { id: "chat", label: "Chat" },
  { id: "artifacts", label: "Artifacts" },
  { id: "git", label: "Git & PRs" },
  { id: "mail", label: "Wire Mail" },
  { id: "terminal", label: "Terminal" },
  { id: "hub", label: "Hub & Arch" },
  { id: "gate", label: "Vault & Gate" },
  { id: "state", label: "FerroState" },
];

export const ContextHeader: React.FC<ContextHeaderProps> = ({
  selectedAgent,
  sessions,
  selectedSessionId,
  onSelectSession,
  activeTab,
  onSelectTab,
  splitView,
  onToggleSplitView,
  themeMode = "dark",
  onToggleTheme,
  authority,
  wsConnected = false,
  onOpenPairingModal,
  onLogout,
  onOpenMobileRoster,
}) => {
  const agentSessions = React.useMemo(() => {
    if (!selectedAgent) return sessions;
    const key = selectedAgent.name_key.toLowerCase();
    const matching = sessions.filter((s) => (s.persona ?? "").toLowerCase() === key);
    return matching.length > 0 ? matching : sessions;
  }, [selectedAgent, sessions]);

  const activeSession =
    agentSessions.find((s) => s.id === selectedSessionId) ?? agentSessions[0] ?? null;

  const authorityText = React.useMemo(() => {
    if (!authority) return "Unauthenticated";
    if (authority.tier === "owner") {
      return `Owner: ${authority.login}`;
    }
    return authority.agent ? `Peer: ${authority.agent}` : "Peer";
  }, [authority]);

  return (
    <header className="fd-header" data-testid="context-header">
      <div style={{ display: "flex", alignItems: "center", gap: "10px", flexWrap: "wrap" }}>
        {onOpenMobileRoster && (
          <button
            type="button"
            className="fd-btn"
            onClick={onOpenMobileRoster}
            aria-label="Toggle Agent Roster"
            data-testid="mobile-roster-toggle"
          >
            ☰ Roster
          </button>
        )}

        {selectedAgent ? (
          <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
            <span style={{ fontWeight: 700, fontSize: "15px" }}>
              {selectedAgent.display_name}
            </span>
            <span className="fd-badge">{selectedAgent.project}</span>
            <span className="fd-badge">{formatRoleLabel(selectedAgent.role)}</span>
          </div>
        ) : (
          <span style={{ fontWeight: 600, color: "var(--text-muted)" }}>No Agent Selected</span>
        )}

        {agentSessions.length > 0 && (
          <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
            {agentSessions.length > 1 ? (
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
            ) : (
              activeSession && (
                <span
                  className="fd-badge fd-mono"
                  data-testid="session-id-pill"
                  title={activeSession.id}
                >
                  #{activeSession.id.slice(-8)}
                </span>
              )
            )}

            {activeSession?.adapter && (
              <span className={getAdapterBadgeClass(activeSession.adapter)}>
                {activeSession.adapter}
              </span>
            )}
          </div>
        )}

        {activeSession?.cwd && (
          <span
            className="fd-badge fd-mono"
            title={activeSession.cwd}
            data-testid="session-cwd-badge"
            style={{ color: "var(--text-muted)", maxWidth: "240px", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}
          >
            {activeSession.cwd}
          </span>
        )}
      </div>

      <div
        role="tablist"
        aria-label="Cockpit Modes"
        style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}
      >
        {COCKPIT_TABS.map((tab) => {
          const isActive = activeTab === tab.id;
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
        })}
      </div>

      <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
        <span
          className="fd-badge"
          title={wsConnected ? "WebSocket Connected" : "WebSocket Disconnected"}
        >
          <span className={`fd-dot ${wsConnected ? "fd-dot-alive" : "fd-dot-dead"}`} />
          WS
        </span>

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

        {onToggleTheme && (
          <button
            type="button"
            className="fd-btn"
            data-testid="theme-toggle-btn"
            onClick={onToggleTheme}
            title={themeMode === "dark" ? "Switch to light theme" : "Switch to dark theme"}
          >
            {themeMode === "dark" ? "☀ Light" : "☾ Dark"}
          </button>
        )}

        <button
          type="button"
          data-testid="split-view-toggle"
          className={`fd-btn${splitView ? " fd-btn-active" : ""}`}
          onClick={onToggleSplitView}
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
    </header>
  );
};
