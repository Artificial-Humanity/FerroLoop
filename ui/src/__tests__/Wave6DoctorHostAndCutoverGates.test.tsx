// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  AgentSettingsModal,
  resolveAgentHostSupervisorTelemetry,
} from "../components/AgentSettingsModal";
import {
  DEFAULT_CUTOVER_GATES_SCORECARD,
  FerroWatchPane,
  resolveAgentHostSupervisorTelemetry as ReExportedResolveHostTelemetry,
} from "../components/FerroWatchPane";
import {
  CANONICAL_DOCTOR_CHECK_IDS,
  DEFAULT_DOCTOR_CHECKS,
  SettingsModal,
} from "../components/SettingsModal";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("Wave 6 Item 1 (Record #220 / Issue #205 Phase A3): Observational System Doctor (fl doctor) Panel", () => {
  it("renders all 6 canonical G7 doctor checks and the Observational · Read-Only guarantee badge in SettingsModal", () => {
    expect(CANONICAL_DOCTOR_CHECK_IDS).toEqual([
      "G7.release-identity",
      "G7.receipt-integrity",
      "G7.path-ownership",
      "G7.store-health",
      "G7.vault-perms",
      "G7.code-ui-agree",
    ]);

    render(
      <SettingsModal
        isOpen={true}
        onClose={() => {}}
        initialTab="products"
      />,
    );

    expect(screen.getByTestId("system-doctor-card")).toBeTruthy();
    expect(screen.getByTestId("doctor-observational-badge").textContent).toContain(
      "Observational · Read-Only (Never Migrates)",
    );

    for (const checkId of CANONICAL_DOCTOR_CHECK_IDS) {
      expect(screen.getByTestId(`doctor-check-row-${checkId}`)).toBeTruthy();
      expect(
        screen.getByTestId(`doctor-check-status-${checkId}`).textContent,
      ).toBe("pass");
    }
  });

  it("fetches GET /api/v1/doctor when clicking Run Doctor Check and updates check statuses", async () => {
    const fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue({
      ok: true,
      json: async () => ({
        checks: DEFAULT_DOCTOR_CHECKS.map((c) =>
          c.id === "G7.code-ui-agree"
            ? { ...c, status: "warn", detail: "UI bundle dirty=true" }
            : c,
        ),
      }),
    } as Response);

    render(
      <SettingsModal
        isOpen={true}
        onClose={() => {}}
        initialTab="products"
      />,
    );

    fireEvent.click(screen.getByTestId("doctor-run-check-btn"));

    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledWith("/api/v1/doctor");
      expect(
        screen.getByTestId("doctor-check-status-G7.code-ui-agree").textContent,
      ).toBe("warn");
    });
  });
});

describe("Wave 6 Item 2 (Record #82 / Issue #217): Native fl host Supervisor Telemetry", () => {
  it("resolves pipe-rpc, pty-vt100, and legacy-tmux transport modes and renders supervisor telemetry in AgentSettingsModal and FerroWatchPane", () => {
    expect(ReExportedResolveHostTelemetry).toBe(
      resolveAgentHostSupervisorTelemetry,
    );
    expect(
      resolveAgentHostSupervisorTelemetry("claude", "felix").transportMode,
    ).toBe("pipe-rpc");
    expect(
      resolveAgentHostSupervisorTelemetry("muse", "felix").transportMode,
    ).toBe("pty-vt100");
    expect(
      resolveAgentHostSupervisorTelemetry("tmux", "felix").transportMode,
    ).toBe("legacy-tmux");

    render(
      <AgentSettingsModal
        isOpen={true}
        onClose={() => {}}
        agentName="felix"
        agentDisplayName="Felix"
        agentRole="ui-developer"
        agentProject="FerroLoop"
        adapter="claude"
        sessionId="sess-felix"
        selectedModel="claude-opus-4-6"
        selectedThinkingLevel="high"
        onChangeModelAndThinking={() => {}}
        contextEstimate={{
          usedTokens: 12000,
          maxTokens: 200000,
          percent: 6,
        }}
        executionMode="auto"
        canCompact={false}
        compacting={false}
        onCompactContext={() => {}}
        canClear={true}
        clearing={false}
        onOpenClearModal={() => {}}
        canDecommission={true}
        decommissioning={false}
        onOpenDecommissionModal={() => {}}
      />,
    );

    expect(screen.getByTestId("agent-host-supervisor-telemetry")).toBeTruthy();
    expect(
      screen.getByTestId("agent-host-transport-mode-badge").textContent,
    ).toContain("pipe-rpc");
    expect(
      screen.getByTestId("agent-host-route-authority-badge").textContent,
    ).toContain("FerroWire Store");
    expect(
      screen.getByTestId("agent-host-ring-buffer-usage").textContent,
    ).toContain("Ring Buffer: 128 KB / 1024 KB");
  });
});

describe("Wave 6 Item 3 (Record #83 / Issue #206): Lab Cutover Gates G1–G10 Readiness Scorecard", () => {
  it("renders all 10 cutover gates (G1–G10) and Native fl host summary in FerroWatchPane", () => {
    expect(DEFAULT_CUTOVER_GATES_SCORECARD).toHaveLength(10);

    render(<FerroWatchPane />);

    expect(screen.getByTestId("host-supervisor-telemetry-card")).toBeTruthy();
    expect(
      screen.getByTestId("host-supervisor-route-authority-badge").textContent,
    ).toContain("FerroWire Store");

    expect(screen.getByTestId("cutover-gates-scorecard")).toBeTruthy();
    for (const gate of DEFAULT_CUTOVER_GATES_SCORECARD) {
      expect(screen.getByTestId(`cutover-gate-row-${gate.id}`)).toBeTruthy();
      expect(
        screen.getByTestId(`cutover-gate-status-${gate.id}`).textContent,
      ).toBe(gate.status);
      expect(
        screen.getByTestId(`cutover-gate-evidence-${gate.id}`).textContent,
      ).toBe(gate.evidence);
    }
  });
});
