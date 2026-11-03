import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import {
  AgentSettingsModal,
  evaluateAgentMcpPosture,
} from "../components/AgentSettingsModal";
import { HubDrawer } from "../components/HubDrawer";
import { SettingsModal } from "../components/SettingsModal";

describe("Record #95 — Desired / Applied / Live-Attested MCP & Agent Posture Drift Badges", () => {
  it("evaluates 3-tier posture status across synced, unapplied_catalog, stale_session, and extra_server", () => {
    // 1. Synced
    const synced = evaluateAgentMcpPosture({
      agentName: "ferris",
      project: "FerroLoop",
      role: "lead",
      adapter: "claude",
      sessionId: "claude:ferris",
    });
    expect(synced.driftKind).toBe("synced");
    expect(synced.vendorConfigPath).toBe("~/.claude.json");

    // 2. Unapplied catalog change
    const unapplied = evaluateAgentMcpPosture({
      agentName: "felix",
      project: "FerroLoop",
      role: "ui-developer",
      adapter: "antigravity",
      sessionId: "antigravity:felix",
      desiredMcpServers: ["fl-state", "agent-bus", "ferrogate-github"],
      appliedMcpServers: ["fl-state", "agent-bus"],
      liveMcpServers: ["fl-state", "agent-bus"],
    });
    expect(unapplied.driftKind).toBe("unapplied_catalog");
    expect(unapplied.missingInApplied).toEqual(["ferrogate-github"]);
    expect(unapplied.remediationLabel).toBe("Sync MCP Config");

    // 3. Stale session needing reforge after MCP sync or role change
    const stale = evaluateAgentMcpPosture({
      agentName: "feira",
      project: "FerroLoop",
      role: "system-developer",
      adapter: "codex",
      sessionId: "codex:feira",
      desiredMcpServers: ["fl-state", "agent-bus"],
      appliedMcpServers: ["fl-state", "agent-bus"],
      liveMcpServers: ["fl-state"],
      sessionStartedBeforeSync: true,
    });
    expect(stale.driftKind).toBe("stale_session");
    expect(stale.missingInLive).toEqual(["agent-bus"]);
    expect(stale.remediationLabel).toBe("Reforge / Restart Session");

    // 4. Unregistered extra server
    const extra = evaluateAgentMcpPosture({
      agentName: "fiona",
      project: "FerroLoop",
      role: "strategist",
      adapter: "codex",
      sessionId: "codex:fiona",
      desiredMcpServers: ["fl-state", "agent-bus"],
      appliedMcpServers: ["fl-state", "agent-bus", "rogue-mcp"],
      liveMcpServers: ["fl-state", "agent-bus", "rogue-mcp"],
    });
    expect(extra.driftKind).toBe("extra_server");
    expect(extra.extraInAppliedOrLive).toEqual(["rogue-mcp"]);
    expect(extra.remediationLabel).toBe("Prune Extra Servers");
  });

  it("renders 3-tier posture section in AgentSettingsModal and executes one-click remediation for unapplied catalog, stale session, and extra server", () => {
    const onRemediate = vi.fn();
    const baseProps = {
      isOpen: true,
      onClose: vi.fn(),
      agentName: "felix",
      agentDisplayName: "Felix",
      agentRole: "ui-developer",
      agentProject: "FerroLoop",
      adapter: "antigravity",
      sessionId: "antigravity:felix",
      selectedModel: "gemini-3.1-pro",
      selectedThinkingLevel: "high",
      onChangeModelAndThinking: vi.fn(),
      contextEstimate: { usedTokens: 20_000, maxTokens: 200_000, percent: 10 },
      canCompact: false,
      compacting: false,
      onCompactContext: vi.fn(),
      canClear: true,
      clearing: false,
      onOpenClearModal: vi.fn(),
      canDecommission: true,
      decommissioning: false,
      onOpenDecommissionModal: vi.fn(),
      onRemediatePosture: onRemediate,
    };

    const { rerender } = render(
      <AgentSettingsModal
        {...baseProps}
        postureInput={{
          desiredMcpServers: ["fl-state", "agent-bus", "ferrogate-vault"],
          appliedMcpServers: ["fl-state", "agent-bus"],
          liveMcpServers: ["fl-state", "agent-bus"],
        }}
      />,
    );

    const postureSection = screen.getByTestId("agent-posture-section");
    expect(
      within(postureSection).getByTestId("agent-posture-status-badge"),
    ).toHaveAttribute("data-posture", "unapplied_catalog");
    expect(within(postureSection).getByTestId("posture-tier-desired")).toHaveTextContent(
      "fl-state, agent-bus, ferrogate-vault",
    );

    // Click Sync MCP Config -> transitions to stale_session because live session still has 2 servers
    fireEvent.click(within(postureSection).getByTestId("posture-remediate-sync-btn"));
    expect(onRemediate).toHaveBeenCalledWith(
      "sync_mcp",
      expect.objectContaining({ driftKind: "unapplied_catalog" }),
    );
    expect(
      within(postureSection).getByTestId("agent-posture-status-badge"),
    ).toHaveAttribute("data-posture", "stale_session");

    // Click Reforge / Restart Session -> transitions to synced
    fireEvent.click(within(postureSection).getByTestId("posture-remediate-reforge-btn"));
    expect(onRemediate).toHaveBeenCalledWith(
      "reforge_session",
      expect.objectContaining({ driftKind: "stale_session" }),
    );
    expect(
      within(postureSection).getByTestId("agent-posture-status-badge"),
    ).toHaveAttribute("data-posture", "synced");

    // Verify extra_server prune button
    rerender(
      <AgentSettingsModal
        {...baseProps}
        agentName="fiona"
        postureInput={{
          desiredMcpServers: ["fl-state", "agent-bus"],
          appliedMcpServers: ["fl-state", "agent-bus", "untracked-mcp"],
          liveMcpServers: ["fl-state", "agent-bus", "untracked-mcp"],
        }}
      />,
    );
    expect(screen.getByTestId("agent-posture-status-badge")).toHaveAttribute(
      "data-posture",
      "extra_server",
    );
    fireEvent.click(screen.getByTestId("posture-remediate-prune-btn"));
    expect(screen.getByTestId("agent-posture-status-badge")).toHaveAttribute(
      "data-posture",
      "synced",
    );
  });

  it("surfaces posture drift badges and one-click Reconcile in SettingsModal and HubDrawer (with dry-run preview and rollback)", () => {
    const onReconcileAgent = vi.fn();
    render(
      <SettingsModal
        isOpen={true}
        onClose={vi.fn()}
        initialTab="agents"
        agents={[
          {
            id: "urn:fl:agent:ferris",
            name_key: "ferris",
            display_name: "Ferris",
            project: "FerroLoop",
            role: "lead",
            status: "active",
          },
          {
            id: "urn:fl:agent:felix",
            name_key: "felix",
            display_name: "Felix",
            project: "FerroLoop",
            role: "ui-developer",
            status: "active",
          },
        ]}
        postureInputsByAgent={{
          felix: {
            desiredMcpServers: ["fl-state", "agent-bus", "ferrogate"],
            appliedMcpServers: ["fl-state", "agent-bus"],
            liveMcpServers: ["fl-state", "agent-bus"],
          },
        }}
        onReconcileAgentPosture={onReconcileAgent}
      />,
    );

    expect(screen.getByTestId("settings-agent-posture-badge-ferris")).toHaveAttribute(
      "data-posture",
      "synced",
    );
    expect(screen.getByTestId("settings-agent-posture-badge-felix")).toHaveAttribute(
      "data-posture",
      "unapplied_catalog",
    );
    fireEvent.click(screen.getByTestId("settings-agent-reconcile-btn-felix"));
    expect(onReconcileAgent).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("settings-agent-posture-badge-felix")).toHaveAttribute(
      "data-posture",
      "synced",
    );

    // HubDrawer 3-Tier Posture card with dry-run preview, reconcile all, and rollback
    const onHubReconcile = vi.fn();
    render(
      <HubDrawer
        postureItems={[
          {
            agentName: "ferris",
            project: "FerroLoop",
            role: "lead",
            adapter: "claude",
            sessionId: "claude:ferris",
          },
          {
            agentName: "feira",
            project: "FerroLoop",
            role: "system-developer",
            adapter: "codex",
            sessionId: "codex:feira",
            sessionStartedBeforeSync: true,
          },
        ]}
        onReconcilePosture={onHubReconcile}
      />,
    );

    const hubCard = screen.getByTestId("hub-posture-drift-card");
    expect(within(hubCard).getByTestId("hub-posture-summary-badge")).toHaveTextContent(
      "1 Drifted / 2 Total",
    );

    fireEvent.click(within(hubCard).getByTestId("hub-posture-dryrun-btn"));
    expect(within(hubCard).getByTestId("hub-posture-dryrun-panel")).toHaveTextContent(
      "[dry-run] feira: Reforge / Restart Session",
    );

    fireEvent.click(within(hubCard).getByTestId("hub-posture-reconcile-btn"));
    expect(onHubReconcile).toHaveBeenCalledTimes(1);
    expect(within(hubCard).getByTestId("hub-posture-summary-badge")).toHaveTextContent(
      "All 2 Synced",
    );

    fireEvent.click(within(hubCard).getByTestId("hub-posture-rollback-btn"));
    expect(within(hubCard).getByTestId("hub-posture-summary-badge")).toHaveTextContent(
      "1 Drifted / 2 Total",
    );
  });
});
