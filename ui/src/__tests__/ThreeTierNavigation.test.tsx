import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import {
  COCKPIT_TABS,
  ContextHeader,
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
    expect(within(tabBar).getAllByRole("tab")).toHaveLength(9);

    // Tier 1: Agent Context (header-agent-settings-btn removed; per-agent settings lives in RosterSidebar & SettingsModal)
    const agentNav = within(tabBar).getByTestId("tier-agent-nav");
    expect(within(agentNav).getByText("Ferris")).toBeInTheDocument();
    expect(within(agentNav).getByRole("tab", { name: /^Chat$/i })).toBeInTheDocument();
    expect(within(agentNav).getByRole("tab", { name: /^Wire Mail$/i })).toBeInTheDocument();
    expect(within(agentNav).queryByTestId("header-agent-settings-btn")).toBeNull();

    // Tier 2: Project Context (session-cwd-badge moved into Project Settings summary card)
    const projectNav = within(tabBar).getByTestId("tier-project-nav");
    expect(within(projectNav).getByTestId("active-project-badge")).toHaveTextContent(
      "FerroLoop",
    );
    expect(within(projectNav).queryByTestId("session-cwd-badge")).toBeNull();
    expect(within(projectNav).getByRole("tab", { name: /^Artifacts$/i })).toBeInTheDocument();
    expect(within(projectNav).getByRole("tab", { name: /^Git & PRs$/i })).toBeInTheDocument();
    const recordsTabBtn = within(projectNav).getByRole("tab", {
      name: /^Records & Board$/i,
    });
    expect(recordsTabBtn).toBeInTheDocument();
    const splitToggleBtn = within(projectNav).getByTestId("split-view-toggle");
    expect(splitToggleBtn).toHaveAttribute("aria-pressed", "false");
    expect(recordsTabBtn.nextElementSibling).toBe(splitToggleBtn);

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

  it("updates active-context-tier-badge when switching between Agent, Project, and System tabs and opens AgentSettingsModal via header-agent-settings-btn", async () => {
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
    expect(within(screen.getByTestId("chat-pane")).queryByTestId("split-view-toggle")).toBeNull();
    expect(
      within(screen.getByTestId("tier-project-nav")).getByTestId("split-view-toggle"),
    ).toHaveAttribute("aria-pressed", "false");

    // Switch to Git & PRs (Project tier) -> opens right split panel while keeping ChatPane mounted
    const tabBar = screen.getByTestId("cockpit-tab-bar");
    const gitTabBtn = within(tabBar).getByRole("tab", { name: /^Git & PRs$/i });
    fireEvent.click(gitTabBtn);
    expect(screen.getByTestId("active-context-tier-badge")).toHaveAttribute(
      "data-tier",
      "project",
    );
    expect(screen.getByTestId("active-context-tier-badge")).toHaveTextContent(
      "Scope: Project · FerroLoop",
    );
    expect(screen.getByTestId("split-view-toggle")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("chat-pane")).toBeInTheDocument();

    // Clicking the same right-panel tab again closes the right split panel and returns to Chat
    fireEvent.click(gitTabBtn);
    expect(screen.getByTestId("split-view-toggle")).toHaveAttribute("aria-pressed", "false");
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
    expect(screen.getByTestId("split-view-toggle")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("chat-pane")).toBeInTheDocument();

    unmount();
  });
});
