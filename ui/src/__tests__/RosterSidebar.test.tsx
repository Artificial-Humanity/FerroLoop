import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import App from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import { ContextHeader } from "../components/ContextHeader";
import { PairingModal } from "../components/PairingModal";
import {
  COLLAPSED_PROJECTS_STORAGE_KEY,
  PINNED_AGENTS_STORAGE_KEY,
  PINNED_PROJECTS_STORAGE_KEY,
  RosterSidebar,
  classifyTextUnreadSeverity,
  classifyTurnUnreadSeverity,
  classifyWireUnreadSeverity,
  togglePinnedAgent,
  togglePinnedProject,
} from "../components/RosterSidebar";
import { computeUnreadSeverityByAgent } from "../App";
import {
  DEFAULT_WORKSPACE_AGENTS,
  type AgentLivenessItem,
  type AgentRecord,
  type DiscoveredSessionSummary,
} from "../types/api";

describe("RosterSidebar", () => {
  beforeEach(() => {
    window.localStorage.removeItem(COLLAPSED_PROJECTS_STORAGE_KEY);
    window.localStorage.removeItem(PINNED_PROJECTS_STORAGE_KEY);
    window.localStorage.removeItem(PINNED_AGENTS_STORAGE_KEY);
  });

  const sampleRoutes: DiscoveredSessionSummary[] = [
    {
      id: "urn:uuid:01965500-0000-7000-8000-000000000001",
      source: "route",
      adapter: "claude",
      persona: "jarvis",
      status: "active",
      cwd: "/home/lmcfarlin/Projects/Artificial-Humanity/AI-Lab-AMD",
    },
    {
      id: "urn:uuid:01965500-0000-7000-8000-000000000002",
      source: "route",
      adapter: "antigravity",
      persona: "ferris",
      status: "active",
      cwd: "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop",
    },
    {
      id: "urn:uuid:01965500-0000-7000-8000-000000000003",
      source: "route",
      adapter: "muse",
      persona: "kayla",
      status: "active",
      cwd: "/home/lmcfarlin/Projects/Artificial-Humanity/Kaggle-Gemma4",
    },
  ];

  const sampleLiveness: AgentLivenessItem[] = [
    {
      agent: "jarvis",
      project: "AI-Lab-AMD",
      role: "lab_manager",
      has_route: true,
      adapter: "claude",
      verdict: { status: "live", pid: 4100 },
    },
    {
      agent: "ferris",
      project: "FerroLoop",
      role: "lead",
      has_route: true,
      adapter: "antigravity",
      verdict: { status: "unverifiable", reason: "stale_status" },
    },
    {
      agent: "sonya",
      project: "Sonora",
      role: "lead",
      has_route: false,
      adapter: null,
      verdict: { status: "exited", outcome: "completed" },
    },
  ];

  it("falls back to DEFAULT_WORKSPACE_AGENTS when agents prop is empty and groups by project", () => {
    const onSelectAgent = vi.fn();
    render(
      <RosterSidebar
        agents={[]}
        routes={sampleRoutes}
        liveness={sampleLiveness}
        unreadByAgent={{ jarvis: 3, ferris: 0 }}
        selectedAgentName="jarvis"
        onSelectAgent={onSelectAgent}
      />,
    );

    // Default projects should be rendered
    expect(screen.getByText("AI-Lab-AMD")).toBeInTheDocument();
    expect(screen.getByText("FerroLoop")).toBeInTheDocument();
    expect(screen.getByText("Kaggle-Gemma4")).toBeInTheDocument();
    expect(screen.getByText("Sonora")).toBeInTheDocument();
    expect(screen.getByText("Website")).toBeInTheDocument();

    // Agents should be rendered
    expect(screen.getByText("JARVIS")).toBeInTheDocument();
    expect(screen.getByText("Ferris")).toBeInTheDocument();
    expect(screen.getByText("Kayla")).toBeInTheDocument();
    expect(screen.getByText("Wesley")).toBeInTheDocument();

    // Unread mail count badge for jarvis
    expect(screen.getByTestId("unread-badge-jarvis")).toHaveTextContent("3");
    expect(screen.queryByTestId("unread-badge-ferris")).toBeNull();

    // Adapter pills and liveness dots
    expect(screen.getByTestId("adapter-pill-jarvis")).toHaveTextContent(/claude/i);
    expect(screen.getByTestId("adapter-pill-ferris")).toHaveTextContent(/antigravity/i);
    expect(screen.getByTestId("liveness-dot-jarvis")).toHaveClass("fd-dot-alive");
    expect(screen.getByTestId("liveness-dot-ferris")).toHaveClass("fd-dot-stale");
    expect(screen.getByTestId("liveness-dot-sonya")).toHaveClass("fd-dot-dead");

    // Clicking an agent triggers onSelectAgent
    fireEvent.click(screen.getByTestId("agent-row-ferris"));
    expect(onSelectAgent).toHaveBeenCalledWith("ferris");
  });

  it("filters agents instantaneously by name, project, role, or adapter", () => {
    const customAgents: AgentRecord[] = [
      ...DEFAULT_WORKSPACE_AGENTS,
      {
        id: "urn:fl:agent:kacey",
        name_key: "kacey",
        display_name: "Kacey",
        project: "Kaggle-Gemma4",
        role: "assistant",
        status: "active",
      },
    ];

    render(
      <RosterSidebar
        agents={customAgents}
        routes={sampleRoutes}
        liveness={sampleLiveness}
        unreadByAgent={{}}
        selectedAgentName="jarvis"
        onSelectAgent={() => {}}
      />,
    );

    const searchInput = screen.getByPlaceholderText(/search agents/i);

    // Filter by adapter "muse" -> matches Kayla
    fireEvent.change(searchInput, { target: { value: "muse" } });
    expect(screen.getByText("Kayla")).toBeInTheDocument();
    expect(screen.queryByText("JARVIS")).toBeNull();

    // Filter by role "assistant" -> matches Kacey
    fireEvent.change(searchInput, { target: { value: "assistant" } });
    expect(screen.getByText("Kacey")).toBeInTheDocument();
    expect(screen.queryByText("Ferris")).toBeNull();

    // Filter by project "Website" -> matches Wesley
    fireEvent.change(searchInput, { target: { value: "Website" } });
    expect(screen.getByText("Wesley")).toBeInTheDocument();
    expect(screen.queryByText("Kayla")).toBeNull();
  });

  it("supports collapsible project groups persisted in localStorage, project unread badge sum, and project settings cog", () => {
    const onOpenProjectSettings = vi.fn();
    const customAgents: AgentRecord[] = [
      ...DEFAULT_WORKSPACE_AGENTS,
      {
        id: "urn:fl:agent:felix",
        name_key: "felix",
        display_name: "Felix",
        project: "FerroLoop",
        role: "ui-developer",
        status: "active",
      },
    ];

    render(
      <RosterSidebar
        agents={customAgents}
        routes={sampleRoutes}
        liveness={sampleLiveness}
        unreadByAgent={{ ferris: 2, felix: 4, jarvis: 1 }}
        selectedAgentName="ferris"
        onSelectAgent={() => {}}
        onOpenProjectSettings={onOpenProjectSettings}
      />,
    );

    // Project unread badge sums ferris (2) + felix (4) = 6
    const ferroUnreadBadge = screen.getByTestId("project-unread-badge-ferroloop");
    expect(ferroUnreadBadge).toHaveTextContent("6");
    expect(screen.getByTestId("project-unread-badge-ai-lab-amd")).toHaveTextContent("1");

    // Clicking project settings cog calls onOpenProjectSettings without collapsing group
    const ferroCogBtn = screen.getByTestId("project-settings-btn-ferroloop");
    fireEvent.click(ferroCogBtn);
    expect(onOpenProjectSettings).toHaveBeenCalledWith("FerroLoop");
    expect(screen.getByTestId("agent-row-ferris")).toBeInTheDocument();

    // Clicking project group toggle collapses FerroLoop agents while keeping unread badge visible
    const ferroToggle = screen.getByTestId("project-group-toggle-ferroloop");
    expect(ferroToggle).toHaveAttribute("aria-expanded", "true");
    fireEvent.click(ferroToggle);
    expect(ferroToggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByTestId("agent-row-ferris")).toBeNull();
    expect(screen.getByTestId("project-unread-badge-ferroloop")).toHaveTextContent("6");

    // Persisted in localStorage
    expect(
      JSON.parse(window.localStorage.getItem(COLLAPSED_PROJECTS_STORAGE_KEY) ?? "[]"),
    ).toContain("ferroloop");
    window.localStorage.removeItem(COLLAPSED_PROJECTS_STORAGE_KEY);
  });

  it("renders roster-global-controls (Settings, ?, icon-only theme toggle) and sorts pinned projects and pinned agents alphabetically to top", () => {
    const onOpenSettingsModal = vi.fn();
    const onOpenHelpModal = vi.fn();
    const onToggleTheme = vi.fn();
    const customAgents: AgentRecord[] = [
      ...DEFAULT_WORKSPACE_AGENTS,
      {
        id: "urn:fl:agent:felix",
        name_key: "felix",
        display_name: "Felix",
        project: "FerroLoop",
        role: "ui-developer",
        status: "active",
      },
      {
        id: "urn:fl:agent:felicity",
        name_key: "felicity",
        display_name: "Felicity",
        project: "FerroLoop",
        role: "assistant",
        status: "active",
      },
    ];

    render(
      <RosterSidebar
        agents={customAgents}
        routes={sampleRoutes}
        liveness={sampleLiveness}
        unreadByAgent={{}}
        selectedAgentName="ferris"
        onSelectAgent={() => {}}
        onOpenSettingsModal={onOpenSettingsModal}
        onOpenHelpModal={onOpenHelpModal}
        themeMode="dark"
        onToggleTheme={onToggleTheme}
      />,
    );

    const globalControls = screen.getByTestId("roster-global-controls");
    expect(globalControls).toBeInTheDocument();

    fireEvent.click(screen.getByTestId("settings-modal-btn"));
    expect(onOpenSettingsModal).toHaveBeenCalledTimes(1);

    fireEvent.click(screen.getByTestId("context-help-btn"));
    expect(onOpenHelpModal).toHaveBeenCalledTimes(1);

    const themeBtn = screen.getByTestId("theme-toggle-btn");
    expect(themeBtn).toHaveTextContent("☀");
    expect(themeBtn).not.toHaveTextContent(/Light/i);
    fireEvent.click(themeBtn);
    expect(onToggleTheme).toHaveBeenCalledTimes(1);

    // Pin Website and Sonora -> both pinned projects move to top in alphabetical order (Sonora, Website)
    act(() => {
      togglePinnedProject("Website");
      togglePinnedProject("Sonora");
    });
    expect(screen.getByTestId("project-pinned-indicator-sonora")).toBeInTheDocument();
    expect(screen.getByTestId("project-pinned-indicator-website")).toBeInTheDocument();
    const projectGroups = screen.getAllByTestId(/^project-group-(?!toggle)/);
    expect(projectGroups[0]).toHaveAttribute("data-testid", "project-group-sonora");
    expect(projectGroups[0]).toHaveAttribute("data-pinned", "true");
    expect(projectGroups[1]).toHaveAttribute("data-testid", "project-group-website");
    expect(projectGroups[1]).toHaveAttribute("data-pinned", "true");
    expect(projectGroups[2]).toHaveAttribute("data-testid", "project-group-ai-lab-amd");

    // Pin Felix and Ferris inside FerroLoop -> pinned agents sort alphabetically (Felix, Ferris) above unpinned Felicia and Felicity
    act(() => {
      togglePinnedAgent("ferris");
      togglePinnedAgent("felix");
    });
    expect(screen.getByTestId("agent-pinned-indicator-felix")).toBeInTheDocument();
    expect(screen.getByTestId("agent-pinned-indicator-ferris")).toBeInTheDocument();
    const ferroGroup = screen.getByTestId("project-group-ferroloop");
    const ferroAgentRows = Array.from(
      ferroGroup.querySelectorAll('[data-testid^="agent-row-"]'),
    );
    expect(ferroAgentRows.map((el) => el.getAttribute("data-testid"))).toEqual([
      "agent-row-felix",
      "agent-row-ferris",
      "agent-row-felicia",
      "agent-row-felicity",
    ]);
  });

  it("omits muted project agent count, renders borderless ⚙ buttons, and formats unread badges as <N>, <N>?, and <N>! with ! > ? > normal project rollup", () => {
    const customAgents: AgentRecord[] = [
      ...DEFAULT_WORKSPACE_AGENTS,
      {
        id: "urn:fl:agent:felix",
        name_key: "felix",
        display_name: "Felix",
        project: "FerroLoop",
        role: "ui-developer",
        status: "active",
      },
      {
        id: "urn:fl:agent:felicity",
        name_key: "felicity",
        display_name: "Felicity",
        project: "FerroLoop",
        role: "assistant",
        status: "active",
      },
    ];

    const { rerender } = render(
      <RosterSidebar
        agents={customAgents}
        routes={sampleRoutes}
        liveness={sampleLiveness}
        unreadByAgent={{ ferris: 2, felix: 1, jarvis: 4 }}
        unreadSeverityByAgent={{
          ferris: "normal",
          felix: "question",
          jarvis: "normal",
        }}
        selectedAgentName="jarvis"
        onSelectAgent={() => {}}
      />,
    );

    // Project group header does not show a muted total agent count next to the project name
    const sonoraToggle = screen.getByTestId("project-group-toggle-sonora");
    expect(sonoraToggle).toHaveTextContent("Sonora");
    expect(sonoraToggle.textContent?.replace(/[▸▾]/g, "").trim()).toBe("Sonora");

    // Borderless ⚙ cog buttons on project and agent rows
    const projCog = screen.getByTestId("project-settings-btn-ferroloop");
    const agentCog = screen.getByTestId("agent-settings-btn-ferris");
    expect(projCog).toHaveClass("fd-gear-btn");
    expect(projCog).not.toHaveClass("fd-btn");
    expect(projCog.style.background).toBe("transparent");
    expect(agentCog).toHaveClass("fd-gear-btn");
    expect(agentCog).not.toHaveClass("fd-btn");
    expect(agentCog.style.background).toBe("transparent");

    // Agent unread badges: jarvis -> "4" (normal), ferris -> "2" (normal), felix -> "1?" (question)
    expect(screen.getByTestId("unread-badge-jarvis")).toHaveTextContent("4");
    expect(screen.getByTestId("unread-badge-jarvis")).toHaveAttribute(
      "data-severity",
      "normal",
    );
    expect(screen.getByTestId("unread-badge-ferris")).toHaveTextContent("2");
    expect(screen.getByTestId("unread-badge-felix")).toHaveTextContent("1?");
    expect(screen.getByTestId("unread-badge-felix")).toHaveAttribute(
      "data-severity",
      "question",
    );

    // Project unread badge for FerroLoop rolls up ferris (normal, 2) + felix (question, 1) -> "3?"
    const ferroBadge = screen.getByTestId("project-unread-badge-ferroloop");
    expect(ferroBadge).toHaveTextContent("3?");
    expect(ferroBadge).toHaveAttribute("data-severity", "question");

    // When felicity has an important/blocker unread message (2!), FerroLoop project badge upgrades to "5!"
    rerender(
      <RosterSidebar
        agents={customAgents}
        routes={sampleRoutes}
        liveness={sampleLiveness}
        unreadByAgent={{ ferris: 2, felix: 1, felicity: 2, jarvis: 4 }}
        unreadSeverityByAgent={{
          ferris: "normal",
          felix: "question",
          felicity: "important",
          jarvis: "normal",
        }}
        selectedAgentName="jarvis"
        onSelectAgent={() => {}}
      />,
    );

    expect(screen.getByTestId("unread-badge-felicity")).toHaveTextContent("2!");
    expect(screen.getByTestId("unread-badge-felicity")).toHaveAttribute(
      "data-severity",
      "important",
    );
    expect(screen.getByTestId("project-unread-badge-ferroloop")).toHaveTextContent("5!");
    expect(screen.getByTestId("project-unread-badge-ferroloop")).toHaveAttribute(
      "data-severity",
      "important",
    );
  });

  it("classifies text, transcript turns, and wire messages into normal, question, and important severities", () => {
    expect(classifyTextUnreadSeverity("Completed build and 280 tests passing with zero errors.")).toBe("normal");
    expect(classifyTextUnreadSeverity("Should we target main or uat for this PR?")).toBe("question");
    expect(classifyTextUnreadSeverity("BLOCKER: Database migration failed with exit code 1.")).toBe("important");

    expect(
      classifyTurnUnreadSeverity({
        index: 1,
        role: "assistant",
        content: "Please choose an option.",
        tool_calls: [{ name: "ask_question", arguments: "{}" }],
      }),
    ).toBe("question");

    expect(
      classifyWireUnreadSeverity({
        id: "msg-1",
        sender_name: "ferris",
        to: "felix",
        subject: "Quick check",
        body: "Can you verify the contrast ratio?",
        urgency: "normal",
      }),
    ).toBe("question");

    expect(
      classifyWireUnreadSeverity({
        id: "msg-2",
        sender_name: "jarvis",
        to: "ferris",
        subject: "GPU alert",
        body: "Node offline",
        urgency: "urgent",
      }),
    ).toBe("important");

    const severities = computeUnreadSeverityByAgent(
      {
        ferris: [
          { index: 0, role: "assistant", content: "Initial turn", tool_calls: [] },
          { index: 1, role: "assistant", content: "Do you approve this layout?", tool_calls: [] },
        ],
        felix: [
          { index: 0, role: "assistant", content: "Initial turn", tool_calls: [] },
          { index: 1, role: "assistant", content: "FATAL: build failed on missing import.", tool_calls: [] },
        ],
      },
      { ferris: 0, felix: 0 },
      [],
    );
    expect(severities).toEqual({
      ferris: "question",
      felix: "important",
    });
  });
});

