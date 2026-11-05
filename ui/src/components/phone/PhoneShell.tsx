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
  /** Whether an agent is selected; Wire Mail is hidden from More without one. */
  hasAgent?: boolean;
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
  hasAgent = true,
  actions,
  children,
}) => {
  const [moreOpen, setMoreOpen] = React.useState(false);
  const moreButtonRef = React.useRef<HTMLButtonElement | null>(null);

  const shellRef = React.useRef<HTMLDivElement | null>(null);
  const restoreFocusRef = React.useRef(false);

  const closeMore = React.useCallback((options?: { restoreFocus?: boolean }) => {
    restoreFocusRef.current = options?.restoreFocus !== false;
    setMoreOpen(false);
  }, []);

  // Return focus to the More tab once the background is interactive again (it is inert while the
  // sheet is open, so focusing it any earlier would be a no-op).
  React.useEffect(() => {
    if (!moreOpen && restoreFocusRef.current) {
      restoreFocusRef.current = false;
      moreButtonRef.current?.focus();
    }
  }, [moreOpen]);

  // While the modal sheet is open, keep keyboard and assistive focus out of the shell behind it.
  React.useEffect(() => {
    if (!moreOpen) return;
    const shell = shellRef.current;
    if (!shell) return;
    const background = Array.from(shell.children).filter(
      (el) => !el.hasAttribute("data-phone-modal-layer"),
    );
    background.forEach((el) => el.setAttribute("inert", ""));
    return () => background.forEach((el) => el.removeAttribute("inert"));
  }, [moreOpen]);

  return (
    <div
      ref={shellRef}
      className="fd-phone-shell"
      data-testid="phone-shell"
      data-phone-view={view}
    >
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
          hasAgent={hasAgent}
          actions={actions}
          onNavigate={onNavigate}
          onClose={closeMore}
        />
      )}
    </div>
  );
};
