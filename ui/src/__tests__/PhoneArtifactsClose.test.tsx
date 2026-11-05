import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import App from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";

// The real pane never calls onClose today; a stand-in exposes the handler App passes down.
vi.mock("../components/ArtifactPreviewPane", () => ({
  ArtifactPreviewPane: ({ onClose }: { onClose?: () => void }) => (
    <div data-testid="artifact-preview-pane">
      <button type="button" data-testid="stub-artifact-close" onClick={onClose} disabled={!onClose}>
        close
      </button>
    </div>
  ),
}));

function makeApiClient(): HubApiClient {
  const fetchImpl = vi.fn(
    async () =>
      new Response(JSON.stringify({}), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
  );
  return new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
}

function makeWsClient(): HubWsClient {
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

describe("Artifacts pane close", () => {
  const originalWidth = window.innerWidth;
  const originalHeight = window.innerHeight;

  afterEach(() => {
    setViewport(originalWidth, originalHeight);
  });

  it("on the phone returns to Chat without touching the desktop split view", () => {
    window.localStorage.clear();
    setViewport(1280, 800);
    render(<App apiClient={makeApiClient()} wsClient={makeWsClient()} />);
    fireEvent.click(screen.getByTestId("split-view-toggle"));
    expect(screen.getByTestId("split-view-toggle")).toHaveAttribute("aria-pressed", "true");

    resizeTo(390, 844);
    fireEvent.click(screen.getByTestId("phone-tab-more"));
    fireEvent.click(screen.getByTestId("phone-more-item-artifacts"));
    expect(screen.getByTestId("phone-shell")).toHaveAttribute("data-phone-view", "artifacts");
    fireEvent.click(screen.getByTestId("stub-artifact-close"));
    expect(screen.getByTestId("phone-shell")).toHaveAttribute("data-phone-view", "chat");

    resizeTo(1280, 800);
    expect(screen.getByTestId("split-view-toggle")).toHaveAttribute("aria-pressed", "true");
  });
});
