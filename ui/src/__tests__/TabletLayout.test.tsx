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

function agentRows(): HTMLElement[] {
  return Array.from(roster().querySelectorAll<HTMLElement>("[data-testid^='agent-row-']"));
}

function firstAgentRow(): HTMLElement {
  const row = agentRows()[0];
  expect(row).toBeDefined();
  return row;
}

const selectedAgentLabel = () => screen.getByTestId("tier-agent-name").textContent;

describe("Tablet master-detail (768 - 1023)", () => {
  const originalWidth = window.innerWidth;
  const originalHeight = window.innerHeight;

  beforeEach(() => {
    window.localStorage.clear();
  });

  afterEach(() => {
    setViewport(originalWidth, originalHeight);
  });

  it("treats 1024x768 as desktop with no tablet master mode", () => {
    renderAt(1024, 768);
    expect(cockpit()).toHaveAttribute("data-device-layout", "desktop");
    expect(cockpit()).not.toHaveAttribute("data-tablet-master");
  });

  it("omits the tablet master mode on phone and desktop", () => {
    const { unmount } = renderAt(390, 844);
    expect(cockpit()).toHaveAttribute("data-device-layout", "phone");
    expect(cockpit()).not.toHaveAttribute("data-tablet-master");
    unmount();
    renderAt(1280, 800);
    expect(cockpit()).toHaveAttribute("data-device-layout", "desktop");
    expect(cockpit()).not.toHaveAttribute("data-tablet-master");
  });

  it("docks the roster in landscape at 900x700", () => {
    renderAt(900, 700);
    expect(cockpit()).toHaveAttribute("data-device-layout", "tablet");
    expect(cockpit()).toHaveAttribute("data-tablet-master", "docked");
    expect(roster()).toHaveAttribute("data-desktop-collapsed", "false");
    expect(roster()).toBeVisible();
    expect(toggle()).toHaveAttribute("aria-expanded", "true");
  });

  it("keeps the docked master between 320 and 360 px wide", () => {
    renderAt(800, 600);
    const width = parseInt(roster().style.width, 10);
    expect(width).toBeGreaterThanOrEqual(320);
    expect(width).toBeLessThanOrEqual(360);
  });

  it("collapses the docked master with the header toggle", () => {
    renderAt(900, 700);
    fireEvent.click(toggle());
    expect(roster()).toHaveAttribute("data-desktop-collapsed", "true");
    expect(toggle()).toHaveAttribute("aria-expanded", "false");
  });

  it("collapses the docked master when the right dock opens", () => {
    renderAt(900, 700);
    fireEvent.click(screen.getByTestId("split-view-toggle"));
    expect(screen.getByTestId("right-split-panel")).toBeInTheDocument();
    expect(roster()).toHaveAttribute("data-desktop-collapsed", "true");
  });

  it("uses a closed overlay drawer in portrait at 800x1100", () => {
    renderAt(800, 1100);
    expect(cockpit()).toHaveAttribute("data-device-layout", "tablet");
    expect(cockpit()).toHaveAttribute("data-tablet-master", "overlay");
    expect(roster()).toHaveAttribute("data-mobile-open", "false");
    expect(roster()).toHaveAttribute("data-desktop-collapsed", "false");
    expect(toggle()).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByTestId("mobile-sidebar-backdrop")).toBeNull();
  });

  it("opens the drawer from the header toggle and closes it from the backdrop", () => {
    renderAt(800, 1100);
    fireEvent.click(toggle());
    expect(roster()).toHaveAttribute("data-mobile-open", "true");
    expect(toggle()).toHaveAttribute("aria-expanded", "true");
    expect(toggle()).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(screen.getByTestId("mobile-sidebar-backdrop"));
    expect(roster()).toHaveAttribute("data-mobile-open", "false");
    expect(toggle()).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByTestId("mobile-sidebar-backdrop")).toBeNull();
  });

  it("toggles the drawer closed with the header toggle and can reopen it", () => {
    renderAt(800, 1100);
    fireEvent.click(toggle());
    fireEvent.click(toggle());
    expect(roster()).toHaveAttribute("data-mobile-open", "false");
    fireEvent.click(toggle());
    expect(roster()).toHaveAttribute("data-mobile-open", "true");
  });

  it("closes the drawer after an agent is selected", () => {
    renderAt(800, 1100);
    fireEvent.click(toggle());
    fireEvent.click(firstAgentRow());
    expect(roster()).toHaveAttribute("data-mobile-open", "false");
    expect(screen.getByTestId("chat-pane")).toBeInTheDocument();
  });

  it("closes the drawer when a right-dock tool tab opens", () => {
    renderAt(800, 1100);
    fireEvent.click(toggle());
    fireEvent.click(screen.getByTestId("split-view-toggle"));
    expect(screen.getByTestId("right-split-panel")).toBeInTheDocument();
    expect(roster()).toHaveAttribute("data-mobile-open", "false");
  });

  it("flips the mode on rotation and keeps the selected agent", () => {
    renderAt(1000, 1300);
    expect(cockpit()).toHaveAttribute("data-tablet-master", "overlay");
    fireEvent.click(toggle());
    const rows = agentRows();
    expect(rows.length).toBeGreaterThan(1);
    fireEvent.click(rows[rows.length - 1]);
    const selected = selectedAgentLabel();
    expect(selected).toBeTruthy();

    resizeTo(1000, 700);
    expect(cockpit()).toHaveAttribute("data-tablet-master", "docked");
    expect(roster()).toHaveAttribute("data-desktop-collapsed", "false");
    expect(screen.getByTestId("chat-pane")).toBeInTheDocument();
    expect(selectedAgentLabel()).toBe(selected);

    resizeTo(1000, 1300);
    expect(cockpit()).toHaveAttribute("data-tablet-master", "overlay");
    expect(roster()).toHaveAttribute("data-mobile-open", "false");
    expect(selectedAgentLabel()).toBe(selected);
  });

  it("closes an open drawer when rotating to landscape and restores a visible roster", () => {
    renderAt(1000, 1300);
    fireEvent.click(toggle());
    expect(roster()).toHaveAttribute("data-mobile-open", "true");
    resizeTo(1000, 700);
    expect(roster()).toHaveAttribute("data-mobile-open", "false");
    expect(roster()).toHaveAttribute("data-desktop-collapsed", "false");
  });

  it("keeps the roster collapsed after rotating to landscape with the right dock open", () => {
    renderAt(1000, 1300);
    fireEvent.click(screen.getByTestId("split-view-toggle"));
    resizeTo(1000, 700);
    expect(screen.getByTestId("right-split-panel")).toBeInTheDocument();
    expect(roster()).toHaveAttribute("data-desktop-collapsed", "true");
  });
});
