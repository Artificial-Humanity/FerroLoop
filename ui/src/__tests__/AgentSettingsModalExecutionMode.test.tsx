import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor, fireEvent, within } from "@testing-library/react";
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

  it("falls back to the persona match, stays Unknown on a failed fetch, and still refreshes when the prop is given", async () => {
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
    await waitFor(() => expect(unused).toHaveBeenCalledTimes(1));
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

describe("AgentSettingsModal 3-tab layout, pinned header, scrollable body, and pinned lifecycle footer", () => {
  it("renders a pinned header with 3 tabs, a scrollable body with 3 mounted panels, and a pinned footer with the 4 lifecycle actions", () => {
    render(<AgentSettingsModal {...modalProps()} />);

    const modal = screen.getByTestId("agent-settings-modal");
    const header = screen.getByTestId("agent-settings-header");
    const scrollBody = screen.getByTestId("agent-settings-scroll-body");
    const footer = screen.getByTestId("agent-settings-footer");

    expect(modal).toContainElement(header);
    expect(modal).toContainElement(scrollBody);
    expect(modal).toContainElement(footer);
    expect(header.style.flexShrink).toBe("0");
    expect(scrollBody.style.overflowY).toBe("auto");
    expect(scrollBody.style.flex).toContain("1");
    expect(footer.style.flexShrink).toBe("0");

    // All 4 lifecycle buttons live in the static footer and outside the scrollable body
    const recastBtn = within(footer).getByTestId("recast-agent-btn-ferris");
    const compactBtn = within(footer).getByTestId("compact-context-btn");
    const clearBtn = within(footer).getByTestId("open-clear-reforge-modal-btn");
    const decommBtn = within(footer).getByTestId("open-decommission-modal-btn");
    expect(recastBtn).toHaveTextContent("Re-Cast");
    expect(compactBtn).toHaveTextContent("Compact");
    expect(clearBtn).toHaveTextContent("Clear / Reforge");
    expect(decommBtn).toHaveTextContent("Decommission");
    expect(scrollBody).not.toContainElement(recastBtn);
    expect(scrollBody).not.toContainElement(compactBtn);
    expect(scrollBody).not.toContainElement(clearBtn);
    expect(scrollBody).not.toContainElement(decommBtn);

    // 3-tab bar inside pinned header
    const tablist = within(header).getByTestId("agent-settings-tabs");
    expect(tablist).toHaveAttribute("role", "tablist");
    const tabOverview = within(tablist).getByTestId("agent-settings-tab-overview");
    const tabPermissions = within(tablist).getByTestId("agent-settings-tab-permissions");
    const tabRuntime = within(tablist).getByTestId("agent-settings-tab-runtime");
    expect(tabOverview).toHaveTextContent("Overview & Model");
    expect(tabPermissions).toHaveTextContent("Permissions & Sandbox");
    expect(tabRuntime).toHaveTextContent("MCP & Runtime");

    const panelOverview = within(scrollBody).getByTestId("agent-settings-panel-overview");
    const panelPermissions = within(scrollBody).getByTestId("agent-settings-panel-permissions");
    const panelRuntime = within(scrollBody).getByTestId("agent-settings-panel-runtime");

    // Default tab is Overview & Model
    expect(tabOverview).toHaveAttribute("aria-selected", "true");
    expect(tabPermissions).toHaveAttribute("aria-selected", "false");
    expect(tabRuntime).toHaveAttribute("aria-selected", "false");
    expect(panelOverview.style.display).toBe("flex");
    expect(panelPermissions.style.display).toBe("none");
    expect(panelRuntime.style.display).toBe("none");
    expect(panelOverview).toContainElement(screen.getByTestId("agent-settings-identity"));

    // Switch to Permissions & Sandbox tab
    fireEvent.click(tabPermissions);
    expect(tabOverview).toHaveAttribute("aria-selected", "false");
    expect(tabPermissions).toHaveAttribute("aria-selected", "true");
    expect(tabRuntime).toHaveAttribute("aria-selected", "false");
    expect(panelOverview.style.display).toBe("none");
    expect(panelPermissions.style.display).toBe("flex");
    expect(panelRuntime.style.display).toBe("none");
    expect(panelPermissions).toContainElement(screen.getByTestId("agent-permissions-section"));
    // Footer remains visible across tabs
    expect(within(footer).getByTestId("recast-agent-btn-ferris")).toBeInTheDocument();

    // Switch to MCP & Runtime tab
    fireEvent.click(tabRuntime);
    expect(tabOverview).toHaveAttribute("aria-selected", "false");
    expect(tabPermissions).toHaveAttribute("aria-selected", "false");
    expect(tabRuntime).toHaveAttribute("aria-selected", "true");
    expect(panelOverview.style.display).toBe("none");
    expect(panelPermissions.style.display).toBe("none");
    expect(panelRuntime.style.display).toBe("flex");
    expect(panelRuntime).toContainElement(screen.getByTestId("agent-posture-section"));
    expect(panelRuntime).toContainElement(screen.getByTestId("agent-worktree-policy-section"));
    expect(panelRuntime).toContainElement(screen.getByTestId("agent-host-supervisor-telemetry"));
    expect(within(footer).getByTestId("open-decommission-modal-btn")).toBeInTheDocument();
  });

  it("supports initialTab prop to open directly on permissions or runtime", () => {
    render(<AgentSettingsModal {...modalProps({ initialTab: "runtime" })} />);
    expect(screen.getByTestId("agent-settings-tab-runtime")).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(screen.getByTestId("agent-settings-panel-runtime").style.display).toBe("flex");
    expect(screen.getByTestId("agent-settings-panel-overview").style.display).toBe("none");
  });

  it("Record #119: reflows AgentSettingsModal cleanly on narrow phone viewports without horizontal overflow or clipped selects", () => {
    render(<AgentSettingsModal {...modalProps({ initialTab: "permissions" })} />);

    const modal = screen.getByTestId("agent-settings-modal");
    expect(modal.style.minWidth).toBe("0");
    expect(modal.style.boxSizing).toBe("border-box");

    const scrollBody = screen.getByTestId("agent-settings-scroll-body");
    expect(scrollBody).toHaveClass("fd-scroll");
    expect(scrollBody.style.overflowX).toBe("hidden");

    const tabOverview = screen.getByTestId("agent-settings-tab-overview");
    const tabPermissions = screen.getByTestId("agent-settings-tab-permissions");
    expect(tabOverview).toHaveTextContent("Overview & Model");
    expect(tabOverview.querySelector(".sr-only")).toHaveTextContent("& Model");
    expect(tabPermissions).toHaveTextContent("Permissions & Sandbox");
    expect(tabPermissions.querySelector(".sr-only")).toHaveTextContent("& Sandbox");

    const postureSelect = screen.getByTestId("agent-posture-select");
    const sandboxSelect = screen.getByTestId("agent-sandbox-select");
    const rulePresetSelect = screen.getByTestId("agent-rule-preset-select");
    const unenforcedSelect = screen.getByTestId("agent-unenforced-defaults-select");
    const modeSelect = screen.getByTestId("agent-permission-mode-select");
    expect(postureSelect.style.boxSizing).toBe("border-box");
    expect(sandboxSelect.style.boxSizing).toBe("border-box");
    expect(rulePresetSelect.style.boxSizing).toBe("border-box");
    expect(unenforcedSelect.style.boxSizing).toBe("border-box");
    expect(modeSelect.style.boxSizing).toBe("border-box");

    const compileReceipt = screen.getByTestId("agent-compile-receipt");
    expect(compileReceipt.style.overflowWrap).toBe("anywhere");

    fireEvent.click(screen.getByTestId("agent-settings-tab-runtime"));
    const postureTiers = screen.getByTestId("agent-posture-tiers");
    expect(postureTiers.style.gridTemplateColumns).toBe(
      "repeat(auto-fit, minmax(140px, 1fr))",
    );
  });
});
