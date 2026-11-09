import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import type { ProjectSummary } from "../types/api";

function summary(
  slug: string,
  name: string,
  kind: "project" | "virtual" = "project",
): ProjectSummary {
  return {
    id: slug,
    slug,
    name,
    kind,
    path: kind === "project" ? `/work/${slug}` : null,
    has_git: false,
    purpose: null,
    tracker: null,
    store_path: null,
    active_agent_count: 0,
  };
}

const FERRO = summary("ferroloop", "FerroLoop");
const SONORA = summary("sonora", "Sonora");
const COUNCIL = summary("council", "Council", "virtual");

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

type ProjectsMode = "ok" | "error" | "missing" | "legacy";

/** A fake hub: the project list lives in `state.projects` and POST/DELETE change it. */
function makeHub(initial: ProjectSummary[], mode: ProjectsMode = "ok") {
  const state = { projects: [...initial], mode };
  const calls: { url: string; method: string; body: unknown }[] = [];
  const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const method = init?.method ?? "GET";
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    calls.push({ url, method, body });
    if (url.includes("/api/v1/auth/session")) {
      return json({ authenticated: true, authority: { tier: "owner", login: "lmcfarlin" } });
    }
    if (url.endsWith("/api/v1/agents")) {
      return json({
        agents: [
          { name_key: "ferris", display_name: "Ferris", project: "ferroloop", role: "lead" },
        ],
        routes: [],
      });
    }
    if (url.endsWith("/api/v1/projects")) {
      if (method === "POST") {
        const created = summary(body.slug, body.name, body.kind);
        state.projects.push(created);
        return json(created, 201);
      }
      if (method === "GET") {
        if (state.mode === "error") return json({ error: "boom" }, 500);
        if (state.mode === "missing") return json({ error: "not found" }, 404);
        if (state.mode === "legacy") {
          // The hub that predates project management: ids and artifact directories only.
          return json({
            projects: state.projects.map((p) => ({
              id: p.slug,
              artifacts_dir: null,
              default_artifacts_dir: `/art/${p.slug}`,
              effective_artifacts_dir: `/art/${p.slug}`,
            })),
          });
        }
        return json({ projects: state.projects });
      }
    }
    const del = url.match(/\/api\/v1\/projects\/([^/?]+)$/);
    if (del && method === "PATCH") {
      const index = state.projects.findIndex((p) => p.slug === del[1]);
      state.projects[index] = { ...state.projects[index], ...body };
      return json(state.projects[index]);
    }
    if (del && method === "DELETE") {
      state.projects = state.projects.filter((p) => p.slug !== del[1]);
      return json({
        deleted: del[1],
        reassigned_agents: 0,
        decommissioned_agents: 0,
        store_deleted: false,
        dir_deleted: false,
      });
    }
    return json({});
  });
  const client = new HubApiClient({
    baseUrl: "http://hub.test",
    fetchImpl: fetchImpl as unknown as typeof fetch,
  });
  return { client, state, calls };
}

function makeWsClient(): HubWsClient {
  return {
    setToken: vi.fn(),
    connect: vi.fn(),
    disconnect: vi.fn(),
    onStatusChange: () => () => {},
    subscribe: () => () => {},
    send: vi.fn(),
    sendFrame: vi.fn(),
  } as unknown as HubWsClient;
}

function setViewport(width: number, height: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, writable: true, value: width });
  Object.defineProperty(window, "innerHeight", { configurable: true, writable: true, value: height });
}

async function renderApp(client: HubApiClient, width = 1280, height = 800) {
  setViewport(width, height);
  const view = render(<App apiClient={client} wsClient={makeWsClient()} />);
  // The initial agent and project loads settle.
  await screen.findByTestId(width < 768 ? "phone-tab-agents" : "roster-sidebar");
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
  return view;
}

const filterOptions = () =>
  within(screen.getByTestId("settings-project-filter-select"))
    .getAllByRole("option")
    .map((o) => (o as HTMLOptionElement).value);

const spinUpOptions = () =>
  within(screen.getByTestId("spin-up-project-select"))
    .getAllByRole("option")
    .map((o) => (o as HTMLOptionElement).value);

