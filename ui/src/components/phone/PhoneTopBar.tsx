import React from "react";
import type { AgentRecord, DiscoveredSessionSummary } from "../../types/api";
import { formatRoleLabel } from "../RosterSidebar";
import { isPhoneToolView, phoneViewLabel, type PhoneView } from "./phoneNavigation";

export interface PhoneTopBarProps {
  view: PhoneView;
  selectedAgent: AgentRecord | null;
  sessions: DiscoveredSessionSummary[];
  selectedSessionId: string | null;
  onSelectSession: (sessionId: string) => void;
  wsConnected: boolean;
  onBack: () => void;
}

export const PhoneTopBar: React.FC<PhoneTopBarProps> = ({
  view,
  selectedAgent,
  sessions,
  selectedSessionId,
  onSelectSession,
  wsConnected,
  onBack,
}) => {
  const agentSessions = React.useMemo(() => {
    if (!selectedAgent) return sessions;
    const key = selectedAgent.name_key.toLowerCase();
    const matching = sessions.filter((s) => (s.persona ?? "").toLowerCase() === key);
    return matching.length > 0 ? matching : sessions;
  }, [selectedAgent, sessions]);
  const activeSession =
    agentSessions.find((s) => s.id === selectedSessionId) ?? agentSessions[0] ?? null;

  return (
    <header className="fd-phone-top-bar" data-testid="phone-top-bar">
      {isPhoneToolView(view) && (
        <button
          type="button"
          className="fd-btn fd-phone-back-btn"
          data-testid="phone-back-btn"
          aria-label="Back to Chat"
          onClick={onBack}
        >
          <span aria-hidden="true">&larr;</span>
        </button>
      )}
      <div className="fd-phone-top-bar-title">
        <strong data-testid="phone-view-title">{phoneViewLabel(view)}</strong>
        <span className="fd-phone-top-bar-agent" data-testid="phone-agent-identity">
          <span className="fd-phone-top-bar-agent-name" data-testid="phone-agent-name">
            {selectedAgent ? selectedAgent.display_name : "No Agent Selected"}
          </span>
          {selectedAgent && (
            <span className="fd-badge" data-testid="phone-agent-role-badge">
              {formatRoleLabel(selectedAgent.role)}
            </span>
          )}
        </span>
      </div>
      {agentSessions.length > 1 && (
        <select
          aria-label="Active Session"
          title="Switch Active Session"
          data-testid="phone-session-selector"
          className="fd-select fd-mono fd-phone-session-select"
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
      <span
        className={`fd-phone-ws-dot${wsConnected ? " is-connected" : ""}`}
        data-testid="phone-ws-status"
        data-connected={wsConnected ? "true" : "false"}
        role="img"
        aria-label={wsConnected ? "Connected to hub" : "Disconnected from hub"}
        title={wsConnected ? "Connected" : "Disconnected"}
      />
    </header>
  );
};