describe("ContextHeader", () => {
  const ferris = DEFAULT_WORKSPACE_AGENTS.find((a) => a.name_key === "ferris")!;
  const sessions: DiscoveredSessionSummary[] = [
    {
      id: "sess-1",
      source: "route",
      adapter: "antigravity",
      persona: "ferris",
      status: "active",
      cwd: "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop",
    },
    {
      id: "sess-2",
      source: "attempt",
      adapter: "codex",
      persona: "ferris",
      status: "completed",
      cwd: "/home/lmcfarlin/worktrees/fl-ferrodeck-ui",
    },
  ];

  it("renders agent context, mode tabs, session selector, authority pill, and inline mobile roster toggle", () => {
    const onSelectTab = vi.fn();
    const onSelectSession = vi.fn();
    const onOpenPairingModal = vi.fn();
    const onOpenMobileRoster = vi.fn();

    render(
      <ContextHeader
        selectedAgent={ferris}
        sessions={sessions}
        selectedSessionId="sess-1"
        onSelectSession={onSelectSession}
        activeTab="chat"
        onSelectTab={onSelectTab}
        authority={{ tier: "owner", login: "lmcfarlin" }}
        wsConnected={true}
        onOpenPairingModal={onOpenPairingModal}
        onOpenMobileRoster={onOpenMobileRoster}
      />,
    );

    expect(screen.getByText("Ferris")).toBeInTheDocument();
    expect(screen.getByText("FerroLoop")).toBeInTheDocument();
    expect(screen.getByText(/Owner:\s*lmcfarlin/i)).toBeInTheDocument();

    // Switch tab to Git & PRs
    fireEvent.click(screen.getByRole("tab", { name: /Git & PRs/i }));
    expect(onSelectTab).toHaveBeenCalledWith("git");

    // Inline mobile roster toggle
    fireEvent.click(screen.getByTestId("mobile-sidebar-toggle-btn"));
    expect(onOpenMobileRoster).toHaveBeenCalledTimes(1);

    // Select second session
    fireEvent.change(screen.getByTestId("session-selector"), {
      target: { value: "sess-2" },
    });
    expect(onSelectSession).toHaveBeenCalledWith("sess-2");
  });
});

