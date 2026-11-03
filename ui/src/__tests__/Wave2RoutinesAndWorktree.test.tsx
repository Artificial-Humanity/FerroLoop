import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { FerroFlowPane } from "../components/FerroFlowPane";
import { SpinUpAgentModal, computeWorktreeBootstrapPreview } from "../components/SpinUpAgentModal";
import { AgentSettingsModal } from "../components/AgentSettingsModal";

describe("Record #100 — Bounded Scheduled Workflow Routines (FerroFlowPane / HubDrawer)", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it("renders Scheduled Routines subtab with cadence, deduplication, catch-up, and Rule 5 budget admission badges", () => {
    render(<FerroFlowPane workflows={[]} workflowRuns={[]} />);

    const routinesSubtab = screen.getByTestId("subtab-routines");
    expect(routinesSubtab).toBeTruthy();
    fireEvent.click(routinesSubtab);

    const section = screen.getByTestId("scheduled-routines-section");
    expect(section).toBeTruthy();

    // Verify default bounded routines
    expect(screen.getByTestId("routine-card-rt-nightly-conformance")).toBeTruthy();
    expect(screen.getByTestId("routine-cadence-rt-nightly-conformance").textContent).toContain(
      "cron: 0 */6 * * *",
    );
    expect(screen.getByTestId("routine-dedupe-rt-nightly-conformance").textContent).toContain(
      "dedupe: skip-if-running",
    );
    expect(screen.getByTestId("routine-catchup-rt-nightly-conformance").textContent).toContain(
      "catch-up: none",
    );
    expect(screen.getByTestId("routine-budget-rt-nightly-conformance").textContent).toContain(
      "Rule 5 Ceiling: 250k tok · $2.50/run",
    );

    // Second routine with interval, queue-one, and catch-up: single-catchup
    expect(screen.getByTestId("routine-card-rt-ledger-reconcile")).toBeTruthy();
    expect(screen.getByTestId("routine-cadence-rt-ledger-reconcile").textContent).toContain(
      "interval: every 30m",
    );
    expect(screen.getByTestId("routine-dedupe-rt-ledger-reconcile").textContent).toContain(
      "dedupe: queue-one",
    );
    expect(screen.getByTestId("routine-catchup-rt-ledger-reconcile").textContent).toContain(
      "catch-up: single-catchup",
    );
  });

  it("supports Pause / Resume, Run Now, and Cancel controls with clean client-side fallback when endpoint returns 404", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response("Not Found", { status: 404 }),
    );

    render(<FerroFlowPane workflows={[]} workflowRuns={[]} />);

    fireEvent.click(screen.getByTestId("subtab-routines"));

    // Pause active routine
    const pauseResumeBtn = screen.getByTestId("routine-pause-resume-btn-rt-nightly-conformance");
    expect(pauseResumeBtn.textContent).toContain("Pause");
    fireEvent.click(pauseResumeBtn);

    await waitFor(() => {
      expect(
        screen.getByTestId("routine-pause-resume-btn-rt-nightly-conformance").textContent,
      ).toContain("Resume");
    });

    // Trigger Run Now on ledger reconciler, then Cancel
    const runNowBtn = screen.getByTestId("routine-run-now-btn-rt-ledger-reconcile");
    fireEvent.click(runNowBtn);

    await waitFor(() => {
      expect(screen.getByTestId("routine-action-status-banner")).toBeTruthy();
    });

    const cancelBtn = screen.getByTestId("routine-cancel-btn-rt-ledger-reconcile");
    fireEvent.click(cancelBtn);

    await waitFor(() => {
      expect(
        (screen.getByTestId("routine-cancel-btn-rt-ledger-reconcile") as HTMLButtonElement).disabled,
      ).toBe(true);
    });
  });
});

