import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import App from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import { ContextHeader, RightPanelHeader } from "../components/ContextHeader";
import { AgentSettingsModal } from "../components/AgentSettingsModal";
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
import { computeUnreadSeverityByAgent, ROSTER_POLL_INTERVAL_MS } from "../App";
import { TEST_WORKSPACE_AGENTS as DEFAULT_WORKSPACE_AGENTS } from "./fixtures/testAgents";
import {
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

  it("embedded mode renders roster content only, in flow, without drawer chrome", () => {
    const onClose = vi.fn();
    const onSelectAgent = vi.fn();
    const { rerender } = render(
      <RosterSidebar
        agents={[]}
        routes={sampleRoutes}
        liveness={sampleLiveness}
        unreadByAgent={{}}
        selectedAgentName="ferris"
        onSelectAgent={onSelectAgent}
        isOpen={true}
        onClose={onClose}
        width={420}
        onResizeStart={vi.fn()}
        leftPanelTab="terminal"
        consoleContent={<div data-testid="console-body-stub" />}
        embedded
      />,
    );

    const sidebar = screen.getByTestId("roster-sidebar");
    expect(sidebar).toHaveAttribute("data-embedded", "true");
    expect(sidebar).toHaveClass("fd-sidebar-embedded");
    expect(sidebar).toHaveAttribute("data-left-panel-tab", "roster");
    expect(sidebar.style.width).toBe("");
    expect(screen.queryByTestId("mobile-sidebar-backdrop")).toBeNull();
    expect(screen.queryByLabelText("Close Roster")).toBeNull();
    expect(screen.queryByTestId("left-panel-resize-handle")).toBeNull();
    expect(screen.queryByTestId("left-panel-header")).toBeNull();
    expect(screen.queryByTestId("roster-triage-toggle-btn")).toBeNull();
    expect(screen.queryByTestId("console-body-stub")).toBeNull();
    fireEvent.click(screen.getByTestId("agent-row-ferris"));
    expect(onSelectAgent).toHaveBeenCalledWith("ferris");

    // Without the prop the drawer chrome is back and nothing embedded leaks through.
    rerender(
      <RosterSidebar
        agents={[]}
        routes={sampleRoutes}
        liveness={sampleLiveness}
        unreadByAgent={{}}
        selectedAgentName="ferris"
        onSelectAgent={onSelectAgent}
        isOpen={true}
        onClose={onClose}
        onResizeStart={vi.fn()}
      />,
    );
    expect(screen.getByTestId("roster-sidebar")).not.toHaveAttribute("data-embedded");
    expect(screen.getByTestId("roster-sidebar")).not.toHaveClass("fd-sidebar-embedded");
    expect(screen.getByTestId("mobile-sidebar-backdrop")).toBeInTheDocument();
    expect(screen.getByLabelText("Close Roster")).toBeInTheDocument();
    expect(screen.getByTestId("left-panel-resize-handle")).toBeInTheDocument();
    expect(screen.getByTestId("left-panel-header")).toBeInTheDocument();
    expect(screen.getByTestId("roster-triage-toggle-btn")).toBeInTheDocument();
  });

  it("never falls back to a static roster when agents, liveness, sessions, and routes are empty, and synthesizes agents from routes/liveness/sessions when present", () => {
    const onSelectAgent = vi.fn();
    const { unmount } = render(
      <RosterSidebar
        agents={[]}
        routes={[]}
        sessions={[]}
        liveness={[]}
        unreadByAgent={{}}
        selectedAgentName={null}
        onSelectAgent={onSelectAgent}
        signedOut={true}
      />,
    );

    expect(screen.getByTestId("roster-empty-state")).toHaveTextContent(
      /sign in to view the workspace agent roster/i,
    );
    expect(screen.queryByText("JARVIS")).toBeNull();
    expect(screen.queryByText("Ferris")).toBeNull();
    expect(screen.queryByText("Felicia")).toBeNull();
    expect(screen.queryByText("Wesley")).toBeNull();
    unmount();

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

    // Projects from liveness and routes should be rendered, without unseeded static projects
    expect(screen.getByText("AI-Lab-AMD")).toBeInTheDocument();
    expect(screen.getByText("FerroLoop")).toBeInTheDocument();
    expect(screen.getByText("Kaggle-Gemma4")).toBeInTheDocument();
    expect(screen.getByText("Sonora")).toBeInTheDocument();
    expect(screen.queryByText("Website")).toBeNull();

    // Agents from liveness and routes should be rendered
    expect(screen.getByText("JARVIS")).toBeInTheDocument();
    expect(screen.getByText("Ferris")).toBeInTheDocument();
    expect(screen.getByText("Kayla")).toBeInTheDocument();
    expect(screen.queryByText("Wesley")).toBeNull();

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

  it("counts a live session in sessionMap as online (hasRoute) and includes session-only personas (e.g. Kacey) in enrichedAgents", () => {
    const onSelectAgent = vi.fn();
    render(
      <RosterSidebar
        agents={[]}
        routes={[]}
        sessions={[
          {
            id: "sess-kacey-live",
            source: "route",
            adapter: "antigravity",
            persona: "kacey",
            status: "active",
            cwd: "/home/lmcfarlin/Projects/Artificial-Humanity/Kaggle-Gemma4",
          },
        ]}
        liveness={[]}
        unreadByAgent={{}}
        selectedAgentName={null}
        onSelectAgent={onSelectAgent}
      />,
    );

    expect(screen.getByTestId("agent-row-kacey")).toBeInTheDocument();
    expect(screen.getByText("Kacey")).toBeInTheDocument();
    expect(screen.getByTestId("liveness-dot-kacey")).toHaveClass("fd-dot-alive");
    expect(screen.getByTestId("activity-tag-kacey")).toHaveAttribute(
      "data-activity",
      "idle",
    );
    expect(screen.getByTestId("adapter-pill-kacey")).toHaveTextContent(/antigravity/i);
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

    const settingsModalBtn = screen.getByTestId("settings-modal-btn");
    const gearIcon = settingsModalBtn.querySelector(".fd-gear-icon") as HTMLElement | null;
    expect(gearIcon).not.toBeNull();
    expect(gearIcon?.style.fontSize).toBe("20px");
    fireEvent.click(settingsModalBtn);
    expect(onOpenSettingsModal).toHaveBeenCalledTimes(1);

    fireEvent.click(screen.getByTestId("context-help-btn"));
    expect(onOpenHelpModal).toHaveBeenCalledTimes(1);

    expect(screen.queryByTestId("theme-toggle-btn")).toBeNull();

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

    // Borderless settings buttons on project and agent rows aligned on a 24x24 right rail,
    // with a distinct 17px hexagonal SVG in var(--accent-primary) at the project level vs. 14px ⚙ at the agent level
    const projHeader = screen.getByTestId("project-header-row-ferroloop");
    const agentRow = screen.getByTestId("agent-row-ferris");
    expect(projHeader.style.padding).toBe("4px 10px");
    expect(agentRow.style.padding).toBe("8px 10px");

    const projCog = screen.getByTestId("project-settings-btn-ferroloop");
    const agentCog = screen.getByTestId("agent-settings-btn-ferris");
    expect(projCog).toHaveClass("fd-gear-btn", "fd-roster-project-gear-btn");
    expect(projCog).not.toHaveClass("fd-btn");
    expect(projCog.style.background).toBe("transparent");
    expect(projCog.style.width).toBe("24px");
    expect(projCog.style.height).toBe("24px");
    expect(projCog.style.color).toBe("var(--accent-primary)");
    const projSvg = projCog.querySelector('[data-testid="project-settings-icon"]');
    expect(projSvg).not.toBeNull();
    expect(projSvg?.getAttribute("width")).toBe("17");
    expect(projSvg?.querySelector("polygon")).not.toBeNull();

    expect(agentCog).toHaveClass("fd-gear-btn", "fd-roster-gear-btn");
    expect(agentCog).not.toHaveClass("fd-btn");
    expect(agentCog.style.background).toBe("transparent");
    // Touch-sized hit area (36px) with a 20px glyph; negative margin keeps the row height.
    expect(agentCog.style.width).toBe("36px");
    expect(agentCog.style.height).toBe("36px");
    expect(agentCog.style.fontSize).toBe("20px");
    expect(agentCog.style.color).toBe("var(--text-muted)");
    expect(agentCog).toHaveTextContent("⚙");

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
    expect(classifyTextUnreadSeverity("Fixed the panic and error handling when a request failed or was blocked by CORS.")).toBe("normal");
    expect(classifyTextUnreadSeverity("Should we target main or uat for this PR?")).toBe("question");
    expect(classifyTextUnreadSeverity("BLOCKER: Database migration failed with exit code 1.")).toBe("important");
    expect(classifyTextUnreadSeverity("We cannot proceed without credentials and need owner approval.")).toBe("important");

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
          { index: 1, role: "assistant", content: "BLOCKER: cannot proceed without owner approval.", tool_calls: [] },
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

  it("renders circular context-used ring next to activity tag inside agent-status-group and does not invoke onClose when selecting an agent or clicking agent settings", () => {
    const onSelectAgent = vi.fn();
    const onOpenAgentSettings = vi.fn();
    const onClose = vi.fn();

    render(
      <RosterSidebar
        agents={DEFAULT_WORKSPACE_AGENTS}
        routes={sampleRoutes}
        liveness={sampleLiveness}
        unreadByAgent={{}}
        contextUsageByAgent={{
          ferris: {
            used_tokens: 420_000,
            max_tokens: 1_000_000,
          },
          jarvis: {
            used_tokens: 185_000,
            max_tokens: 200_000,
          },
        }}
        selectedAgentName="ferris"
        onSelectAgent={onSelectAgent}
        onOpenAgentSettings={onOpenAgentSettings}
        onClose={onClose}
      />,
    );

    const ferrisStatusGroup = screen.getByTestId("agent-status-group-ferris");
    const ferrisIdentityGroup = screen.getByTestId("agent-identity-group-ferris");
    const ferrisActivityTag = screen.getByTestId("activity-tag-ferris");
    const ferrisRing = screen.getByTestId("context-ring-ferris");

    expect(ferrisStatusGroup).toContainElement(ferrisActivityTag);
    expect(ferrisStatusGroup).toContainElement(ferrisRing);
    expect(ferrisIdentityGroup).toContainElement(screen.getByTestId("role-badge-ferris"));
    expect(ferrisIdentityGroup).toContainElement(screen.getByTestId("adapter-pill-ferris"));

    expect(ferrisRing).toHaveAttribute("role", "img");
    expect(ferrisRing).toHaveAttribute("data-percent", "42");
    expect(ferrisRing).toHaveAttribute(
      "title",
      "Context used: 42% (420k / 1m tokens)",
    );
    expect(ferrisRing).toHaveAttribute(
      "aria-label",
      "Context used: 42% (420k / 1m tokens)",
    );
    expect(ferrisRing).toHaveTextContent("42%");
    expect(ferrisRing.querySelectorAll("circle")).toHaveLength(2);

    const jarvisRing = screen.getByTestId("context-ring-jarvis");
    expect(jarvisRing).toHaveAttribute("data-percent", "93");
    expect(jarvisRing).toHaveTextContent("93%");

    // Clicking an agent row or agent settings button must NOT call onClose (which collapses desktop sidebar)
    fireEvent.click(screen.getByTestId("agent-row-jarvis"));
    expect(onSelectAgent).toHaveBeenCalledWith("jarvis");
    expect(onClose).not.toHaveBeenCalled();

    fireEvent.keyDown(screen.getByTestId("agent-row-jarvis"), { key: "Enter" });
    expect(onClose).not.toHaveBeenCalled();

    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    expect(onOpenAgentSettings).toHaveBeenCalledWith("ferris");
    expect(onClose).not.toHaveBeenCalled();
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
      <>
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
        />
        <RightPanelHeader
          selectedAgent={ferris}
          rightSplitTab="artifacts"
          onSelectTab={onSelectTab}
        />
      </>,
    );

    expect(screen.getByText("Ferris")).toBeInTheDocument();
    expect(screen.getAllByText("FerroLoop").length).toBeGreaterThan(0);
    expect(screen.getByText(/Owner:\s*lmcfarlin/i)).toBeInTheDocument();

    // Switch tab to Git & PRs via RightPanelHeader
    fireEvent.click(screen.getByTestId("right-panel-tab-git"));
    expect(onSelectTab).toHaveBeenCalledWith("git");

    // Inline mobile roster toggle inside ChatPane toolbar
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

  it("omits the session id pill from ContextHeader for a single session and renders it inside AgentSettingsModal", () => {
    render(
      <>
        <ContextHeader
          selectedAgent={ferrisAgent}
          sessions={oneSession}
          selectedSessionId="sess-only-1234abcd"
          onSelectSession={() => {}}
          activeTab="chat"
          onSelectTab={() => {}}
          onOpenPairingModal={() => {}}
        />
        <AgentSettingsModal
          isOpen={true}
          onClose={() => {}}
          agentName="ferris"
          agentDisplayName="Ferris"
          sessionId="sess-only-1234abcd"
          adapter="claude"
          selectedModel="claude-opus-5-5"
          selectedThinkingLevel="high"
          onChangeModelAndThinking={() => {}}
          contextEstimate={{
            usedTokens: 1000,
            maxTokens: 200000,
            percent: 1,
          }}
          canCompact={false}
          compacting={false}
          onCompactContext={() => {}}
          canClear={true}
          clearing={false}
          onOpenClearModal={() => {}}
          canDecommission={true}
          decommissioning={false}
          onOpenDecommissionModal={() => {}}
        />
      </>,
    );

    expect(screen.queryByTestId("session-selector")).toBeNull();
    expect(screen.getByTestId("session-id-pill")).toHaveTextContent("#1234abcd");
    expect(screen.getByTestId("agent-settings-identity")).toContainElement(
      screen.getByTestId("session-id-pill"),
    );
    expect(screen.getByTestId("recast-agent-btn-ferris")).toHaveAttribute(
      "title",
      "Reassign agent project or role in the registry without clearing the live session",
    );
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
      if (url.includes("/api/v1/agents") && !url.includes("/liveness")) {
        return new Response(
          JSON.stringify({ agents: DEFAULT_WORKSPACE_AGENTS, routes: [] }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      if (url.includes("/api/v1/sessions") && !url.includes("/transcript")) {
        return new Response(
          JSON.stringify({
            sessions: [
              {
                id: "sess-ferris-yolo",
                source: "route",
                adapter: "claude",
                persona: "ferris",
                status: "active",
                execution_mode: "yolo",
              },
            ],
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

    // Re-open drawer and select an agent -> auto-closes mobile drawer without collapsing desktop sidebar
    fireEvent.click(toggleBtn);
    expect(sidebar).toHaveAttribute("data-mobile-open", "true");
    // Ensure desktop sidebar is expanded before selecting an agent
    if (sidebar.getAttribute("data-desktop-collapsed") === "true") {
      fireEvent.click(toggleBtn);
    }
    expect(sidebar).toHaveAttribute("data-desktop-collapsed", "false");
    fireEvent.click(screen.getByTestId("agent-row-jarvis"));
    expect(sidebar).toHaveAttribute("data-mobile-open", "false");
    expect(sidebar).toHaveAttribute("data-desktop-collapsed", "false");
    expect(screen.queryByTestId("mobile-sidebar-backdrop")).toBeNull();

    // Clicking per-agent settings cog on roster row selects agent and opens agent-settings-modal without collapsing desktop sidebar,
    // and passes activeSession?.execution_mode immediately into AgentSettingsModal
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    expect(screen.getByTestId("agent-settings-modal")).toBeInTheDocument();
    expect(screen.getByTestId("execution-mode-badge")).toHaveAttribute(
      "data-mode",
      "yolo",
    );
    expect(screen.getByTestId("execution-mode-badge")).toHaveTextContent("Yolo");
    expect(sidebar).toHaveAttribute("data-desktop-collapsed", "false");
    fireEvent.click(screen.getByTestId("close-agent-settings-btn"));

    // Below the 1080px three-panel threshold (tablet/small desktop, here 900px), toggling the left
    // sidebar closes the right split panel and vice versa. Phone widths (< 768px) no longer use
    // this drawer; they render the phone shell (see PhoneShell.test.tsx).
    const prevInnerWidth = window.innerWidth;
    Object.defineProperty(window, "innerWidth", {
      writable: true,
      configurable: true,
      value: 900,
    });
    const splitToggleBtn = screen.getByTestId("split-view-toggle");
    fireEvent.click(toggleBtn);
    expect(sidebar).toHaveAttribute("data-mobile-open", "true");
    fireEvent.click(splitToggleBtn);
    expect(sidebar).toHaveAttribute("data-mobile-open", "false");
    expect(splitToggleBtn).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(toggleBtn);
    expect(sidebar).toHaveAttribute("data-mobile-open", "true");
    expect(splitToggleBtn).toHaveAttribute("aria-pressed", "false");
    Object.defineProperty(window, "innerWidth", {
      writable: true,
      configurable: true,
      value: prevInnerWidth,
    });

    unmount();
  });

  it("shows no static roster while unauthenticated (401 bootstrap) and loads the full roster including the-council and session-only Kacey after login", async () => {
    let authenticated = false;
    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? "GET";
      if (url.includes("/api/v1/auth/pair") && method === "POST") {
        authenticated = true;
        return new Response(
          JSON.stringify({
            authenticated: true,
            authority: { tier: "owner", login: "lmcfarlin" },
            session_token: "tok-after-login",
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      if (url.includes("/api/v1/auth/session")) {
        if (!authenticated) {
          return new Response(
            JSON.stringify({ authenticated: false, authority: null }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          );
        }
        return new Response(
          JSON.stringify({
            authenticated: true,
            authority: { tier: "owner", login: "lmcfarlin" },
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      if (!authenticated) {
        return new Response(JSON.stringify({ error: "unauthorized" }), {
          status: 401,
          headers: { "Content-Type": "application/json" },
        });
      }
      if (url.endsWith("/api/v1/projects")) {
        return new Response(
          JSON.stringify({
            projects: [
              {
                id: "ferroloop",
                slug: "ferroloop",
                name: "FerroLoop",
                kind: "project",
                path: "/work/ferroloop",
                has_git: true,
                purpose: null,
                tracker: null,
                store_path: null,
                active_agent_count: 2,
              },
              {
                id: "the-council",
                slug: "the-council",
                name: "The Council",
                kind: "virtual",
                path: null,
                has_git: false,
                purpose: "Deliberative council",
                tracker: null,
                store_path: null,
                active_agent_count: 5,
              },
              {
                id: "kaggle-gemma4",
                slug: "kaggle-gemma4",
                name: "Kaggle-Gemma4",
                kind: "project",
                path: "/work/kaggle-gemma4",
                has_git: true,
                purpose: null,
                tracker: null,
                store_path: null,
                active_agent_count: 2,
              },
            ],
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      if (url.endsWith("/api/v1/agents")) {
        return new Response(
          JSON.stringify({
            agents: [
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
                role: "developer",
                status: "active",
              },
              {
                id: "urn:fl:agent:athena",
                name_key: "athena",
                display_name: "Athena",
                project: "the-council",
                role: "strategist",
                status: "active",
              },
              {
                id: "urn:fl:agent:solon",
                name_key: "solon",
                display_name: "Solon",
                project: "the-council",
                role: "architect",
                status: "active",
              },
            ],
            routes: [],
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      if (url.endsWith("/api/v1/agents/liveness")) {
        return new Response(
          JSON.stringify({
            liveness: [
              {
                agent: "ferris",
                project: "FerroLoop",
                role: "lead",
                has_route: true,
                adapter: "claude",
                verdict: { state: "alive" },
              },
              {
                agent: "felix",
                project: "FerroLoop",
                role: "developer",
                has_route: true,
                adapter: "antigravity",
                verdict: { state: "alive" },
              },
              {
                agent: "athena",
                project: "the-council",
                role: "strategist",
                has_route: true,
                adapter: "claude",
                verdict: { state: "alive" },
              },
              {
                agent: "solon",
                project: "the-council",
                role: "architect",
                has_route: true,
                adapter: "codex",
                verdict: { state: "alive" },
              },
            ],
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      if (url.endsWith("/api/v1/sessions")) {
        return new Response(
          JSON.stringify({
            sessions: [
              {
                id: "sess-kacey-1",
                source: "route",
                adapter: "antigravity",
                persona: "kacey",
                status: "active",
                cwd: "/home/lmcfarlin/Projects/Artificial-Humanity/Kaggle-Gemma4",
              },
            ],
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

    // 1. On initial 401 bootstrap, pairing modal opens and NO static roster rows are rendered
    expect(await screen.findByTestId("roster-empty-state")).toHaveTextContent(
      /Sign in to view the workspace agent roster/i,
    );
    expect(screen.queryAllByTestId(/^agent-row-/)).toHaveLength(0);
    expect(screen.queryByTestId("agent-row-felicia")).toBeNull();
    expect(screen.queryByTestId("agent-row-ferris")).toBeNull();

    // 2. Authenticate via PairingModal token form
    const tokenInput = await screen.findByPlaceholderText("Paste bearer token");
    fireEvent.change(tokenInput, { target: { value: "valid-owner-token" } });
    fireEvent.click(screen.getByRole("button", { name: "Save Token" }));

    // 3. After login, projects, agents, liveness, and sessions refresh together
    expect(await screen.findByTestId("project-group-the council")).toBeInTheDocument();
    expect(screen.getByTestId("agent-row-athena")).toBeInTheDocument();
    expect(screen.getByTestId("agent-row-solon")).toBeInTheDocument();
    expect(screen.getByTestId("agent-row-felix")).toBeInTheDocument();
    expect(screen.getByTestId("agent-row-ferris")).toBeInTheDocument();

    // Session-only persona Kacey appears in the roster and renders online
    expect(screen.getByTestId("agent-row-kacey")).toBeInTheDocument();
    expect(screen.getByTestId("liveness-dot-kacey")).toHaveClass("fd-dot-alive");
    expect(screen.getByTestId("activity-tag-kacey")).toHaveAttribute(
      "data-activity",
      "idle",
    );

    unmount();
  });

  it("periodically polls /api/v1/agents and /api/v1/agents/liveness while authenticated", async () => {
    vi.useFakeTimers();
    try {
      let pollWave = 0;
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
        if (url.endsWith("/api/v1/agents")) {
          pollWave += 1;
          return new Response(
            JSON.stringify({
              agents:
                pollWave === 1
                  ? [
                      {
                        id: "urn:fl:agent:ferris",
                        name_key: "ferris",
                        display_name: "Ferris",
                        project: "FerroLoop",
                        role: "lead",
                        status: "active",
                      },
                    ]
                  : [
                      {
                        id: "urn:fl:agent:ferris",
                        name_key: "ferris",
                        display_name: "Ferris",
                        project: "FerroLoop",
                        role: "lead",
                        status: "active",
                      },
                      {
                        id: "urn:fl:agent:athena",
                        name_key: "athena",
                        display_name: "Athena",
                        project: "the-council",
                        role: "strategist",
                        status: "active",
                      },
                    ],
              routes: [],
            }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          );
        }
        if (url.endsWith("/api/v1/agents/liveness")) {
          return new Response(
            JSON.stringify({
              liveness:
                pollWave <= 1
                  ? [
                      {
                        agent: "ferris",
                        project: "FerroLoop",
                        role: "lead",
                        has_route: true,
                        adapter: "claude",
                        verdict: { state: "alive" },
                      },
                    ]
                  : [
                      {
                        agent: "ferris",
                        project: "FerroLoop",
                        role: "lead",
                        has_route: true,
                        adapter: "claude",
                        verdict: { state: "alive" },
                      },
                      {
                        agent: "athena",
                        project: "the-council",
                        role: "strategist",
                        has_route: true,
                        adapter: "claude",
                        verdict: { state: "alive" },
                      },
                    ],
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

      await act(async () => {
        await Promise.resolve();
      });
      expect(screen.getByTestId("agent-row-ferris")).toBeInTheDocument();
      expect(screen.queryByTestId("agent-row-athena")).toBeNull();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(ROSTER_POLL_INTERVAL_MS + 100);
      });

      expect(screen.getByTestId("agent-row-athena")).toBeInTheDocument();
      expect(screen.getByTestId("liveness-dot-athena")).toHaveClass("fd-dot-alive");

      unmount();
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps signed-out empty-state sign-in prompt after closing the pairing modal", async () => {
    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (
        url.includes("/api/v1/auth/session") ||
        url.endsWith("/api/v1/agents") ||
        url.endsWith("/api/v1/agents/liveness") ||
        url.endsWith("/api/v1/sessions") ||
        url.endsWith("/api/v1/projects")
      ) {
        return new Response(JSON.stringify({ error: "unauthorized" }), {
          status: 401,
          headers: { "Content-Type": "application/json" },
        });
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

    const closeBtn = await screen.findByRole("button", {
      name: "Close Pairing Modal",
    });
    await waitFor(() => {
      expect(screen.getByTestId("roster-empty-state")).toHaveTextContent(
        "Sign in to view the workspace agent roster.",
      );
    });

    // Close the pairing modal while still unauthenticated
    fireEvent.click(closeBtn);
    expect(
      screen.queryByRole("button", { name: "Close Pairing Modal" }),
    ).toBeNull();

    expect(screen.getByTestId("roster-empty-state")).toHaveTextContent(
      "Sign in to view the workspace agent roster.",
    );
    expect(screen.queryAllByTestId(/^agent-row-/)).toHaveLength(0);

    unmount();
  });

  it("leaves selectedAgent last-resort project unset instead of inventing FerroLoop", async () => {
    const recordsCalls: string[] = [];
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
      if (url.endsWith("/api/v1/agents")) {
        return new Response(JSON.stringify({ agents: [], routes: [] }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      if (url.endsWith("/api/v1/agents/liveness")) {
        return new Response(JSON.stringify({ liveness: [] }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      if (url.endsWith("/api/v1/sessions")) {
        return new Response(JSON.stringify({ sessions: [] }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      if (url.endsWith("/api/v1/projects")) {
        return new Response(
          JSON.stringify({
            projects: [
              {
                id: "ferroloop",
                slug: "ferroloop",
                name: "FerroLoop",
                kind: "project",
                path: "/work/ferroloop",
                has_git: true,
                purpose: null,
                tracker: null,
                store_path: null,
                active_agent_count: 0,
              },
              {
                id: "sonora",
                slug: "sonora",
                name: "Sonora",
                kind: "project",
                path: "/work/sonora",
                has_git: true,
                purpose: null,
                tracker: null,
                store_path: null,
                active_agent_count: 0,
              },
            ],
          }),
          {
            status: 200,
            headers: { "Content-Type": "application/json" },
          },
        );
      }
      if (url.includes("/api/v1/records")) {
        recordsCalls.push(url);
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

    const { unmount } = render(
      <App
        apiClient={apiClient}
        wsClient={wsStub}
        initialSelectedAgentName="ferris"
      />,
    );

    const projectBadge = await screen.findByTestId("active-project-badge");
    expect(projectBadge.textContent).toBe("");

    fireEvent.click(screen.getByTestId("split-view-toggle"));
    const rightHeader = screen.getByTestId("right-panel-header");
    fireEvent.click(within(rightHeader).getByTestId("right-panel-tab-board"));

    const boardSelect = (await screen.findByTestId(
      "board-project-select",
    )) as HTMLSelectElement;
    await waitFor(() => {
      expect(Array.from(boardSelect.options).map((o) => o.value)).toEqual([
        "",
        "ferroloop",
        "sonora",
      ]);
    });
    expect(boardSelect.value).toBe("");
    expect(boardSelect.selectedOptions[0]?.textContent).toBe("None");
    expect(recordsCalls).toEqual([]);
    expect(screen.getByTestId("active-context-tier-badge")).toHaveTextContent(
      "Scope: Project",
    );
    expect(
      screen.getByTestId("active-context-tier-badge"),
    ).not.toHaveTextContent(/ferroloop/i);

    unmount();
  });

  it("dedupes back-to-back visibilitychange and focus events into a single roster/liveness refetch", async () => {
    let agentsFetchCount = 0;
    let livenessFetchCount = 0;

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
      if (url.endsWith("/api/v1/agents")) {
        agentsFetchCount += 1;
        return new Response(
          JSON.stringify({
            agents: [
              {
                id: "urn:fl:agent:ferris",
                name_key: "ferris",
                display_name: "Ferris",
                project: "FerroLoop",
                role: "lead",
                status: "active",
              },
            ],
            routes: [],
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      if (url.endsWith("/api/v1/agents/liveness")) {
        livenessFetchCount += 1;
        return new Response(JSON.stringify({ liveness: [] }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
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

    await screen.findByTestId("agent-row-ferris");
    const baselineAgents = agentsFetchCount;
    const baselineLiveness = livenessFetchCount;

    Object.defineProperty(document, "hidden", {
      configurable: true,
      get: () => false,
    });

    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
      window.dispatchEvent(new Event("focus"));
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(agentsFetchCount - baselineAgents).toBe(1);
    expect(livenessFetchCount - baselineLiveness).toBe(1);

    unmount();
  });
});
