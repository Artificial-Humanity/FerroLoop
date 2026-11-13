import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import { SettingsModal } from "../components/SettingsModal";
import { AgentSettingsModal } from "../components/AgentSettingsModal";
import { areaInfo } from "../components/AreaCoverage";
import { RosterSidebar } from "../components/RosterSidebar";
import type { AgentRecord, AreaRecord, ProjectSummary } from "../types/api";

const director: AgentRecord = { id: "d", name_key: "dana", display_name: "Dana", project: "alpha", role: "director", status: "active", extra_areas: ["beta", "kaggle"] };
const areas: AreaRecord[] = [
  { area_key: "alpha", name: "Alpha", path: "/a", purpose: "" },
  { area_key: "beta", name: "Beta", path: "/b", purpose: "" },
  { area_key: "kaggle", name: "Kaggle", path: null, purpose: "" },
];
const projects = areas.slice(0, 2).map(a => ({ id: a.area_key, slug: a.area_key, name: a.name, kind: "project", path: a.path, has_git: false, purpose: null, tracker: null, store_path: null, active_agent_count: 0 })) as ProjectSummary[];
afterEach(() => { cleanup(); window.localStorage.clear(); });

describe("agent coverage", () => {
  it("shows a director in each covered area and distinguishes virtual coverage", () => {
    render(<RosterSidebar agents={[director]} areas={areas} projects={projects} routes={[]} liveness={[]} unreadByAgent={{}} selectedAgentName={null} onSelectAgent={vi.fn()} embedded />);
    expect(within(screen.getByTestId("project-group-alpha")).getByText("Dana")).toBeInTheDocument();
    expect(within(screen.getByTestId("project-group-beta")).getByText("Dana")).toBeInTheDocument();
    const virtual = screen.getByTestId("project-group-kaggle");
    expect(within(virtual).getByText("Dana")).toBeInTheDocument();
    expect(within(virtual).getByText("virtual")).toBeInTheDocument();
    expect(within(screen.getByTestId("project-group-beta")).getByText(/also covers/i)).toBeInTheDocument();
  });
  it("finds agents by a covered area name", () => {
    render(<RosterSidebar agents={[director]} areas={areas} projects={projects} routes={[]} liveness={[]} unreadByAgent={{}} selectedAgentName={null} onSelectAgent={vi.fn()} embedded />);
    fireEvent.change(screen.getByPlaceholderText("Search agents, projects, adapters..."), { target: { value: "Kaggle" } });
    expect(screen.getAllByText("Dana").length).toBeGreaterThan(0);
  });
  it("filters the agent list by extra coverage while keeping default bulk scope primary", () => {
    const client = new HubApiClient({ baseUrl: "http://hub.test", fetchImpl: vi.fn(async () => new Response("{}", { headers: { "Content-Type": "application/json" } })) as typeof fetch });
    render(<SettingsModal isOpen onClose={vi.fn()} agents={[director]} areas={areas} projects={projects} projectsFromHub apiClient={client} />);
    fireEvent.change(screen.getByTestId("settings-project-filter-select"), { target: { value: "Kaggle" } });
    const row = screen.getByTestId("settings-agent-row-dana");
    expect(row).toHaveTextContent("Dana");
    expect(row).toHaveTextContent("Beta");
    expect(row).toHaveTextContent("Kaggle · virtual");
    expect(screen.getByTestId("bulk-session-ops-heading")).toHaveTextContent("0 Agents");
  });

});

const betaAgent: AgentRecord = { ...director, id: "b", name_key: "bea", display_name: "Bea", project: "beta", extra_areas: [] };
function client() {
  return new HubApiClient({ baseUrl: "http://hub.test", fetchImpl: vi.fn(async () => new Response("{}", { headers: { "Content-Type": "application/json" } })) as typeof fetch });
}

