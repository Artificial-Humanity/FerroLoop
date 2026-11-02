import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import "@testing-library/jest-dom/vitest";
import {
  AgentSettingsModal,
  formatExecutionMode,
  type AgentSettingsModalProps,
} from "../components/AgentSettingsModal";
import type { HubApiClient } from "../api/client";

function modalProps(overrides: Partial<AgentSettingsModalProps> = {}): AgentSettingsModalProps {
  return {
    isOpen: true,
    onClose: () => {},
    agentName: "ferris",
    agentDisplayName: "Ferris",
    sessionId: "sess-1",
    adapter: "claude",
    selectedModel: "claude-opus-5-5",
    selectedThinkingLevel: "high",
    onChangeModelAndThinking: () => {},
    contextEstimate: { usedTokens: 1000, maxTokens: 200000, percent: 1 },
    canCompact: false,
    compacting: false,
    onCompactContext: () => {},
    canClear: true,
    clearing: false,
    onOpenClearModal: () => {},
    canDecommission: true,
    decommissioning: false,
    onOpenDecommissionModal: () => {},
    ...overrides,
  };
}

describe("AgentSettingsModal Execution Mode block", () => {
  it.each([
    ["auto", "Auto"],
    ["yolo", "Yolo"],
    ["plan", "Plan"],
    ["default", "Default"],
    ["accept-edits", "Accept edits"],
    ["always-approve", "always-approve"],
  ])("labels the %s mode as %s", (mode, label) => {
    render(<AgentSettingsModal {...modalProps({ executionMode: mode })} />);
    const badge = screen.getByTestId("execution-mode-badge");
    expect(badge).toHaveTextContent(label);
    expect(badge).toHaveAttribute("data-mode", mode);
    expect(screen.getByTestId("agent-settings-execution-mode")).toHaveTextContent(
      "Execution Mode",
    );
    expect(screen.getByTestId("execution-mode-note")).toHaveTextContent("Read-only");
  });

  it("styles only yolo as a warning", () => {
    const { rerender } = render(<AgentSettingsModal {...modalProps({ executionMode: "yolo" })} />);
    expect(screen.getByTestId("execution-mode-badge")).toHaveAttribute("data-warning", "true");
    expect(screen.getByTestId("execution-mode-badge")).toHaveAttribute(
      "title",
      expect.stringContaining("without asking"),
    );
    for (const mode of ["auto", "plan", "default", "accept-edits", "other", null]) {
      rerender(<AgentSettingsModal {...modalProps({ executionMode: mode })} />);
      expect(screen.getByTestId("execution-mode-badge")).not.toHaveAttribute("data-warning");
    }
  });

  it("shows Unknown when the hub reports no mode", () => {
    render(<AgentSettingsModal {...modalProps({ executionMode: null })} />);
    expect(screen.getByTestId("execution-mode-badge")).toHaveTextContent("Unknown");
    expect(screen.getByTestId("execution-mode-badge")).toHaveAttribute("data-mode", "unknown");
  });

  it("does not render the vendor quota summary even when a caller still passes quotaUsage", () => {
    render(
      <AgentSettingsModal
        {...modalProps({
          executionMode: "auto",
          quotaUsage: {
            tier: "plus",
            primary_window: { used_percent: 100, window_minutes: 300, resets_at_ms: Date.now() },
            exhausted_reason: "HTTP 429",
          },
        })}
      />,
    );
    expect(screen.queryByTestId("agent-settings-quota-summary")).toBeNull();
    expect(screen.queryByTestId("quota-usage-badge")).toBeNull();
    expect(screen.queryByTestId("quota-tier-chip")).toBeNull();
    expect(screen.queryByText("Vendor Account Quota")).toBeNull();
    expect(screen.queryByText("HTTP 429")).toBeNull();
  });

  it("reads the mode from the sessions list when the executionMode prop is omitted", async () => {
    const getSessions = vi.fn().mockResolvedValue({
      sessions: [
        { id: "other", source: "route", adapter: "codex", status: "active", execution_mode: "plan" },
        { id: "sess-1", source: "route", adapter: "claude", status: "active", execution_mode: "yolo" },
      ],
    });
    render(
      <AgentSettingsModal
        {...modalProps({ apiClient: { getSessions } as unknown as HubApiClient })}
      />,
    );
    expect(screen.getByTestId("execution-mode-badge")).toHaveTextContent("Unknown");
    await waitFor(() =>
      expect(screen.getByTestId("execution-mode-badge")).toHaveTextContent("Yolo"),
    );
    expect(getSessions).toHaveBeenCalledTimes(1);
  });

  it("falls back to the persona match, stays Unknown on a failed fetch, and does not fetch when the prop is given", async () => {
    const byPersona = vi.fn().mockResolvedValue({
      sessions: [
        { id: "x", source: "route", adapter: "claude", persona: "Ferris", status: "active", execution_mode: "accept-edits" },
      ],
    });
    const { unmount } = render(
      <AgentSettingsModal
        {...modalProps({ sessionId: "sess-gone", apiClient: { getSessions: byPersona } as unknown as HubApiClient })}
      />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("execution-mode-badge")).toHaveTextContent("Accept edits"),
    );
    unmount();

    const failing = vi.fn().mockRejectedValue(new Error("down"));
    render(
      <AgentSettingsModal
        {...modalProps({ apiClient: { getSessions: failing } as unknown as HubApiClient })}
      />,
    );
    await waitFor(() => expect(failing).toHaveBeenCalled());
    expect(screen.getByTestId("execution-mode-badge")).toHaveTextContent("Unknown");

    const unused = vi.fn();
    render(
      <AgentSettingsModal
        {...modalProps({ executionMode: "plan", apiClient: { getSessions: unused } as unknown as HubApiClient })}
      />,
    );
    expect(unused).not.toHaveBeenCalled();
  });
});

describe("formatExecutionMode", () => {
  it("maps the vocabulary, passes raw values through and names a missing one Unknown", () => {
    expect(formatExecutionMode("ACCEPT-EDITS")).toBe("Accept edits");
    expect(formatExecutionMode("dontask")).toBe("dontask");
    expect(formatExecutionMode("  ")).toBe("Unknown");
    expect(formatExecutionMode(undefined)).toBe("Unknown");
  });
});