describe("Record #87 — Worktree Bootstrap & Port Allocation UI (SpinUpAgentModal & AgentSettingsModal)", () => {
  it("computes deterministic worktree paths, conflict-free ports, and .fl/worktree.toml bootstrap hooks", () => {
    const createPreview = computeWorktreeBootstrapPreview("felix", "ferroloop", "create");
    expect(createPreview.worktreePath).toBe(
      "~/Projects/Artificial-Humanity/.worktrees/ferroloop/felix",
    );
    expect(createPreview.allocatedPort).toBeGreaterThanOrEqual(4100);
    expect(createPreview.allocatedVitePort).toBe(createPreview.allocatedPort + 1000);
    expect(createPreview.bootstrapHooks.length).toBe(3);

    const sharedPreview = computeWorktreeBootstrapPreview("felix", "ferroloop", "shared");
    expect(sharedPreview.worktreePath).toBe("~/Projects/Artificial-Humanity/ferroloop");
    expect(sharedPreview.bootstrapHooks).toEqual(["fl state sync --shared-checkout"]);
  });

  it("renders worktree policy selector, allocated ports badge, and .fl/worktree.toml hooks in SpinUpAgentModal and submits when changed", async () => {
    const onSpinUp = vi.fn().mockResolvedValue({
      session_id: "sess-felix",
      persona: "felix",
      project: "ferroloop",
      role: "ui-developer",
      vendor: "antigravity",
    });

    render(
      <SpinUpAgentModal
        isOpen={true}
        initialProject="ferroloop"
        onClose={vi.fn()}
        onSpinUp={onSpinUp}
      />,
    );

    expect(screen.getByTestId("spin-up-worktree-preview")).toBeTruthy();
    expect(screen.getByTestId("spin-up-allocated-ports-badge").textContent).toContain("PORT=");
    expect(screen.getByTestId("spin-up-allocated-ports-badge").textContent).toContain(
      "VITE_PORT=",
    );
    expect(screen.getByTestId("spin-up-bootstrap-hooks").textContent).toContain(
      ".fl/worktree.toml",
    );

    // Change persona and worktree policy
    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "felix" },
    });
    fireEvent.change(screen.getByTestId("spin-up-worktree-policy-select"), {
      target: { value: "reuse" },
    });

    expect(screen.getByTestId("spin-up-worktree-path").textContent).toContain(
      "~/Projects/Artificial-Humanity/.worktrees/ferroloop/reused-felix",
    );

    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    await waitFor(() => {
      expect(onSpinUp).toHaveBeenCalledTimes(1);
    });
    const payload = onSpinUp.mock.calls[0][0];
    expect(payload.worktree_policy).toBe("reuse");
    expect(typeof payload.allocated_port).toBe("number");
    expect(typeof payload.allocated_vite_port).toBe("number");
  });

  it("renders Worktree Bootstrap & Port Allocation inspector in AgentSettingsModal", () => {
    render(
      <AgentSettingsModal
        isOpen={true}
        onClose={vi.fn()}
        agentName="felix"
        agentDisplayName="Felix"
        agentRole="ui-developer"
        agentProject="ferroloop"
        adapter="antigravity"
        sessionId="sess-felix"
        selectedModel="claude-opus-4-6"
        selectedThinkingLevel="high"
        onChangeModelAndThinking={vi.fn()}
        contextEstimate={{ usedTokens: 12000, maxTokens: 200000, percent: 6 }}
        canCompact={false}
        compacting={false}
        onCompactContext={vi.fn()}
        canClear={false}
        clearing={false}
        onOpenClearModal={vi.fn()}
        canDecommission={false}
        decommissioning={false}
        onOpenDecommissionModal={vi.fn()}
      />,
    );

    expect(screen.getByTestId("agent-worktree-policy-section")).toBeTruthy();
    expect(screen.getByTestId("agent-allocated-ports-badge").textContent).toContain("PORT=");
    expect(screen.getByTestId("agent-bootstrap-hooks-preview").textContent).toContain(
      "git submodule update --init",
    );

    fireEvent.change(screen.getByTestId("agent-worktree-policy-select"), {
      target: { value: "shared" },
    });
    expect(screen.getByTestId("agent-bootstrap-hooks-preview").textContent).toContain(
      "fl state sync --shared-checkout",
    );
  });
});
