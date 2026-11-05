import React from "react";
import { PHONE_TOOL_VIEWS, type PhoneView } from "./phoneNavigation";

export interface PhoneMoreActions {
  themeMode: "dark" | "light" | "system";
  onToggleTheme?: () => void;
  onOpenSettings?: () => void;
  onOpenHelp?: () => void;
  onOpenNodes?: () => void;
  onOpenSpinUp?: () => void;
  onOpenPairing?: () => void;
  onLogout?: () => void;
}

export interface PhoneMoreSheetProps {
  view: PhoneView;
  actions: PhoneMoreActions;
  /** Wire Mail needs a selected agent; it is hidden when there is none. */
  hasAgent?: boolean;
  onNavigate: (view: PhoneView) => void;
  /** `restoreFocus: false` is used when an action opens a modal that should keep focus. */
  onClose: (options?: { restoreFocus?: boolean }) => void;
}

export const PhoneMoreSheet: React.FC<PhoneMoreSheetProps> = ({
  view,
  actions,
  hasAgent = true,
  onNavigate,
  onClose,
}) => {
  const sheetRef = React.useRef<HTMLDivElement | null>(null);

  React.useEffect(() => {
    sheetRef.current?.focus();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
        return;
      }
      if (event.key !== "Tab") return;
      const sheet = sheetRef.current;
      if (!sheet) return;
      const focusable = Array.from(
        sheet.querySelectorAll<HTMLElement>("button:not([disabled]), [href], [tabindex]:not([tabindex='-1'])"),
      );
      if (focusable.length === 0) {
        event.preventDefault();
        sheet.focus();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      const active = document.activeElement;
      if (event.shiftKey) {
        if (active === first || active === sheet || !sheet.contains(active)) {
          event.preventDefault();
          last.focus();
        }
      } else if (active === last || !sheet.contains(active)) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  const actionItems: Array<{
    id: string;
    label: string;
    handler?: () => void;
    opensModal?: boolean;
  }> = [
    { id: "settings", label: "Settings", handler: actions.onOpenSettings, opensModal: true },
    { id: "help", label: "Help", handler: actions.onOpenHelp, opensModal: true },
    { id: "theme", label: `Theme: ${actions.themeMode}`, handler: actions.onToggleTheme },
    { id: "nodes", label: "Nodes", handler: actions.onOpenNodes, opensModal: true },
    { id: "spin-up", label: "Spin up agent", handler: actions.onOpenSpinUp, opensModal: true },
    { id: "pair", label: "Pair device", handler: actions.onOpenPairing, opensModal: true },
    { id: "logout", label: "Log out", handler: actions.onLogout },
  ];
  const toolViews = PHONE_TOOL_VIEWS.filter((tool) => hasAgent || tool.id !== "mail");

  return (
    <>
      <div
        className="fd-phone-sheet-backdrop"
        data-testid="phone-more-backdrop"
        data-phone-modal-layer=""
        aria-hidden="true"
        onClick={() => onClose()}
      />
      <div
        ref={sheetRef}
        className="fd-phone-more-sheet"
        data-testid="phone-more-sheet"
        data-phone-modal-layer=""
        role="dialog"
        aria-modal="true"
        aria-label="More"
        tabIndex={-1}
      >
        <div className="fd-phone-sheet-section" role="group" aria-label="Tool views">
          {toolViews.map((tool) => (
            <button
              key={tool.id}
              type="button"
              className={`fd-phone-sheet-item${view === tool.id ? " is-active" : ""}`}
              data-testid={`phone-more-item-${tool.id}`}
              aria-current={view === tool.id ? "page" : undefined}
              onClick={() => {
                onNavigate(tool.id);
                onClose();
              }}
            >
              {tool.label}
            </button>
          ))}
        </div>
        <div className="fd-phone-sheet-section" role="group" aria-label="Actions">
          {actionItems.map((item) => (
            <button
              key={item.id}
              type="button"
              className="fd-phone-sheet-item"
              data-testid={`phone-more-action-${item.id}`}
              onClick={() => {
                item.handler?.();
                onClose({ restoreFocus: !item.opensModal });
              }}
            >
              {item.label}
            </button>
          ))}
        </div>
      </div>
    </>
  );
};