describe("coverage review regressions", () => {
  it.each(["individual", "all"])("keeps %s selection within primary-project bulk scope", async (selection) => {
    const onBulkSessionAction = vi.fn().mockResolvedValue({ action: "clear", total: 1, succeeded: 1, results: [] });
    render(<SettingsModal isOpen onClose={vi.fn()} agents={[director, betaAgent]} areas={areas} projects={projects} projectsFromHub apiClient={client()} onBulkSessionAction={onBulkSessionAction} />);
    fireEvent.change(screen.getByTestId("settings-project-filter-select"), { target: { value: "Beta" } });
    expect(screen.getByTestId("settings-agent-row-dana")).toBeInTheDocument();
    expect(screen.getByTestId("agent-select-checkbox-dana")).toBeDisabled();
    fireEvent.click(screen.getByTestId(selection === "all" ? "settings-agents-select-all-btn" : "agent-select-checkbox-bea"));
    fireEvent.click(screen.getByTestId("bulk-clear-btn"));
    expect(screen.getByTestId("bulk-confirm-panel")).not.toHaveTextContent("dana");
    fireEvent.click(screen.getByTestId("bulk-confirm-btn"));
    await waitFor(() => expect(onBulkSessionAction).toHaveBeenCalledWith("clear", ["bea"]));
  });
  it.each(["clear", "teardown"] as const)("keeps direct API %s calls within selected primary-project agents", async action => {
    const apiClient = client();
    const clear = vi.spyOn(apiClient, "clearSession").mockResolvedValue({ ok: true, session_id: "bea", mode: "clear" });
    const decommission = vi.spyOn(apiClient, "decommissionSession").mockResolvedValue({ session_id: "bea", mode: "teardown", status: "completed", handshake_dispatched: true, unregistered: true, runtime_stopped: true });
    render(<SettingsModal isOpen onClose={vi.fn()} agents={[director, betaAgent]} areas={areas} projects={projects} projectsFromHub apiClient={apiClient} />);
    fireEvent.change(screen.getByTestId("settings-project-filter-select"), { target: { value: "Beta" } });
    fireEvent.click(screen.getByTestId("settings-agents-select-all-btn"));
    fireEvent.click(screen.getByTestId(action === "clear" ? "bulk-clear-btn" : "bulk-action-teardown-btn"));
    fireEvent.click(screen.getByTestId("bulk-confirm-btn"));
    const request = action === "clear" ? clear : decommission;
    await waitFor(() => expect(request).toHaveBeenCalledWith("bea", action));
    expect(request).toHaveBeenCalledTimes(1);
  });
  it("does not widen an explicit selection when its primary project changes", async () => {
    const apiClient = client();
    const onBulkSessionAction = vi.fn().mockResolvedValue({ action: "clear", total: 0, succeeded: 0, results: [] });
    const props = { isOpen: true, onClose: vi.fn(), areas, projects, projectsFromHub: true, apiClient, onBulkSessionAction };
    const otherBeta = { ...betaAgent, id: "c", name_key: "bob", display_name: "Bob" };
    const { rerender } = render(<SettingsModal {...props} agents={[director, betaAgent, otherBeta]} />);
    fireEvent.change(screen.getByTestId("settings-project-filter-select"), { target: { value: "Beta" } });
    fireEvent.click(screen.getByTestId("agent-select-checkbox-bea"));
    fireEvent.click(screen.getByTestId("bulk-clear-btn"));
    rerender(<SettingsModal {...props} agents={[director, { ...betaAgent, project: "alpha", extra_areas: ["beta"] }, otherBeta]} />);
    fireEvent.click(screen.getByTestId("bulk-confirm-btn"));
    await waitFor(() => expect(onBulkSessionAction).toHaveBeenCalledWith("clear", []));
  });
  it("prefers a real project registry entry over a pathless area in chips and headers", () => {
    const pathless = areas.map(a => ({ ...a, path: null }));
    expect(areaInfo("alpha", pathless, projects).virtual).toBe(false);
    render(<RosterSidebar agents={[director]} areas={pathless} projects={projects} routes={[]} liveness={[]} unreadByAgent={{}} selectedAgentName={null} onSelectAgent={vi.fn()} embedded />);
    expect(screen.getByTestId("project-group-toggle-alpha")).not.toHaveTextContent("virtual");
    expect(within(screen.getByTestId("project-group-alpha")).queryByText("Beta · virtual")).not.toBeInTheDocument();
  });
  it("labels virtual primary areas in the agent list even without extra coverage", () => {
    render(<SettingsModal isOpen onClose={vi.fn()} agents={[{ ...director, project: "kaggle", extra_areas: [] }]} areas={areas} projects={projects} projectsFromHub apiClient={client()} />);
    expect(within(screen.getByTestId("settings-agent-row-dana")).getByText("Kaggle · virtual")).toBeInTheDocument();
  });
  it("labels a virtual primary area in agent detail without extra coverage", () => {
    render(<AgentSettingsModal isOpen onClose={vi.fn()} agentName="dana" agentProject="kaggle" areas={areas} projects={projects} apiClient={client()} selectedModel="" selectedThinkingLevel="" onChangeModelAndThinking={vi.fn()} contextEstimate={{ usedTokens: 0, maxTokens: 200000, percent: 0 }} canCompact={false} compacting={false} onCompactContext={vi.fn()} canClear={false} clearing={false} onOpenClearModal={vi.fn()} canDecommission={false} decommissioning={false} onOpenDecommissionModal={vi.fn()} />);
    expect(screen.getByText("Kaggle · virtual")).toBeInTheDocument();
  });
  it("shows the primary area and all extras in a roster row under an extra area", () => {
    render(<RosterSidebar agents={[director]} areas={areas} projects={projects} routes={[]} liveness={[]} unreadByAgent={{}} selectedAgentName={null} onSelectAgent={vi.fn()} embedded />);
    const group = screen.getByTestId("project-group-beta");
    expect(within(group).getByText("Primary: Alpha")).toBeInTheDocument();
    expect(within(group).getByLabelText("Additional coverage")).toHaveTextContent("Beta");
    expect(within(group).getByLabelText("Additional coverage")).toHaveTextContent("Kaggle · virtual");
  });
});
