import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App, { THEME_STORAGE_KEY } from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import { PhoneBottomNav } from "../components/phone/PhoneBottomNav";
import {
  PHONE_PRIMARY_TABS,
  PHONE_TOOL_VIEWS,
  mapToPhoneView,
} from "../components/phone/phoneNavigation";
import { OPEN_ARTIFACT_EVENT } from "../components/RichMarkdown";

function makeStubApiClient(): HubApiClient {
  const fetchImpl = vi.fn(async () =>
    new Response(JSON.stringify({}), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }),
  );
  return new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
}

function makeStubWsClient(): HubWsClient {
  return {
    setToken: vi.fn(),
    connect: vi.fn(),
    disconnect: vi.fn(),
    onStatusChange: () => () => {},
    subscribe: () => () => {},
    send: vi.fn(),
    sendFrame: vi.fn(),
  } as unknown as HubWsClient;
}

function setViewport(width: number, height: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, writable: true, value: width });
  Object.defineProperty(window, "innerHeight", { configurable: true, writable: true, value: height });
}

function resizeTo(width: number, height: number) {
  act(() => {
    setViewport(width, height);
    fireEvent(window, new Event("resize"));
  });
}

function renderAt(width: number, height: number) {
  setViewport(width, height);
  return render(<App apiClient={makeStubApiClient()} wsClient={makeStubWsClient()} />);
}

const PANE_TEST_IDS: Record<string, string> = {
  git: "git-pr-pane",
  terminal: "terminal-pane",
  artifacts: "artifact-preview-pane",
  events: "triage-queue-pane",
  hub: "hub-drawer",
  gate: "vault-gate-pane",
  state: "ferrostate-pane",
};

function openMoreItem(view: string) {
  fireEvent.click(screen.getByTestId("phone-tab-more"));
  fireEvent.click(screen.getByTestId(`phone-more-item-${view}`));
}

describe("phoneNavigation", () => {
  it("lists the primary tabs and every tool view", () => {
    expect(PHONE_PRIMARY_TABS.map((t) => t.id)).toEqual(["agents", "chat", "board", "watch"]);
    expect(PHONE_TOOL_VIEWS.map((t) => t.id)).toEqual([
      "git",
      "terminal",
      "artifacts",
      "events",
      "mail",
      "hub",
      "gate",
      "state",
    ]);
    expect(PHONE_TOOL_VIEWS.find((t) => t.id === "git")?.label).toBe("Diffs & Git");
  });

  it("maps cockpit and left-panel tabs to phone views", () => {
    expect(mapToPhoneView("chat")).toBe("chat");
    expect(mapToPhoneView("roster")).toBe("agents");
    expect(mapToPhoneView("board")).toBe("board");
    expect(mapToPhoneView("watch")).toBe("watch");
    expect(mapToPhoneView("triage")).toBe("events");
    for (const tab of ["git", "terminal", "artifacts", "mail", "hub", "gate", "state"] as const) {
      expect(mapToPhoneView(tab)).toBe(tab);
    }
  });
});

describe("PhoneBottomNav unread badge", () => {
  const props = {
    view: "chat" as const,
    onNavigate: vi.fn(),
    onOpenMore: vi.fn(),
    moreOpen: false,
    moreButtonRef: { current: null },
  };

  it("shows the unread total on the Agents tab and hides it at zero", () => {
    const { rerender } = render(<PhoneBottomNav {...props} agentsUnread={3} />);
    expect(within(screen.getByTestId("phone-tab-agents")).getByTestId("phone-agents-unread-badge")).toHaveTextContent("3");
    rerender(<PhoneBottomNav {...props} agentsUnread={0} />);
    expect(screen.queryByTestId("phone-agents-unread-badge")).toBeNull();
  });
});

