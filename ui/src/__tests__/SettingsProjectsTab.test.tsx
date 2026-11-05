import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import { SettingsModal } from "../components/SettingsModal";
import { ProjectsSettingsTab } from "../components/settings/ProjectsSettingsTab";
import type { ProjectSummary } from "../types/api";

const PLAIN: ProjectSummary = {
  id: "sonora",
  slug: "sonora",
  name: "Sonora",
  kind: "project",
  path: "/work/sonora",
  has_git: false,
  purpose: "Audio research",
  tracker: null,
  store_path: null,
  active_agent_count: 0,
  artifacts_dir: null,
  default_artifacts_dir: "/art/sonora",
  effective_artifacts_dir: "/art/sonora",
};

const GIT: ProjectSummary = {
  id: "ferroloop",
  slug: "ferroloop",
  name: "FerroLoop",
  kind: "project",
  path: "/work/ferroloop",
  has_git: true,
  purpose: "Core platform",
  tracker: { kind: "github", repo: "acme/ferroloop", credential: "app" },
  store_path: "/work/ferroloop/.ferro/ferroloop.fls",
  active_agent_count: 3,
  artifacts_dir: "/mnt/art/ferroloop",
  default_artifacts_dir: "/art/ferroloop",
  effective_artifacts_dir: "/mnt/art/ferroloop",
};

const GROUP: ProjectSummary = {
  id: "research-group",
  slug: "research-group",
  name: "Research Group",
  kind: "virtual",
  path: null,
  has_git: false,
  purpose: null,
  tracker: null,
  store_path: null,
  active_agent_count: 1,
};

const LAB: ProjectSummary = {
  id: "research-group",
  slug: "research-group",
  name: "Research Group",
  kind: "project",
  path: "/work/research",
  has_git: false,
  purpose: null,
  tracker: null,
  store_path: null,
  active_agent_count: 0,
  artifacts_dir: null,
  default_artifacts_dir: "/art/research-group",
  effective_artifacts_dir: "/art/research-group",
};

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function makeClient(
  projects: ProjectSummary[] | "error" = [PLAIN, GIT, GROUP],
  extra?: (url: string, method: string, body: unknown) => Response | null,
) {
  const calls: { url: string; method: string; body: unknown }[] = [];
  const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const method = init?.method ?? "GET";
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    calls.push({ url, method, body });
    const custom = extra?.(url, method, body);
    if (custom) return custom;
    if (url.endsWith("/api/v1/projects") && method === "GET") {
      return projects === "error"
        ? json({ error: "boom" }, 500)
        : json({ projects });
    }
    return json({});
  });
  const client = new HubApiClient({
    baseUrl: "http://hub.test",
    fetchImpl: fetchImpl as unknown as typeof fetch,
  });
  return { client, calls };
}

async function renderModal(
  client: HubApiClient,
  props: Partial<React.ComponentProps<typeof SettingsModal>> = {},
) {
  const view = render(
    <SettingsModal
      isOpen={true}
      onClose={vi.fn()}
      authority={{ tier: "owner", login: "lmcfarlin" }}
      apiClient={client}
      {...props}
    />,
  );
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
  return view;
}

async function openProjectsTab() {
  fireEvent.click(screen.getByTestId("settings-tab-projects"));
  await act(async () => {
    await Promise.resolve();
  });
}

