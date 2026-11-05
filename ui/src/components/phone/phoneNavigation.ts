import type { CockpitTab } from "../../types/api";
import type { LeftPanelTab } from "../RosterSidebar";

/** Primary phone views (bottom bar) plus the tool views reached through More. */
export type PhoneView =
  | "agents"
  | "chat"
  | "board"
  | "watch"
  | "git"
  | "terminal"
  | "artifacts"
  | "events"
  | "mail"
  | "hub"
  | "gate"
  | "state";

export type PhonePrimaryView = "agents" | "chat" | "board" | "watch";
export type PhoneToolView = Exclude<PhoneView, PhonePrimaryView>;

export const PHONE_PRIMARY_TABS: Array<{ id: PhonePrimaryView; label: string }> = [
  { id: "agents", label: "Agents" },
  { id: "chat", label: "Chat" },
  { id: "board", label: "Board" },
  { id: "watch", label: "Watch" },
];

export const PHONE_TOOL_VIEWS: Array<{ id: PhoneToolView; label: string }> = [
  { id: "git", label: "Diffs & Git" },
  { id: "terminal", label: "Terminal" },
  { id: "artifacts", label: "Artifacts" },
  { id: "events", label: "Events" },
  { id: "mail", label: "Wire Mail" },
  { id: "hub", label: "Hub & Arch" },
  { id: "gate", label: "Vault & Gate" },
  { id: "state", label: "FerroState" },
];

const TOOL_VIEW_IDS: ReadonlySet<PhoneView> = new Set(PHONE_TOOL_VIEWS.map((t) => t.id));

export function isPhoneToolView(view: PhoneView): view is PhoneToolView {
  return TOOL_VIEW_IDS.has(view);
}

export function phoneViewLabel(view: PhoneView): string {
  return (
    PHONE_PRIMARY_TABS.find((t) => t.id === view)?.label ??
    PHONE_TOOL_VIEWS.find((t) => t.id === view)?.label ??
    "Chat"
  );
}

/** Maps a desktop cockpit tab or left-panel tab onto its phone view. */
export function mapToPhoneView(tab: CockpitTab | LeftPanelTab): PhoneView {
  switch (tab) {
    case "chat":
      return "chat";
    case "roster":
      return "agents";
    case "board":
      return "board";
    case "watch":
      return "watch";
    case "triage":
      return "events";
    case "git":
    case "terminal":
    case "artifacts":
    case "mail":
    case "hub":
    case "gate":
    case "state":
      return tab;
    default:
      return "chat";
  }
}
