import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import {
  COCKPIT_TABS,
  ContextHeader,
  RightPanelHeader,
  getTabContextTier,
} from "../components/ContextHeader";
import {
  DEFAULT_WORKSPACE_AGENTS,
  type CockpitTab,
  type DiscoveredSessionSummary,
} from "../types/api";

describe("Record 56 — FerroDeck+ 3-Tier Navigation Hierarchy", () => {
  it("maps all 9 cockpit tabs to their corresponding tier (agent, project, system)", () => {
    const expectedTiers: Record<CockpitTab, "agent" | "project" | "system"> = {
      chat: "agent",
      mail: "agent",
      artifacts: "project",
      git: "project",
      board: "project",
      terminal: "system",
      hub: "system",
      gate: "system",
      state: "system",
    };

    expect(COCKPIT_TABS).toHaveLength(9);
    for (const tab of COCKPIT_TABS) {
      expect(getTabContextTier(tab.id)).toBe(expectedTiers[tab.id]);
    }
  });

  it("renders tier-agent-nav, tier-project-nav, and tier-system-nav inside cockpit-tab-bar with their respective badges, tabs, and actions", () => {
    const ferris = DEFAULT_WORKSPACE_AGENTS.find((a) => a.name_key === "ferris")!;
    const sessions: DiscoveredSessionSummary[] = [
      {
        id: "sess-ferris-99",
        source: "route",
        adapter: "claude",
        persona: "ferris",
        status: "active",
        cwd: "/tmp/felix-three-tier-navigation",
      },
    ];

    render(
      <ContextHeader
        selectedAgent={ferris}
        sessions={sessions}
        selectedSessionId="sess-ferris-99"
        onSelectSession={vi.fn()}
        activeTab="chat"
        onSelectTab={vi.fn()}
        splitView={false}
        onToggleSplitView={vi.fn()}
        authority={{ tier: "owner", login: "lmcfarlin" }}
        wsConnected={true}
        onlineNodesCount={1}
        onOpenNodesModal={vi.fn()}
        onOpenSettingsModal={vi.fn()}
        onOpenAgentSettings={vi.fn()}
        onOpenSpinUpModal={vi.fn()}
        onOpenPairingModal={vi.fn()}
      />,
    );

    const tabBar = screen.getByTestId("cockpit-tab-bar");
    expect(within(tabBar).getAllByRole("tab")).toHaveLength(5);

    // Tier 1: Agent Context (includes active-project-badge; Wire Mail, Artifacts, Git & PRs, and Records & Board moved to RightPanelHeader)
    const agentNav = within(tabBar).getByTestId("tier-agent-nav");
    expect(within(agentNav).getByText("Ferris")).toHaveAttribute(
      "title",
      "Active Agent Name",
    );
    expect(within(agentNav).getByRole("tab", { name: /^Chat$/i })).toBeInTheDocument();
    expect(within(agentNav).queryByRole("tab", { name: /^Wire Mail$/i })).toBeNull();
    expect(within(agentNav).queryByTestId("header-agent-settings-btn")).toBeNull();
    const projectBadge = within(agentNav).getByTestId("active-project-badge");
    expect(projectBadge).toHaveTextContent("FerroLoop");
    expect(projectBadge).toHaveAttribute("title", "Current Project");
    expect(within(tabBar).queryByTestId("tier-project-nav")).toBeNull();
    expect(within(agentNav).queryByTestId("session-cwd-badge")).toBeNull();
    expect(within(agentNav).queryByRole("tab", { name: /^Artifacts$/i })).toBeNull();
    expect(within(agentNav).queryByRole("tab", { name: /^Git & PRs$/i })).toBeNull();
    expect(
      within(agentNav).queryByRole("tab", { name: /^Records & Board$/i }),
    ).toBeNull();
    expect(within(agentNav).queryByTestId("split-view-toggle")).toBeNull();

    // Tier 3: System Context (inline inside tier-system-nav)
    const systemNav = within(tabBar).getByTestId("tier-system-nav");
    expect(within(systemNav).getByRole("tab", { name: /^Terminal$/i })).toBeInTheDocument();
    expect(within(systemNav).getByRole("tab", { name: /^Hub & Arch$/i })).toBeInTheDocument();
    expect(within(systemNav).getByRole("tab", { name: /^Vault & Gate$/i })).toBeInTheDocument();
    expect(within(systemNav).getByRole("tab", { name: /^FerroState$/i })).toBeInTheDocument();

    expect(within(systemNav).getByTestId("nodes-modal-btn")).toBeInTheDocument();
    expect(within(systemNav).getByTestId("header-spin-up-btn")).toBeInTheDocument();
    expect(within(systemNav).getByTestId("authority-status-btn")).toBeInTheDocument();
    expect(within(systemNav).queryByTestId("system-utility-bar")).toBeNull();
  });

  it("renders RightPanelHeader icon tabs with tooltips and hides Wire Mail when selectedAgent is null", () => {
    const ferris = DEFAULT_WORKSPACE_AGENTS.find((a) => a.name_key === "ferris")!;
    const onSelectTab = vi.fn();

    const { rerender } = render(
      <RightPanelHeader
        selectedAgent={ferris}
        rightSplitTab="git"
        onSelectTab={onSelectTab}
      />,
    );

    const header = screen.getByTestId("right-panel-header");
    expect(within(header).getByTestId("right-panel-tab-mail")).toHaveAttribute(
      "title",
      "Wire Mail",
    );
    expect(within(header).getByTestId("right-panel-tab-artifacts")).toHaveAttribute(
      "title",
      "Artifacts",
    );
    expect(within(header).getByTestId("right-panel-tab-git")).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(within(header).getByTestId("right-panel-tab-board")).toHaveAttribute(
      "title",
      "Records & Board",
    );
    expect(within(header).getByTestId("right-panel-active-title")).toHaveTextContent(
      "Git & PRs",
    );

    // When selectedAgent is null, Wire Mail is hidden and 'mail' falls back to 'artifacts'
    rerender(
      <RightPanelHeader
        selectedAgent={null}
        rightSplitTab="mail"
        onSelectTab={onSelectTab}
      />,
    );
    expect(within(header).queryByTestId("right-panel-tab-mail")).toBeNull();
    expect(within(header).getByTestId("right-panel-tab-artifacts")).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(within(header).getByTestId("right-panel-active-title")).toHaveTextContent(
      "Artifacts",
    );
  });

  it("updates active-context-tier-badge when opening right split panel via split-view-toggle and switching views in right-panel-header", async () => {
    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const json = (body: unknown) =>
        new Response(JSON.stringify(body), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      if (url.includes("/api/v1/auth/session")) {
        return json({
          authenticated: true,
          authority: { tier: "owner", login: "lmcfarlin" },
        });
      }
      if (url.includes("/api/v1/sessions") && !url.includes("/transcript")) {
        return json({
          sessions: [
            {
              id: "sess-ferris-01",
              source: "route",
              adapter: "claude",
              persona: "ferris",
              status: "active",
              cwd: "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop",
            },
          ],
        });
      }
      return json({});
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const wsStub = {
      setToken: vi.fn(),
      connect: vi.fn(),
      disconnect: vi.fn(),
      onStatusChange: () => () => {},
      subscribe: () => () => {},
    } as unknown as HubWsClient;

    const { unmount } = render(<App apiClient={apiClient} wsClient={wsStub} />);

    // Default tab is Chat (Agent tier)
    const scopeBadge = await screen.findByTestId("active-context-tier-badge");
    expect(scopeBadge).toHaveAttribute("data-tier", "agent");
    expect(scopeBadge).toHaveTextContent("Scope: Agent · Ferris");
    expect(screen.queryByTestId("chat-artifacts-toggle-btn")).toBeNull();
    const tabBar = screen.getByTestId("cockpit-tab-bar");
    const rosterToggleBtn = within(tabBar).getByTestId("mobile-sidebar-toggle-btn");
    const agentNav = within(tabBar).getByTestId("tier-agent-nav");
    const splitToggleBtn = within(tabBar).getByTestId("split-view-toggle");
    expect(rosterToggleBtn.nextElementSibling).toBe(agentNav);
    expect(splitToggleBtn).toHaveAttribute("aria-pressed", "false");

    const chatPane = screen.getByTestId("chat-pane");
    expect(within(chatPane).queryByTestId("mobile-sidebar-toggle-btn")).toBeNull();
    expect(within(chatPane).queryByTestId("split-view-toggle")).toBeNull();
    const filterInput = within(chatPane).getByPlaceholderText(
      /Filter transcript turns, tools, or reasoning/i,
    );
    expect(filterInput.style.fontSize).toBe("16px");

    // Open right split panel via split-view-toggle -> mounts right-panel-header
    fireEvent.click(splitToggleBtn);
    expect(splitToggleBtn).toHaveAttribute("aria-pressed", "true");
    const rightHeader = screen.getByTestId("right-panel-header");
    expect(rightHeader).toBeInTheDocument();

    // Select Artifacts, Wire Mail, Records & Board, and Git & PRs from right-panel-header
    fireEvent.click(within(rightHeader).getByTestId("right-panel-tab-artifacts"));
    expect(screen.getByTestId("active-context-tier-badge")).toHaveAttribute(
      "data-tier",
      "project",
    );
    expect(within(rightHeader).getByTestId("right-panel-active-title")).toHaveTextContent(
      "Artifacts",
    );

    fireEvent.click(within(rightHeader).getByTestId("right-panel-tab-mail"));
    expect(screen.getByTestId("active-context-tier-badge")).toHaveAttribute(
      "data-tier",
      "agent",
    );
    expect(within(rightHeader).getByTestId("right-panel-active-title")).toHaveTextContent(
      "Wire Mail",
    );

    fireEvent.click(within(rightHeader).getByTestId("right-panel-tab-board"));
    expect(screen.getByTestId("active-context-tier-badge")).toHaveAttribute(
      "data-tier",
      "project",
    );
    expect(within(rightHeader).getByTestId("right-panel-active-title")).toHaveTextContent(
      "Records & Board",
    );

    fireEvent.click(within(rightHeader).getByTestId("right-panel-tab-git"));
    expect(screen.getByTestId("active-context-tier-badge")).toHaveAttribute(
      "data-tier",
      "project",
    );
    expect(screen.getByTestId("active-context-tier-badge")).toHaveTextContent(
      "Scope: Project · FerroLoop",
    );
    expect(screen.getByTestId("chat-pane")).toBeInTheDocument();

    // Clicking split-view-toggle closes the right split panel and returns to Chat
    fireEvent.click(splitToggleBtn);
    expect(splitToggleBtn).toHaveAttribute("aria-pressed", "false");
    expect(screen.queryByTestId("right-panel-header")).toBeNull();
    expect(screen.getByTestId("active-context-tier-badge")).toHaveAttribute(
      "data-tier",
      "agent",
    );

    // Switch to Hub & Arch (System tier) -> opens right split panel on Hub & Arch
    fireEvent.click(within(tabBar).getByRole("tab", { name: /^Hub & Arch$/i }));
    expect(screen.getByTestId("active-context-tier-badge")).toHaveAttribute(
      "data-tier",
      "system",
    );
    expect(screen.getByTestId("active-context-tier-badge")).toHaveTextContent(
      "Scope: System · AI-Lab-0",
    );
    expect(splitToggleBtn).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("chat-pane")).toBeInTheDocument();

    unmount();
  });
});