describe("Settings Projects tab", () => {
  it("places Projects right after Agents in the select and the strip", async () => {
    const { client } = makeClient();
    await renderModal(client);
    const options = within(screen.getByTestId("settings-tab-select"))
      .getAllByRole("option")
      .map((o) => o.getAttribute("data-testid"));
    expect(options.slice(0, 3)).toEqual([
      "settings-tab-option-agents",
      "settings-tab-option-projects",
      "settings-tab-option-system",
    ]);
    const tabs = screen
      .getAllByRole("tab", { hidden: true })
      .map((t) => t.getAttribute("data-testid"));
    expect(tabs.slice(0, 3)).toEqual([
      "settings-tab-agents",
      "settings-tab-projects",
      "settings-tab-system",
    ]);
  });

  it("is absent in project-scoped Settings", async () => {
    const { client } = makeClient();
    await renderModal(client, { initialProject: "Sonora" });
    expect(screen.queryByTestId("settings-tab-option-projects")).toBeNull();
    expect(screen.queryByTestId("settings-tab-projects")).toBeNull();
    expect(screen.queryByTestId("settings-projects-tab")).toBeNull();
  });

  it("renders a card for a plain project", async () => {
    const { client } = makeClient();
    await renderModal(client);
    await openProjectsTab();
    const card = screen.getByTestId("settings-project-card-sonora");
    expect(card).toHaveTextContent("Sonora");
    expect(card).toHaveTextContent("sonora");
    expect(card).toHaveTextContent("Audio research");
    expect(card).toHaveTextContent("/work/sonora");
    expect(screen.getByTestId("settings-project-kind-sonora")).toHaveTextContent(
      "Project",
    );
    expect(within(card).queryByText("Git")).toBeNull();
    expect(card).toHaveTextContent("0 active agents");
  });

  it("renders a git project with tracker and agent count", async () => {
    const { client } = makeClient();
    await renderModal(client);
    await openProjectsTab();
    const card = screen.getByTestId("settings-project-card-ferroloop");
    expect(within(card).getByText("Git")).toBeInTheDocument();
    expect(card).toHaveTextContent("acme/ferroloop");
    expect(card).toHaveTextContent("app");
    expect(card).toHaveTextContent("3 active agents");
  });

  it("renders a virtual group with Workspace Root", async () => {
    const { client } = makeClient();
    await renderModal(client);
    await openProjectsTab();
    const card = screen.getByTestId("settings-project-card-research-group");
    expect(
      screen.getByTestId("settings-project-kind-research-group"),
    ).toHaveTextContent("Virtual Group");
    expect(card).toHaveTextContent("Workspace Root");
    expect(card).toHaveTextContent("1 active agent");
    expect(card).not.toHaveTextContent("1 active agents");
    expect(within(card).queryByText("Git")).toBeNull();
  });

  it("View Agents switches to the Agents tab with the filter applied", async () => {
    const { client } = makeClient();
    await renderModal(client);
    await openProjectsTab();
    fireEvent.click(screen.getByTestId("settings-project-view-agents-sonora"));
    expect(screen.getByTestId("settings-tab-agents")).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(screen.getByTestId("settings-tab-select")).toHaveValue("agents");
    expect(screen.getByTestId("settings-project-filter-select")).toHaveValue(
      "Sonora",
    );
  });

  it("View Agents on a virtual group filters by its slug", async () => {
    const { client } = makeClient();
    await renderModal(client);
    await openProjectsTab();
    fireEvent.click(
      screen.getByTestId("settings-project-view-agents-research-group"),
    );
    expect(screen.getByTestId("settings-project-filter-select")).toHaveValue(
      "research-group",
    );
  });

  it("shows the artifacts directory row in the card and not on the Agents tab", async () => {
    const { client } = makeClient();
    await renderModal(client);
    fireEvent.click(screen.getByTestId("settings-tab-agents"));
    fireEvent.change(screen.getByTestId("settings-project-filter-select"), {
      target: { value: "FerroLoop" },
    });
    expect(
      within(screen.getByTestId("settings-panel-agents")).queryByTestId(
        "settings-project-artifacts-dir-row",
      ),
    ).toBeNull();
    await openProjectsTab();
    const card = screen.getByTestId("settings-project-card-ferroloop");
    expect(
      within(card).getByTestId("settings-project-artifacts-dir-value"),
    ).toHaveTextContent("/mnt/art/ferroloop");
    expect(
      within(card).getByTestId("settings-project-artifacts-dir-override-badge"),
    ).toBeInTheDocument();
    const plain = screen.getByTestId("settings-project-card-sonora");
    expect(
      within(plain).getByTestId("settings-project-artifacts-dir-value"),
    ).toHaveTextContent("/art/sonora");
  });

  it("opens the change modal from the card and shows the status in that card", async () => {
    const { client, calls } = makeClient([LAB, GIT], (url, method, body) => {
      if (url.endsWith("/api/v1/projects/research-group/artifacts-dir") && method === "POST") {
        const b = body as { new_path: string | null };
        return json({
          artifacts_dir: b.new_path,
          default_artifacts_dir: "/art/sonora",
          effective_artifacts_dir: b.new_path,
          migrated_files: 0,
          migrated_bytes: 0,
        });
      }
      return null;
    });
    await renderModal(client);
    await openProjectsTab();
    const card = screen.getByTestId("settings-project-card-research-group");
    fireEvent.click(
      within(card).getByTestId("settings-project-change-artifacts-dir-btn"),
    );
    const modal = screen.getByTestId("change-artifacts-dir-modal");
    expect(
      within(modal).getByTestId("change-artifacts-dir-modal-title"),
    ).toHaveTextContent("Change Research Group Artifacts Directory");
    expect(modal).toHaveTextContent(
      "Override the artifacts directory for Research Group, or reset",
    );
    fireEvent.change(
      within(modal).getByTestId("change-artifacts-dir-new-path"),
      { target: { value: "/mnt/new/research" } },
    );
    fireEvent.click(
      within(modal).getByTestId("change-artifacts-dir-migrate-checkbox"),
    );
    fireEvent.click(
      within(modal).getByTestId("change-artifacts-dir-confirm-btn"),
    );
    const status = await within(card).findByTestId("project-artifacts-dir-status");
    expect(status).toHaveTextContent(
      "Updated Research Group artifacts directory to /mnt/new/research.",
    );
    expect(
      screen.getAllByTestId("project-artifacts-dir-status"),
    ).toHaveLength(1);
    expect(
      calls.find(
        (c) =>
          c.url.endsWith("/api/v1/projects/research-group/artifacts-dir") &&
          c.method === "POST",
      )?.body,
    ).toEqual({ new_path: "/mnt/new/research", migrate: false });
  });

  it("shows an empty state", async () => {
    const { client } = makeClient([]);
    await renderModal(client);
    await openProjectsTab();
    expect(screen.getByTestId("settings-projects-empty")).toBeInTheDocument();
    expect(screen.getByTestId("settings-projects-create-btn")).toBeInTheDocument();
  });

  it("shows an error state when the list fails", async () => {
    const { client } = makeClient("error");
    await renderModal(client);
    await openProjectsTab();
    expect(screen.getByTestId("settings-projects-error")).toBeInTheDocument();
    expect(screen.queryByTestId("settings-projects-empty")).toBeNull();
  });

  it("loads the list once per open", async () => {
    const { client, calls } = makeClient();
    await renderModal(client);
    await openProjectsTab();
    fireEvent.click(screen.getByTestId("settings-tab-agents"));
    await openProjectsTab();
    const gets = calls.filter(
      (c) => c.url.endsWith("/api/v1/projects") && c.method === "GET",
    );
    // One call feeds the artifacts-dir map, one feeds the Projects tab.
    expect(gets).toHaveLength(2);
  });

  it("names the card heading and labels its buttons with the project", () => {
    render(
      <ProjectsSettingsTab
        projects={[LAB]}
        loading={false}
        error={null}
        onViewAgents={vi.fn()}
        artifactsDir={{
          getState: () => ({ effectiveDir: "/x", artifactsDir: null }),
          onChange: vi.fn(),
          status: null,
        }}
      />,
    );
    expect(
      screen.getByRole("heading", { level: 3, name: "Research Group" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Edit Research Group" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Delete Research Group" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "View agents for Research Group" }),
    ).toBeInTheDocument();
  });

  it("passes the project to the Edit, Delete and Create callbacks", () => {
    const onCreate = vi.fn();
    const onEdit = vi.fn();
    const onDelete = vi.fn();
    const onViewAgents = vi.fn();
    render(
      <ProjectsSettingsTab
        projects={[GIT]}
        loading={false}
        error={null}
        onViewAgents={onViewAgents}
        onCreate={onCreate}
        onEdit={onEdit}
        onDelete={onDelete}
        artifactsDir={{
          getState: () => ({ effectiveDir: "/x", artifactsDir: null }),
          onChange: vi.fn(),
          status: null,
        }}
      />,
    );
    fireEvent.click(screen.getByTestId("settings-projects-create-btn"));
    fireEvent.click(screen.getByTestId("settings-project-edit-ferroloop"));
    fireEvent.click(screen.getByTestId("settings-project-delete-ferroloop"));
    fireEvent.click(screen.getByTestId("settings-project-view-agents-ferroloop"));
    expect(onCreate).toHaveBeenCalledTimes(1);
    expect(onEdit).toHaveBeenCalledWith(GIT);
    expect(onDelete).toHaveBeenCalledWith(GIT);
    expect(onViewAgents).toHaveBeenCalledWith("ferroloop");
  });

  it("renders the buttons without callbacks and clicking them is harmless", () => {
    render(
      <ProjectsSettingsTab
        projects={[GIT]}
        loading={false}
        error={null}
        onViewAgents={vi.fn()}
        artifactsDir={{
          getState: () => ({ effectiveDir: "/x", artifactsDir: null }),
          onChange: vi.fn(),
          status: null,
        }}
      />,
    );
    fireEvent.click(screen.getByTestId("settings-projects-create-btn"));
    fireEvent.click(screen.getByTestId("settings-project-edit-ferroloop"));
    fireEvent.click(screen.getByTestId("settings-project-delete-ferroloop"));
  });

  describe("create flow", () => {
    const NEW_GROUP: ProjectSummary = {
      id: "council",
      slug: "council",
      name: "Council",
      kind: "virtual",
      path: null,
      has_git: false,
      purpose: "Cross-project scope",
      tracker: null,
      store_path: null,
      active_agent_count: 0,
    };

    const createHandler =
      (created: ProjectSummary) =>
      (url: string, method: string): Response | null =>
        url.endsWith("/api/v1/projects") && method === "POST"
          ? json(created, 201)
          : null;

    it("opens the New Project modal from the create button", async () => {
      const { client } = makeClient();
      await renderModal(client);
      await openProjectsTab();
      expect(screen.queryByTestId("new-project-modal")).toBeNull();
      fireEvent.click(screen.getByTestId("settings-projects-create-btn"));
      expect(screen.getByTestId("new-project-modal")).toBeInTheDocument();
    });

    it("shows the new card and calls onProjectsChanged once after a create", async () => {
      const onProjectsChanged = vi.fn();
      const { client, calls } = makeClient(
        [PLAIN, GIT, GROUP],
        createHandler(NEW_GROUP),
      );
      await renderModal(client, { onProjectsChanged });
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-projects-create-btn"));
      fireEvent.click(screen.getByTestId("new-project-kind-virtual"));
      fireEvent.change(screen.getByTestId("new-project-name"), {
        target: { value: "Council" },
      });
      fireEvent.change(screen.getByTestId("new-project-purpose"), {
        target: { value: "Cross-project scope" },
      });
      await act(async () => {
        fireEvent.click(screen.getByTestId("new-project-submit"));
      });
      expect(screen.queryByTestId("new-project-modal")).toBeNull();
      expect(screen.getByTestId("settings-tab-projects")).toHaveAttribute(
        "aria-selected",
        "true",
      );
      const card = screen.getByTestId("settings-project-card-council");
      expect(card).toHaveTextContent("Council");
      expect(card).toHaveTextContent("Workspace Root");
      expect(onProjectsChanged).toHaveBeenCalledTimes(1);
      expect(
        calls.find((c) => c.method === "POST" && c.url.endsWith("/api/v1/projects"))
          ?.body,
      ).toEqual({
        name: "Council",
        slug: "council",
        kind: "virtual",
        purpose: "Cross-project scope",
      });
    });

    it("replaces an existing card with the same slug instead of duplicating it", async () => {
      const renamed = { ...PLAIN, name: "Sonora Renamed" };
      const { client } = makeClient([PLAIN], createHandler(renamed));
      await renderModal(client);
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-projects-create-btn"));
      fireEvent.change(screen.getByTestId("new-project-name"), {
        target: { value: "Other" },
      });
      fireEvent.change(screen.getByTestId("new-project-path"), {
        target: { value: "/work/other" },
      });
      await act(async () => {
        fireEvent.click(screen.getByTestId("new-project-submit"));
      });
      expect(screen.getAllByTestId("settings-project-card-sonora")).toHaveLength(1);
      expect(screen.getByTestId("settings-project-card-sonora")).toHaveTextContent(
        "Sonora Renamed",
      );
    });

    it("rejects a slug already on the list without calling the hub", async () => {
      const { client, calls } = makeClient();
      await renderModal(client);
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-projects-create-btn"));
      fireEvent.change(screen.getByTestId("new-project-name"), {
        target: { value: "Sonora" },
      });
      fireEvent.change(screen.getByTestId("new-project-path"), {
        target: { value: "/work/s" },
      });
      await act(async () => {
        fireEvent.click(screen.getByTestId("new-project-submit"));
      });
      expect(
        calls.filter((c) => c.method === "POST" && c.url.endsWith("/api/v1/projects")),
      ).toHaveLength(0);
      expect(screen.getByTestId("new-project-slug")).toHaveAttribute(
        "aria-invalid",
        "true",
      );
    });

    it("Escape closes only the New Project modal, not Settings", async () => {
      const onClose = vi.fn();
      const { client } = makeClient();
      await renderModal(client, { onClose });
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-projects-create-btn"));
      fireEvent.keyDown(window, { key: "Escape" });
      expect(screen.queryByTestId("new-project-modal")).toBeNull();
      expect(onClose).not.toHaveBeenCalled();
      fireEvent.keyDown(window, { key: "Escape" });
      expect(onClose).toHaveBeenCalledTimes(1);
    });

    it("returns focus to the create button after closing", async () => {
      const { client } = makeClient();
      await renderModal(client);
      await openProjectsTab();
      const btn = screen.getByTestId("settings-projects-create-btn");
      btn.focus();
      fireEvent.click(btn);
      expect(screen.getByTestId("new-project-name")).toHaveFocus();
      fireEvent.click(screen.getByTestId("new-project-cancel"));
      expect(btn).toHaveFocus();
    });

    it("keeps the Projects tab and shows the server error on a failed create", async () => {
      const onProjectsChanged = vi.fn();
      const { client } = makeClient([PLAIN], (url, method) =>
        url.endsWith("/api/v1/projects") && method === "POST"
          ? json({ error: "slug already registered" }, 409)
          : null,
      );
      await renderModal(client, { onProjectsChanged });
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-projects-create-btn"));
      fireEvent.change(screen.getByTestId("new-project-name"), {
        target: { value: "Fresh" },
      });
      fireEvent.change(screen.getByTestId("new-project-path"), {
        target: { value: "/work/fresh" },
      });
      await act(async () => {
        fireEvent.click(screen.getByTestId("new-project-submit"));
      });
      expect(screen.getByTestId("new-project-error")).toHaveTextContent(
        /slug already registered/,
      );
      expect(screen.getByTestId("new-project-name")).toHaveValue("Fresh");
      expect(onProjectsChanged).not.toHaveBeenCalled();
      expect(screen.queryByTestId("settings-project-card-fresh")).toBeNull();
    });
  });
});
