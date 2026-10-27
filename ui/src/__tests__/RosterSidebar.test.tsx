import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ContextHeader } from "../components/ContextHeader";
import { PairingModal } from "../components/PairingModal";
import { RosterSidebar } from "../components/RosterSidebar";
import {
  DEFAULT_WORKSPACE_AGENTS,
  type AgentLivenessItem,
  type AgentRecord,
  type DiscoveredSessionSummary,
} from "../types/api";

describe("RosterSidebar", () => {
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

  it("renders agent context, mode tabs, session selector, authority pill, and split-view toggle", () => {
    const onSelectTab = vi.fn();
    const onToggleSplitView = vi.fn();
    const onSelectSession = vi.fn();
    const onOpenPairingModal = vi.fn();

    render(
      <ContextHeader
        selectedAgent={ferris}
        sessions={sessions}
        selectedSessionId="sess-1"
        onSelectSession={onSelectSession}
        activeTab="chat"
        onSelectTab={onSelectTab}
        splitView={false}
        onToggleSplitView={onToggleSplitView}
        authority={{ tier: "owner", login: "lmcfarlin" }}
        wsConnected={true}
        onOpenPairingModal={onOpenPairingModal}
        onOpenMobileRoster={() => {}}
      />,
    );

    expect(screen.getByText("Ferris")).toBeInTheDocument();
    expect(screen.getByText("FerroLoop")).toBeInTheDocument();
    expect(screen.getByText(/Owner:\s*lmcfarlin/i)).toBeInTheDocument();

    // Switch tab to Git & PRs
    fireEvent.click(screen.getByRole("tab", { name: /Git & PRs/i }));
    expect(onSelectTab).toHaveBeenCalledWith("git");

    // Toggle split view
    fireEvent.click(screen.getByTestId("split-view-toggle"));
    expect(onToggleSplitView).toHaveBeenCalledTimes(1);

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
