import React from "react";
import {
  PHONE_PRIMARY_TABS,
  isPhoneToolView,
  type PhoneView,
} from "./phoneNavigation";

export interface PhoneBottomNavProps {
  view: PhoneView;
  onNavigate: (view: PhoneView) => void;
  onOpenMore: () => void;
  moreOpen: boolean;
  agentsUnread: number;
  moreButtonRef: React.Ref<HTMLButtonElement>;
}

export const PhoneBottomNav: React.FC<PhoneBottomNavProps> = ({
  view,
  onNavigate,
  onOpenMore,
  moreOpen,
  agentsUnread,
  moreButtonRef,
}) => {
  const moreActive = isPhoneToolView(view);
  return (
    <nav className="fd-phone-bottom-nav" data-testid="phone-bottom-nav" aria-label="Primary">
      {PHONE_PRIMARY_TABS.map((tab) => {
        const active = view === tab.id;
        return (
          <button
            key={tab.id}
            type="button"
            className={`fd-phone-tab${active ? " is-active" : ""}`}
            data-testid={`phone-tab-${tab.id}`}
            aria-current={active ? "page" : undefined}
            onClick={() => onNavigate(tab.id)}
          >
            <span>{tab.label}</span>
            {tab.id === "agents" && agentsUnread > 0 && (
              <span
                className="fd-phone-tab-badge"
                data-testid="phone-agents-unread-badge"
                aria-label={`${agentsUnread} unread`}
              >
                {agentsUnread > 99 ? "99+" : agentsUnread}
              </span>
            )}
          </button>
        );
      })}
      <button
        ref={moreButtonRef}
        type="button"
        className={`fd-phone-tab${moreActive ? " is-active" : ""}`}
        data-testid="phone-tab-more"
        aria-current={moreActive ? "page" : undefined}
        aria-haspopup="dialog"
        aria-expanded={moreOpen}
        onClick={onOpenMore}
      >
        <span>More</span>
      </button>
    </nav>
  );
};
