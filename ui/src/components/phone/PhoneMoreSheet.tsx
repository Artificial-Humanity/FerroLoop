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
  onNavigate: (view: PhoneView) => void;
  onClose: () => void;
}

export const PhoneMoreSheet: React.FC<PhoneMoreSheetProps> = ({
  view,
  actions,
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
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  const run = (fn?: () => void) => () => {
    fn?.();
    onClose();
  };

  const actionItems: Array<{ id: string; label: string; handler?: () => void }> = [
    { id: "settings", label: "Settings", handler: actions.onOpenSettings },
    { id: "help", label: "Help", handler: actions.onOpenHelp },
    { id: "theme", label: `Theme: ${actions.themeMode}`, handler: actions.onToggleTheme },
    { id: "nodes", label: "Nodes", handler: actions.onOpenNodes },
    { id: "spin-up", label: "Spin up agent", handler: actions.onOpenSpinUp },
    { id: "pair", label: "Pair device", handler: actions.onOpenPairing },
    { id: "logout", label: "Log out", handler: actions.onLogout },
  ];

  return (
    <>
      <div
        className="fd-phone-sheet-backdrop"
        data-testid="phone-more-backdrop"
        aria-hidden="true"
        onClick={onClose}
      />
      <div
        ref={sheetRef}
        className="fd-phone-more-sheet"
        data-testid="phone-more-sheet"
        role="dialog"
        aria-modal="true"
        aria-label="More"
        tabIndex={-1}
      >
        <div className="fd-phone-sheet-section" role="group" aria-label="Tool views">
          {PHONE_TOOL_VIEWS.map((tool) => (
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
              onClick={run(item.handler)}
            >
              {item.label}
            </button>
          ))}
        </div>
      </div>
    </>
  );
};
