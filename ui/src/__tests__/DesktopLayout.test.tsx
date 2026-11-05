import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";

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

const cockpit = () => screen.getByTestId("ferrodeck-cockpit");
const roster = () => screen.getByTestId("roster-sidebar");
const toggle = () => screen.getByTestId("mobile-sidebar-toggle-btn");
const dockToggle = () => screen.getByTestId("split-view-toggle");

const PERSISTED_WIDTH_KEYS = [
  "fd_left_roster_width",
  "fd_left_console_width",
  "fd_right_panel_width",
] as const;

describe("Desktop layout (>= 1024)", () => {
  const originalWidth = window.innerWidth;
  const originalHeight = window.innerHeight;

  beforeEach(() => {
    window.localStorage.clear();
  });

  afterEach(() => {
    setViewport(originalWidth, originalHeight);
  });

  describe.each([
    [1280, 800],
    [1440, 900],
  ])("three-panel at %ix%i", (width, height) => {
    it("renders the desktop cockpit with all three panes and no phone or tablet chrome", () => {
      renderAt(width, height);
      expect(cockpit()).toHaveAttribute("data-device-layout", "desktop");
      expect(cockpit()).toHaveAttribute("data-viewport-panel-mode", "three-panel");
      expect(cockpit()).not.toHaveAttribute("data-tablet-master");
      expect(screen.getByTestId("context-header")).toBeInTheDocument();
      expect(roster()).toBeInTheDocument();
      expect(screen.getByTestId("cockpit-workspace")).toBeInTheDocument();
      expect(screen.queryByTestId("phone-shell")).toBeNull();
    });

    it("shows the left resize handle", () => {
      renderAt(width, height);
      expect(screen.getByTestId("left-panel-resize-handle")).toBeInTheDocument();
    });

    it("keeps the roster open when the right dock opens", () => {
      renderAt(width, height);
      expect(roster()).toHaveAttribute("data-desktop-collapsed", "false");
      fireEvent.click(dockToggle());
      expect(screen.getByTestId("right-split-panel")).toBeInTheDocument();
      expect(roster()).toHaveAttribute("data-desktop-collapsed", "false");
    });
  });

  describe.each([
    [1024, 768],
    [1050, 800],
  ])("two-panel at %ix%i", (width, height) => {
    it("renders the desktop cockpit in two-panel mode", () => {
      renderAt(width, height);
      expect(cockpit()).toHaveAttribute("data-device-layout", "desktop");
      expect(cockpit()).toHaveAttribute("data-viewport-panel-mode", "two-panel");
      expect(cockpit()).not.toHaveAttribute("data-tablet-master");
      expect(screen.queryByTestId("phone-shell")).toBeNull();
    });

    it("collapses the roster when the right dock opens", () => {
      renderAt(width, height);
      expect(roster()).toHaveAttribute("data-desktop-collapsed", "false");
      fireEvent.click(dockToggle());
      expect(screen.getByTestId("right-split-panel")).toBeInTheDocument();
      expect(roster()).toHaveAttribute("data-desktop-collapsed", "true");
    });

    it("closes the right dock when the header toggle reopens the roster", () => {
      renderAt(width, height);
      fireEvent.click(dockToggle());
      expect(screen.getByTestId("right-split-panel")).toBeInTheDocument();
      fireEvent.click(toggle());
      expect(screen.queryByTestId("right-split-panel")).toBeNull();
      expect(roster()).toHaveAttribute("data-desktop-collapsed", "false");
    });

    it("collapses and restores the roster with the header toggle", () => {
      renderAt(width, height);
      fireEvent.click(toggle());
      expect(roster()).toHaveAttribute("data-desktop-collapsed", "true");
      fireEvent.click(toggle());
      expect(roster()).toHaveAttribute("data-desktop-collapsed", "false");
    });

    it("closes the right dock when the dock toggle is pressed again", () => {
      renderAt(width, height);
      fireEvent.click(dockToggle());
      fireEvent.click(dockToggle());
      expect(screen.queryByTestId("right-split-panel")).toBeNull();
    });
  });

  it("collapses the roster when a width resize crosses below 1080 with the right dock open", () => {
    renderAt(1280, 800);
    fireEvent.click(dockToggle());
    expect(roster()).toHaveAttribute("data-desktop-collapsed", "false");
    resizeTo(1050, 800);
    expect(roster()).toHaveAttribute("data-desktop-collapsed", "true");
  });

  it.each([
    [1280, 800],
    [900, 700],
    [800, 1100],
    [390, 844],
  ])("leaves the persisted panel widths untouched when merely rendering at %ix%i", (w, h) => {
    renderAt(w, h);
    for (const key of PERSISTED_WIDTH_KEYS) {
      expect(window.localStorage.getItem(key)).toBeNull();
    }
  });

  it("keeps pre-existing persisted panel widths unchanged across tablet and phone sizes", () => {
    const seeded: Record<string, string> = {
      fd_left_roster_width: "300",
      fd_left_console_width: "420",
      fd_right_panel_width: "500",
    };
    for (const [key, value] of Object.entries(seeded)) {
      window.localStorage.setItem(key, value);
    }
    renderAt(900, 700);
    resizeTo(800, 1100);
    resizeTo(390, 844);
    resizeTo(1280, 800);
    for (const [key, value] of Object.entries(seeded)) {
      expect(window.localStorage.getItem(key)).toBe(value);
    }
  });
});
