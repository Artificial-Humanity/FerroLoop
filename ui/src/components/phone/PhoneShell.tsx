import React from "react";
import type { AgentRecord, DiscoveredSessionSummary } from "../../types/api";
import { PhoneBottomNav } from "./PhoneBottomNav";
import { PhoneMoreSheet, type PhoneMoreActions } from "./PhoneMoreSheet";
import { PhoneTopBar } from "./PhoneTopBar";
import type { PhoneView } from "./phoneNavigation";

export interface PhoneShellProps {
  view: PhoneView;
  onNavigate: (view: PhoneView) => void;
  /** Leaves a tool view and returns to Chat. */
  onBack: () => void;
  selectedAgent: AgentRecord | null;
  sessions: DiscoveredSessionSummary[];
  selectedSessionId: string | null;
  onSelectSession: (sessionId: string) => void;
  wsConnected: boolean;
  agentsUnread: number;
  actions: PhoneMoreActions;
  /** The active view's pane, already chosen by the caller. */
  children: React.ReactNode;
}

export const PhoneShell: React.FC<PhoneShellProps> = ({
  view,
  onNavigate,
  onBack,
  selectedAgent,
  sessions,
  selectedSessionId,
  onSelectSession,
  wsConnected,
  agentsUnread,
  actions,
  children,
}) => {
  const [moreOpen, setMoreOpen] = React.useState(false);
  const moreButtonRef = React.useRef<HTMLButtonElement | null>(null);

  const closeMore = React.useCallback(() => {
    setMoreOpen(false);
    moreButtonRef.current?.focus();
  }, []);

  return (
    <div className="fd-phone-shell" data-testid="phone-shell" data-phone-view={view}>
      <PhoneTopBar
        view={view}
        selectedAgent={selectedAgent}
        sessions={sessions}
        selectedSessionId={selectedSessionId}
        onSelectSession={onSelectSession}
        wsConnected={wsConnected}
        onBack={onBack}
      />
      <main className="fd-phone-view" data-testid="phone-view">
        {children}
      </main>
      <PhoneBottomNav
        view={view}
        onNavigate={(next) => {
          setMoreOpen(false);
          onNavigate(next);
        }}
        onOpenMore={() => setMoreOpen(true)}
        moreOpen={moreOpen}
        agentsUnread={agentsUnread}
        moreButtonRef={moreButtonRef}
      />
      {moreOpen && (
        <PhoneMoreSheet
          view={view}
          actions={actions}
          onNavigate={onNavigate}
          onClose={closeMore}
        />
      )}
    </div>
  );
};