describe("PairingModal", () => {
  it("submits pairing code or bearer token and displays error state cleanly", async () => {
    const onPairWithCode = vi.fn(async () => {});
    const onPairWithToken = vi.fn(async () => {});
    const onClose = vi.fn();

    render(
      <PairingModal
        isOpen={true}
        onClose={onClose}
        onPairWithCode={onPairWithCode}
        onPairWithToken={onPairWithToken}
        error="Invalid or expired pairing code"
      />,
    );

    expect(screen.getByText("Invalid or expired pairing code")).toBeInTheDocument();

    const codeInput = screen.getByPlaceholderText(/pairing code/i);
    fireEvent.change(codeInput, { target: { value: "FL-998877" } });
    fireEvent.click(screen.getByRole("button", { name: /pair with code/i }));

    await waitFor(() => {
      expect(onPairWithCode).toHaveBeenCalledWith("FL-998877");
    });

    const tokenInput = screen.getByPlaceholderText(/bearer token/i);
    fireEvent.change(tokenInput, { target: { value: "owner-secret-token" } });
    fireEvent.click(screen.getByRole("button", { name: /save token/i }));

    await waitFor(() => {
      expect(onPairWithToken).toHaveBeenCalledWith("owner-secret-token");
    });
  });
});

describe("ContextHeader session pill and header controls", () => {
  const ferrisAgent = DEFAULT_WORKSPACE_AGENTS.find((a) => a.name_key === "ferris")!;
  const oneSession: DiscoveredSessionSummary[] = [
    {
      id: "sess-only-1234abcd",
      source: "route",
      adapter: "claude",
      persona: "ferris",
      status: "active",
    },
  ];

  it("shows a session id pill instead of the selector for a single session", () => {
    render(
      <ContextHeader
        selectedAgent={ferrisAgent}
        sessions={oneSession}
        selectedSessionId="sess-only-1234abcd"
        onSelectSession={() => {}}
        activeTab="chat"
        onSelectTab={() => {}}
        onOpenPairingModal={() => {}}
      />,
    );

    expect(screen.queryByTestId("session-selector")).toBeNull();
    expect(screen.getByTestId("session-id-pill")).toHaveTextContent("#1234abcd");
    expect(screen.queryByTestId("system-utility-bar")).toBeNull();
  });

  it("keeps the selector when the agent has multiple sessions", () => {
    render(
      <ContextHeader
        selectedAgent={ferrisAgent}
        sessions={[
          ...oneSession,
          {
            id: "sess-second-5678efgh",
            source: "attempt",
            adapter: "codex",
            persona: "ferris",
            status: "completed",
          },
        ]}
        selectedSessionId="sess-only-1234abcd"
        onSelectSession={() => {}}
        activeTab="chat"
        onSelectTab={() => {}}
        onOpenPairingModal={() => {}}
      />,
    );

    expect(screen.getByTestId("session-selector")).toBeInTheDocument();
    expect(screen.queryByTestId("session-id-pill")).toBeNull();
  });
});