describe("Phone shell (< 768px)", () => {
  const originalWidth = window.innerWidth;
  const originalHeight = window.innerHeight;

  beforeEach(() => {
    window.localStorage.clear();
  });

  afterEach(() => {
    setViewport(originalWidth, originalHeight);
  });

  it("renders the phone shell instead of the desktop chrome at 390x844", () => {
    renderAt(390, 844);
    expect(screen.getByTestId("ferrodeck-cockpit")).toHaveAttribute("data-device-layout", "phone");
    expect(screen.getByTestId("ferrodeck-cockpit")).toHaveAttribute("data-viewport-panel-mode", "single");
    expect(screen.getByTestId("phone-shell")).toBeInTheDocument();
    expect(screen.getByTestId("phone-top-bar")).toBeInTheDocument();
    expect(screen.getByTestId("phone-bottom-nav")).toBeInTheDocument();
    expect(screen.queryByTestId("context-header")).toBeNull();
    expect(screen.queryByTestId("cockpit-tab-bar")).toBeNull();
    expect(screen.queryByTestId("right-split-panel")).toBeNull();
    expect(screen.queryByTestId("mobile-sidebar-backdrop")).toBeNull();
    expect(screen.queryByTestId("mobile-split-backdrop")).toBeNull();
    expect(screen.queryByTestId("phone-back-btn")).toBeNull();
    // Chat is the default view.
    expect(screen.getByTestId("chat-pane")).toBeInTheDocument();
    expect(screen.getByTestId("phone-tab-chat")).toHaveAttribute("aria-current", "page");
    expect(screen.getByTestId("phone-ws-status")).toBeInTheDocument();
    expect(screen.getByTestId("phone-agent-name")).toHaveTextContent("Ferris");
  });

  it("maps the width boundaries onto panel mode and device layout", () => {
    const { unmount } = renderAt(740, 900);
    let cockpit = screen.getByTestId("ferrodeck-cockpit");
    expect(cockpit).toHaveAttribute("data-viewport-panel-mode", "single");
    expect(cockpit).toHaveAttribute("data-device-layout", "phone");
    unmount();

    const second = renderAt(768, 1024);
    cockpit = screen.getByTestId("ferrodeck-cockpit");
    expect(cockpit).toHaveAttribute("data-viewport-panel-mode", "two-panel");
    expect(cockpit).toHaveAttribute("data-device-layout", "tablet");
    expect(screen.queryByTestId("phone-shell")).toBeNull();
    second.unmount();

    renderAt(1280, 800);
    cockpit = screen.getByTestId("ferrodeck-cockpit");
    expect(cockpit).toHaveAttribute("data-device-layout", "desktop");
    expect(screen.queryByTestId("phone-shell")).toBeNull();
  });

  it("switches between the four primary bottom tabs", () => {
    renderAt(390, 844);

    fireEvent.click(screen.getByTestId("phone-tab-agents"));
    const roster = screen.getByTestId("roster-sidebar");
    expect(roster).toHaveAttribute("data-embedded", "true");
    expect(roster).toHaveClass("fd-sidebar-embedded");
    expect(screen.queryByTestId("left-panel-header")).toBeNull();
    expect(screen.queryByTestId("left-panel-resize-handle")).toBeNull();
    expect(screen.queryByTestId("mobile-sidebar-backdrop")).toBeNull();
    expect(screen.queryByLabelText("Close Roster")).toBeNull();
    expect(screen.queryByTestId("chat-pane")).toBeNull();
    expect(screen.getByTestId("phone-tab-agents")).toHaveAttribute("aria-current", "page");
    expect(screen.getByTestId("phone-tab-chat")).not.toHaveAttribute("aria-current");

    fireEvent.click(screen.getByTestId("phone-tab-board"));
    expect(screen.getByTestId("records-board-pane")).toBeInTheDocument();
    expect(screen.queryByTestId("roster-sidebar")).toBeNull();
    expect(screen.getByTestId("phone-tab-board")).toHaveAttribute("aria-current", "page");

    fireEvent.click(screen.getByTestId("phone-tab-watch"));
    expect(screen.getByTestId("ferrowatch-pane")).toBeInTheDocument();
    expect(screen.queryByTestId("records-board-pane")).toBeNull();

    fireEvent.click(screen.getByTestId("phone-tab-chat"));
    expect(screen.getByTestId("chat-pane")).toBeInTheDocument();
    expect(screen.queryByTestId("phone-back-btn")).toBeNull();
  });

  it("lists every tool view in More, opens each pane, highlights More and goes back to Chat", () => {
    renderAt(390, 844);

    fireEvent.click(screen.getByTestId("phone-tab-more"));
    const sheet = screen.getByTestId("phone-more-sheet");
    expect(sheet).toHaveAttribute("role", "dialog");
    for (const tool of PHONE_TOOL_VIEWS) {
      expect(within(sheet).getByTestId(`phone-more-item-${tool.id}`)).toHaveTextContent(tool.label);
    }
    fireEvent.click(screen.getByTestId("phone-more-backdrop"));
    expect(screen.queryByTestId("phone-more-sheet")).toBeNull();

    for (const tool of PHONE_TOOL_VIEWS) {
      openMoreItem(tool.id);
      expect(screen.queryByTestId("phone-more-sheet")).toBeNull();
      expect(screen.getByTestId("phone-shell")).toHaveAttribute("data-phone-view", tool.id);
      const paneId = PANE_TEST_IDS[tool.id];
      if (paneId) {
        expect(screen.getByTestId(paneId)).toBeInTheDocument();
      }
      expect(screen.getByTestId("phone-tab-more")).toHaveAttribute("aria-current", "page");
      expect(screen.getByTestId("phone-view-title")).toHaveTextContent(tool.label);

      fireEvent.click(screen.getByTestId("phone-back-btn"));
      expect(screen.getByTestId("chat-pane")).toBeInTheDocument();
      expect(screen.queryByTestId("phone-back-btn")).toBeNull();
      expect(screen.getByTestId("phone-tab-more")).not.toHaveAttribute("aria-current");
    }
  });

  it("closes the More sheet with Escape and returns focus to the More tab", () => {
    renderAt(390, 844);
    const more = screen.getByTestId("phone-tab-more");
    fireEvent.click(more);
    expect(screen.getByTestId("phone-more-sheet")).toBeInTheDocument();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByTestId("phone-more-sheet")).toBeNull();
    expect(more).toHaveFocus();

    fireEvent.click(more);
    fireEvent.click(screen.getByTestId("phone-more-backdrop"));
    expect(screen.queryByTestId("phone-more-sheet")).toBeNull();
    expect(more).toHaveFocus();
  });

  it("lands on Chat after selecting an agent from the Agents view", () => {
    renderAt(390, 844);
    fireEvent.click(screen.getByTestId("phone-tab-agents"));
    const roster = screen.getByTestId("roster-sidebar");
    const agentRow = roster.querySelector("[data-testid^='agent-row-']") as HTMLElement | null;
    expect(agentRow).not.toBeNull();
    fireEvent.click(agentRow!);
    expect(screen.getByTestId("chat-pane")).toBeInTheDocument();
    expect(screen.queryByTestId("roster-sidebar")).toBeNull();
    expect(screen.getByTestId("phone-tab-chat")).toHaveAttribute("aria-current", "page");
  });

  it("routes an artifact open event to the artifacts view without touching desktop state", () => {
    renderAt(390, 844);
    act(() => {
      window.dispatchEvent(
        new CustomEvent(OPEN_ARTIFACT_EVENT, { detail: { path: "docs/notes.md" } }),
      );
    });
    expect(screen.getByTestId("phone-shell")).toHaveAttribute("data-phone-view", "artifacts");
    expect(screen.getByTestId("artifact-preview-pane")).toBeInTheDocument();
    expect(screen.getByTestId("phone-back-btn")).toBeInTheDocument();
    expect(screen.queryByTestId("right-split-panel")).toBeNull();

    // Phone navigation leaves the desktop arrangement alone: the right dock is still closed.
    resizeTo(1280, 800);
    expect(screen.getByTestId("split-view-toggle")).toHaveAttribute("aria-pressed", "false");
  });

  it("opens settings, help, nodes, spin up and pairing from More and closes the sheet", () => {
    renderAt(390, 844);
    const open = (id: string) => {
      fireEvent.click(screen.getByTestId("phone-tab-more"));
      fireEvent.click(screen.getByTestId(`phone-more-action-${id}`));
      expect(screen.queryByTestId("phone-more-sheet")).toBeNull();
    };
    open("settings");
    expect(screen.getByTestId("settings-tab-appearance")).toBeInTheDocument();
    fireEvent.keyDown(document, { key: "Escape" });
    open("help");
    expect(screen.getByTestId("contextual-help-drawer")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("close-help-drawer-btn"));
    open("nodes");
    expect(screen.getByTestId("nodes-modal")).toBeInTheDocument();
    open("spin-up");
    expect(screen.getByTestId("spin-up-agent-modal")).toBeInTheDocument();
    open("pair");
    expect(screen.getByTestId("auth-tab-token")).toBeInTheDocument();
  });

  it("toggles the theme and logs out from More", async () => {
    const wsClient = makeStubWsClient();
    setViewport(390, 844);
    window.localStorage.setItem(THEME_STORAGE_KEY, "dark");
    render(<App apiClient={makeStubApiClient()} wsClient={wsClient} />);

    fireEvent.click(screen.getByTestId("phone-tab-more"));
    fireEvent.click(screen.getByTestId("phone-more-action-theme"));
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("light");

    fireEvent.click(screen.getByTestId("phone-tab-more"));
    await act(async () => {
      fireEvent.click(screen.getByTestId("phone-more-action-logout"));
    });
    expect(wsClient.disconnect).toHaveBeenCalled();
  });

  it("keeps the desktop split view and left panel through 390 -> 1280 -> 390", () => {
    renderAt(1280, 800);
    fireEvent.click(screen.getByTestId("split-view-toggle"));
    expect(screen.getByTestId("split-view-toggle")).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(screen.getByTestId("left-panel-tab-terminal"));
    expect(screen.getByTestId("roster-sidebar")).toHaveAttribute("data-left-panel-tab", "terminal");

    resizeTo(390, 844);
    expect(screen.getByTestId("phone-shell")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("phone-tab-board"));
    openMoreItem("git");
    fireEvent.click(screen.getByTestId("phone-tab-agents"));
    fireEvent.click(screen.getByTestId("phone-tab-watch"));

    resizeTo(1280, 800);
    expect(screen.queryByTestId("phone-shell")).toBeNull();
    expect(screen.getByTestId("split-view-toggle")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("right-split-panel")).toBeInTheDocument();
    const roster = screen.getByTestId("roster-sidebar");
    expect(roster).toHaveAttribute("data-left-panel-tab", "terminal");
    expect(roster).toHaveAttribute("data-desktop-collapsed", "false");
    expect(roster).toHaveAttribute("data-mobile-open", "false");
    expect(roster).not.toHaveAttribute("data-embedded");

    resizeTo(390, 844);
    resizeTo(1280, 800);
    expect(screen.getByTestId("split-view-toggle")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("roster-sidebar")).toHaveAttribute("data-left-panel-tab", "terminal");
  });

  it("starts at defaults when a phone session first grows to desktop", () => {
    renderAt(390, 844);
    fireEvent.click(screen.getByTestId("phone-tab-board"));
    openMoreItem("terminal");
    resizeTo(1280, 800);
    expect(screen.getByTestId("split-view-toggle")).toHaveAttribute("aria-pressed", "false");
    expect(screen.queryByTestId("right-split-panel")).toBeNull();
    const roster = screen.getByTestId("roster-sidebar");
    expect(roster).toHaveAttribute("data-left-panel-tab", "roster");
    expect(roster).toHaveAttribute("data-desktop-collapsed", "false");
  });
});