async function openSettings() {
  fireEvent.click(screen.getByTestId("settings-modal-btn"));
  await screen.findByTestId("settings-project-filter-select");
  // Opening Settings reloads the shared list.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

async function closeSettings() {
  fireEvent.keyDown(window, { key: "Escape" });
  await waitFor(() => expect(screen.queryByTestId("settings-project-filter-select")).toBeNull());
}

async function openSpinUp() {
  fireEvent.click(screen.getByTestId("spin-up-agent-btn"));
  await screen.findByTestId("spin-up-project-select");
}

async function closeSpinUp() {
  fireEvent.keyDown(window, { key: "Escape" });
  await waitFor(() => expect(screen.queryByTestId("spin-up-project-select")).toBeNull());
}

async function openProjectsTab() {
  fireEvent.click(screen.getByTestId("settings-tab-projects"));
  await act(async () => {
    await Promise.resolve();
  });
}

async function createCouncilThroughSettings() {
  fireEvent.click(screen.getByTestId("settings-projects-create-btn"));
  fireEvent.click(screen.getByTestId("new-project-kind-virtual"));
  fireEvent.change(screen.getByTestId("new-project-name"), { target: { value: "Council" } });
  await act(async () => {
    fireEvent.click(screen.getByTestId("new-project-submit"));
  });
  await waitFor(() => expect(screen.queryByTestId("new-project-modal")).toBeNull());
}

describe("one project list for the switcher, the roster and Spin-Up", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });
  afterEach(() => {
    setViewport(1024, 768);
  });

  it("shows a virtual group in all three places after the initial load", async () => {
    const { client } = makeHub([FERRO, SONORA, COUNCIL]);
    await renderApp(client);

    // Roster group, even though no agent belongs to it.
    expect(screen.getByTestId("project-group-council")).toBeInTheDocument();
    expect(screen.getByTestId("project-group-ferroloop")).toBeInTheDocument();
    expect(screen.getByTestId("project-group-sonora")).toBeInTheDocument();

    await openSettings();
    expect(filterOptions()).toEqual(["all", "FerroLoop", "Sonora", "Council", "Workspace"]);
    expect(screen.getByRole("option", { name: "Council" })).toBeInTheDocument();
    await closeSettings();

    await openSpinUp();
    expect(spinUpOptions()).toEqual(["ferroloop", "sonora", "council"]);
  });

  it("orders the roster groups like the list", async () => {
    const { client } = makeHub([SONORA, COUNCIL, FERRO]);
    await renderApp(client);
    const groups = screen
      .getAllByTestId(/^project-group-(?!toggle)/)
      .map((g) => g.getAttribute("data-testid"));
    expect(groups).toEqual(["project-group-sonora", "project-group-council", "project-group-ferroloop"]);
  });

  it("puts agents on the group of their project slug", async () => {
    const { client } = makeHub([FERRO, SONORA, COUNCIL]);
    await renderApp(client);
    expect(
      within(screen.getByTestId("project-group-ferroloop")).getByText("Ferris"),
    ).toBeInTheDocument();
  });

  it("sends the slug when spinning up into a virtual group", async () => {
    const { client, calls } = makeHub([FERRO, SONORA, COUNCIL]);
    await renderApp(client);
    await openSpinUp();
    fireEvent.change(screen.getByTestId("spin-up-persona-input"), { target: { value: "kacey" } });
    fireEvent.change(screen.getByTestId("spin-up-project-select"), { target: { value: "council" } });
    await act(async () => {
      fireEvent.click(screen.getByTestId("spin-up-submit-btn"));
    });
    await waitFor(() =>
      expect(calls.some((c) => c.url.endsWith("/api/v1/sessions/spin-up"))).toBe(true),
    );
    const call = calls.find((c) => c.url.endsWith("/api/v1/sessions/spin-up"));
    expect(call?.body).toMatchObject({ persona: "kacey", project: "council" });
  });

  it("shows a group created in Settings -> Projects everywhere without a reload", async () => {
    const { client, calls } = makeHub([FERRO, SONORA]);
    await renderApp(client);
    expect(screen.queryByTestId("project-group-council")).toBeNull();

    await openSettings();
    expect(filterOptions()).not.toContain("Council");
    await openProjectsTab();
    await createCouncilThroughSettings();

    // The created group reaches the shared list through onProjectsChanged -> refresh.
    await waitFor(() => expect(screen.getByTestId("project-group-council")).toBeInTheDocument());
    expect(
      calls.filter((c) => c.method === "GET" && c.url.endsWith("/api/v1/projects")).length,
    ).toBeGreaterThanOrEqual(2);

    fireEvent.click(screen.getByTestId("settings-tab-agents"));
    await waitFor(() => expect(filterOptions()).toContain("Council"));
    await closeSettings();

    await openSpinUp();
    expect(spinUpOptions()).toEqual(["ferroloop", "sonora", "council"]);
  });

  it("drops a deleted entry from all three and resets the filter that pointed at it", async () => {
    const { client } = makeHub([FERRO, SONORA, COUNCIL]);
    await renderApp(client);
    await openSettings();
    await openProjectsTab();

    // Make Council the active filter, then come back and delete it.
    fireEvent.click(screen.getByTestId("settings-project-view-agents-council"));
    expect(screen.getByTestId("settings-project-filter-select")).toHaveValue("Council");
    fireEvent.click(screen.getByTestId("settings-tab-projects"));
    fireEvent.click(screen.getByTestId("settings-project-delete-council"));
    await act(async () => {
      fireEvent.click(screen.getByTestId("delete-project-submit"));
    });

    await waitFor(() => expect(screen.queryByTestId("project-group-council")).toBeNull());
    fireEvent.click(screen.getByTestId("settings-tab-agents"));
    await waitFor(() => expect(filterOptions()).not.toContain("Council"));
    expect(screen.getByTestId("settings-project-filter-select")).toHaveValue("all");
    await closeSettings();

    await openSpinUp();
    expect(spinUpOptions()).toEqual(["ferroloop", "sonora"]);
  });

  it("follows a project renamed on the hub in the Agents filter", async () => {
    const table = summary("council", "The Council", "virtual");
    const { client, state } = makeHub([FERRO, SONORA, table]);
    await renderApp(client);
    await openSettings();

    // Pick the project by its label, the way a user does.
    fireEvent.change(screen.getByTestId("settings-project-filter-select"), {
      target: { value: "The Council" },
    });
    expect(screen.getByTestId("settings-project-filter-select")).toHaveValue("The Council");

    // Someone else renames it on the hub; the next refresh in this session picks it up.
    state.projects = state.projects.map((p) =>
      p.slug === "council" ? { ...p, name: "Round Table" } : p,
    );
    await openProjectsTab();
    fireEvent.click(screen.getByTestId("settings-projects-create-btn"));
    fireEvent.click(screen.getByTestId("new-project-kind-virtual"));
    fireEvent.change(screen.getByTestId("new-project-name"), { target: { value: "Zeta" } });
    await act(async () => {
      fireEvent.click(screen.getByTestId("new-project-submit"));
    });
    await waitFor(() => expect(screen.queryByTestId("new-project-modal")).toBeNull());
    await waitFor(() =>
      expect(screen.getByTestId("project-group-zeta")).toBeInTheDocument(),
    );

    fireEvent.click(screen.getByTestId("settings-tab-agents"));
    await waitFor(() => expect(filterOptions()).toContain("Round Table"));
    expect(filterOptions()).not.toContain("The Council");
    expect(screen.getByTestId("settings-project-filter-select")).toHaveValue("Round Table");
  });

  it("resets the Agents filter when the project it points at is deleted elsewhere", async () => {
    const table = summary("council", "The Council", "virtual");
    const { client, state } = makeHub([FERRO, SONORA, table]);
    await renderApp(client);
    await openSettings();
    fireEvent.change(screen.getByTestId("settings-project-filter-select"), {
      target: { value: "The Council" },
    });
    state.projects = state.projects.filter((p) => p.slug !== "council");
    await openProjectsTab();
    fireEvent.click(screen.getByTestId("settings-projects-create-btn"));
    fireEvent.click(screen.getByTestId("new-project-kind-virtual"));
    fireEvent.change(screen.getByTestId("new-project-name"), { target: { value: "Zeta" } });
    await act(async () => {
      fireEvent.click(screen.getByTestId("new-project-submit"));
    });
    await waitFor(() => expect(screen.getByTestId("project-group-zeta")).toBeInTheDocument());
    fireEvent.click(screen.getByTestId("settings-tab-agents"));
    await waitFor(() => expect(filterOptions()).not.toContain("The Council"));
    expect(screen.getByTestId("settings-project-filter-select")).toHaveValue("all");
  });

  it("shows the load error, not the empty state, when the hub list fails", async () => {
    const { client, state } = makeHub([FERRO, SONORA], "error");
    await renderApp(client);
    await openSettings();
    await openProjectsTab();
    expect(screen.getByTestId("settings-projects-error")).toHaveTextContent(
      "Could not load projects",
    );
    expect(screen.queryByTestId("settings-projects-empty")).toBeNull();

    // A later successful reload replaces the error with the cards.
    state.mode = "ok";
    await closeSettings();
    await openSettings();
    await openProjectsTab();
    await waitFor(() =>
      expect(screen.getByTestId("settings-project-card-ferroloop")).toBeInTheDocument(),
    );
    expect(screen.queryByTestId("settings-projects-error")).toBeNull();
    expect(screen.getByTestId("settings-project-card-sonora")).toBeInTheDocument();
  });

  it.each<[string, ProjectsMode]>([
    ["fails", "error"],
    ["is missing on an older hub", "missing"],
  ])("does not render a guessed built-in project list when the endpoint %s", async (_label, mode) => {
    const { client } = makeHub([COUNCIL], mode);
    await renderApp(client);

    // The agents' own group is still there; no static built-in groups are added.
    expect(screen.getByTestId("project-group-ferroloop")).toBeInTheDocument();
    expect(screen.queryByTestId("project-group-council")).toBeNull();

    await openSettings();
    expect(filterOptions()).toEqual(["all", "Workspace", "ferroloop"]);
    await closeSettings();

    await openSpinUp();
    expect(spinUpOptions()).toEqual([""]);
  });

  it("shows the virtual group in the roster, Settings and Spin-Up at phone width", async () => {
    const { client } = makeHub([FERRO, SONORA, COUNCIL]);
    await renderApp(client, 390, 844);

    fireEvent.click(screen.getByTestId("phone-tab-agents"));
    expect(screen.getByTestId("roster-sidebar")).toHaveAttribute("data-embedded", "true");
    expect(screen.getByTestId("project-group-council")).toBeInTheDocument();

    await openSettings();
    expect(filterOptions()).toContain("Council");
    // On the phone, Spin-Up opens from Settings -> Agents -> New Agent.
    fireEvent.change(screen.getByTestId("settings-project-filter-select"), {
      target: { value: "Council" },
    });
    fireEvent.click(screen.getByTestId("settings-spin-up-btn"));
    await screen.findByTestId("spin-up-project-select");
    expect(spinUpOptions()).toContain("council");
    // The filtered project is preselected.
    expect(screen.getByTestId("spin-up-project-select")).toHaveValue("council");
  });

  it.each<[string, ProjectsMode]>([
    ["a legacy reply", "legacy"],
    ["a 404", "missing"],
  ])("shows the unsupported notice without static project fallbacks on %s", async (_label, mode) => {
    const { client } = makeHub([FERRO, SONORA], mode);
    await renderApp(client);

    // No empty groups from the hub's ids; the agent's own group is still there.
    expect(screen.getByTestId("project-group-ferroloop")).toBeInTheDocument();
    expect(screen.queryByTestId("project-group-sonora")).toBeNull();
    expect(screen.queryByTestId("project-group-lucida")).toBeNull();

    await openSettings();
    expect(filterOptions()).toEqual(["all", "Workspace", "ferroloop"]);
    await openProjectsTab();
    expect(screen.getByTestId("settings-projects-unsupported")).toHaveTextContent(
      "This hub does not support project management. Update the hub to create, edit or delete projects.",
    );
    expect(screen.queryByTestId("settings-projects-create-btn")).toBeNull();
    expect(screen.queryByTestId("settings-project-card-ferroloop")).toBeNull();
    expect(screen.queryByTestId("settings-project-edit-ferroloop")).toBeNull();
    await closeSettings();

    await openSpinUp();
    expect(spinUpOptions()).toEqual([""]);
  });

  it("shows the cards and no notice for a new-style reply", async () => {
    const { client } = makeHub([FERRO, SONORA]);
    await renderApp(client);
    await openSettings();
    await openProjectsTab();
    expect(screen.queryByTestId("settings-projects-unsupported")).toBeNull();
    expect(screen.getByTestId("settings-project-card-ferroloop")).toBeInTheDocument();
    expect(screen.getByTestId("settings-projects-create-btn")).toBeInTheDocument();
  });
});