describe("Mobile responsive sidebar drawer (Record 55)", () => {
  it("toggles mobile-sidebar-toggle-btn, renders mobile-sidebar-backdrop, dismisses on backdrop click, and auto-closes on agent select", async () => {
    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/api/v1/auth/session")) {
        return new Response(
          JSON.stringify({
            authenticated: true,
            authority: { tier: "owner", login: "lmcfarlin" },
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      return new Response(JSON.stringify({}), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
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

    const sidebar = await screen.findByTestId("roster-sidebar");
    const toggleBtn = await screen.findByTestId("mobile-sidebar-toggle-btn");

    // Initially closed on mobile
    expect(sidebar).toHaveAttribute("data-mobile-open", "false");
    expect(screen.queryByTestId("mobile-sidebar-backdrop")).toBeNull();

    // Clicking mobile-sidebar-toggle-btn opens the drawer and renders backdrop
    fireEvent.click(toggleBtn);
    expect(sidebar).toHaveAttribute("data-mobile-open", "true");
    expect(screen.getByTestId("mobile-sidebar-backdrop")).toBeInTheDocument();

    // Clicking mobile-sidebar-backdrop closes the drawer
    fireEvent.click(screen.getByTestId("mobile-sidebar-backdrop"));
    expect(sidebar).toHaveAttribute("data-mobile-open", "false");
    expect(screen.queryByTestId("mobile-sidebar-backdrop")).toBeNull();

    // Re-open drawer and select an agent -> auto-closes drawer
    fireEvent.click(toggleBtn);
    expect(sidebar).toHaveAttribute("data-mobile-open", "true");
    fireEvent.click(screen.getByTestId("agent-row-jarvis"));
    expect(sidebar).toHaveAttribute("data-mobile-open", "false");
    expect(screen.queryByTestId("mobile-sidebar-backdrop")).toBeNull();

    // Clicking per-agent settings cog on roster row selects agent and opens agent-settings-modal
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    expect(screen.getByTestId("agent-settings-modal")).toBeInTheDocument();

    unmount();
  });
});
