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

  it("passes the project and the clicked button to the Edit, Delete and Create callbacks", () => {
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
    expect(onCreate).toHaveBeenCalledWith(
      screen.getByTestId("settings-projects-create-btn"),
    );
    expect(onEdit).toHaveBeenCalledWith(
      GIT,
      screen.getByTestId("settings-project-edit-ferroloop"),
    );
    expect(onDelete).toHaveBeenCalledWith(
      GIT,
      screen.getByTestId("settings-project-delete-ferroloop"),
    );
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

    it("returns focus to the create button after each way of closing", async () => {
      // The button is never focused before the click (Safari/Firefox on macOS
      // do not focus a button on click); the tab captures it at click time.
      const closers: Array<() => void> = [
        () => fireEvent.keyDown(window, { key: "Escape" }),
        () => fireEvent.click(screen.getByTestId("new-project-cancel")),
        () => fireEvent.click(screen.getByTestId("new-project-modal-backdrop")),
      ];
      for (const close of closers) {
        const { client } = makeClient();
        const view = await renderModal(client);
        await openProjectsTab();
        const btn = screen.getByTestId("settings-projects-create-btn");
        expect(btn).not.toHaveFocus();
        fireEvent.click(btn);
        expect(screen.getByTestId("new-project-name")).toHaveFocus();
        close();
        expect(screen.queryByTestId("new-project-modal")).toBeNull();
        expect(btn).toHaveFocus();
        view.unmount();
      }
    });

    it("returns focus to the create button after a successful create", async () => {
      const { client } = makeClient([PLAIN, GIT, GROUP], createHandler(NEW_GROUP));
      await renderModal(client);
      await openProjectsTab();
      const btn = screen.getByTestId("settings-projects-create-btn");
      fireEvent.click(btn);
      fireEvent.click(screen.getByTestId("new-project-kind-virtual"));
      fireEvent.change(screen.getByTestId("new-project-name"), {
        target: { value: "Council" },
      });
      await act(async () => {
        fireEvent.click(screen.getByTestId("new-project-submit"));
      });
      expect(screen.queryByTestId("new-project-modal")).toBeNull();
      expect(btn).toHaveFocus();
    });

    it("does not reopen the New Project modal when Settings closes and reopens", async () => {
      const { client } = makeClient();
      const view = await renderModal(client);
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-projects-create-btn"));
      expect(screen.getByTestId("new-project-modal")).toBeInTheDocument();
      view.rerender(
        <SettingsModal
          isOpen={false}
          onClose={vi.fn()}
          authority={{ tier: "owner", login: "lmcfarlin" }}
          apiClient={client}
        />,
      );
      view.rerender(
        <SettingsModal
          isOpen={true}
          onClose={vi.fn()}
          authority={{ tier: "owner", login: "lmcfarlin" }}
          apiClient={client}
        />,
      );
      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(screen.queryByTestId("new-project-modal")).toBeNull();
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
  describe("edit flow", () => {
    const patchHandler =
      (slug: string, respond: (body: Record<string, unknown>) => ProjectSummary) =>
      (url: string, method: string, body: unknown): Response | null =>
        url.endsWith(`/api/v1/projects/${slug}`) && method === "PATCH"
          ? json(respond(body as Record<string, unknown>))
          : null;

    it("opens the Edit modal from the card and focuses Name", async () => {
      const { client } = makeClient();
      await renderModal(client);
      await openProjectsTab();
      expect(screen.queryByTestId("edit-project-modal")).toBeNull();
      fireEvent.click(screen.getByTestId("settings-project-edit-ferroloop"));
      expect(screen.getByTestId("edit-project-modal")).toBeInTheDocument();
      expect(screen.getByTestId("edit-project-name")).toHaveValue("FerroLoop");
      expect(screen.getByTestId("edit-project-name")).toHaveFocus();
    });

    it("sends only the changed fields, updates the card and calls onProjectsChanged once", async () => {
      const onProjectsChanged = vi.fn();
      const { client, calls } = makeClient(
        [PLAIN, GIT, GROUP],
        patchHandler("ferroloop", (b) => ({ ...GIT, ...b }) as ProjectSummary),
      );
      await renderModal(client, { onProjectsChanged });
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-edit-ferroloop"));
      fireEvent.change(screen.getByTestId("edit-project-name"), {
        target: { value: "Ferro Loop" },
      });
      await act(async () => {
        fireEvent.click(screen.getByTestId("edit-project-submit"));
      });
      expect(screen.queryByTestId("edit-project-modal")).toBeNull();
      expect(
        calls.find((c) => c.method === "PATCH")?.body,
      ).toEqual({ name: "Ferro Loop" });
      expect(screen.getAllByTestId("settings-project-card-ferroloop")).toHaveLength(1);
      expect(screen.getByTestId("settings-project-card-ferroloop")).toHaveTextContent(
        "Ferro Loop",
      );
      expect(onProjectsChanged).toHaveBeenCalledTimes(1);
    });

    it("clearing the tracker sends github: null", async () => {
      const { client, calls } = makeClient(
        [PLAIN, GIT, GROUP],
        patchHandler("ferroloop", (b) => ({ ...GIT, tracker: null, ...b }) as ProjectSummary),
      );
      await renderModal(client);
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-edit-ferroloop"));
      fireEvent.change(screen.getByTestId("edit-project-github"), {
        target: { value: "" },
      });
      await act(async () => {
        fireEvent.click(screen.getByTestId("edit-project-submit"));
      });
      expect(calls.find((c) => c.method === "PATCH")?.body).toEqual({ github: null });
      expect(screen.getByTestId("settings-project-card-ferroloop")).not.toHaveTextContent(
        "acme/ferroloop",
      );
    });

    it("disables Save with no change and sends nothing", async () => {
      const { client, calls } = makeClient();
      await renderModal(client);
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-edit-sonora"));
      expect(screen.getByTestId("edit-project-submit")).toBeDisabled();
      fireEvent.submit(screen.getByTestId("edit-project-submit").closest("form")!);
      await act(async () => {});
      expect(calls.filter((c) => c.method === "PATCH")).toHaveLength(0);
    });

    it("a virtual group edit has no path or tracker fields", async () => {
      const { client } = makeClient();
      await renderModal(client);
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-edit-research-group"));
      expect(screen.getByTestId("edit-project-name")).toBeInTheDocument();
      expect(screen.getByTestId("edit-project-purpose")).toBeInTheDocument();
      for (const id of ["path", "github", "credential"]) {
        expect(screen.queryByTestId(`edit-project-${id}`)).toBeNull();
      }
    });

    it("validation blocks the request", async () => {
      const { client, calls } = makeClient();
      await renderModal(client);
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-edit-sonora"));
      fireEvent.change(screen.getByTestId("edit-project-path"), {
        target: { value: "relative/dir" },
      });
      await act(async () => {
        fireEvent.click(screen.getByTestId("edit-project-submit"));
      });
      expect(calls.filter((c) => c.method === "PATCH")).toHaveLength(0);
      expect(screen.getByTestId("edit-project-path")).toHaveAttribute(
        "aria-invalid",
        "true",
      );
    });

    it("shows the server error and keeps the dialog open", async () => {
      const onProjectsChanged = vi.fn();
      const { client } = makeClient([PLAIN], (url, method) =>
        url.endsWith("/api/v1/projects/sonora") && method === "PATCH"
          ? json({ error: "directory does not exist" }, 400)
          : null,
      );
      await renderModal(client, { onProjectsChanged });
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-edit-sonora"));
      fireEvent.change(screen.getByTestId("edit-project-path"), {
        target: { value: "/nope" },
      });
      await act(async () => {
        fireEvent.click(screen.getByTestId("edit-project-submit"));
      });
      expect(screen.getByTestId("edit-project-error")).toHaveTextContent(
        /directory does not exist/,
      );
      expect(screen.getByTestId("edit-project-path")).toHaveValue("/nope");
      expect(onProjectsChanged).not.toHaveBeenCalled();
    });

    it("Escape closes only the Edit modal, then Settings", async () => {
      const onClose = vi.fn();
      const { client } = makeClient();
      await renderModal(client, { onClose });
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-edit-sonora"));
      fireEvent.keyDown(window, { key: "Escape" });
      expect(screen.queryByTestId("edit-project-modal")).toBeNull();
      expect(onClose).not.toHaveBeenCalled();
      fireEvent.keyDown(window, { key: "Escape" });
      expect(onClose).toHaveBeenCalledTimes(1);
    });

    it("returns focus to the Edit button after each way of closing", async () => {
      const closers: Array<() => void> = [
        () => fireEvent.keyDown(window, { key: "Escape" }),
        () => fireEvent.click(screen.getByTestId("edit-project-cancel")),
        () => fireEvent.click(screen.getByTestId("edit-project-modal-backdrop")),
      ];
      for (const close of closers) {
        const { client } = makeClient();
        const view = await renderModal(client);
        await openProjectsTab();
        const btn = screen.getByTestId("settings-project-edit-sonora");
        expect(btn).not.toHaveFocus();
        fireEvent.click(btn);
        close();
        expect(screen.queryByTestId("edit-project-modal")).toBeNull();
        expect(btn).toHaveFocus();
        view.unmount();
      }
    });

    it("does not reopen when Settings closes and reopens", async () => {
      const { client } = makeClient();
      const view = await renderModal(client);
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-edit-sonora"));
      for (const isOpen of [false, true]) {
        view.rerender(
          <SettingsModal
            isOpen={isOpen}
            onClose={vi.fn()}
            authority={{ tier: "owner", login: "lmcfarlin" }}
            apiClient={client}
          />,
        );
      }
      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(screen.queryByTestId("edit-project-modal")).toBeNull();
    });
  });

  describe("delete flow", () => {
    const FULL: DeleteResponse = {
      deleted: "ferroloop",
      reassigned_agents: 3,
      decommissioned_agents: 0,
      store_deleted: true,
      dir_deleted: false,
    };
    type DeleteResponse = import("../types/api").DeleteProjectResponse;

    const deleteHandler =
      (slug: string, respond: () => Response) =>
      (url: string, method: string): Response | null =>
        url.endsWith(`/api/v1/projects/${slug}`) && method === "DELETE"
          ? respond()
          : null;

    it("opens the Delete dialog from the card, with the guard only for active agents", async () => {
      const { client } = makeClient();
      await renderModal(client);
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-delete-sonora"));
      expect(screen.getByTestId("delete-project-modal")).toBeInTheDocument();
      expect(screen.queryByTestId("delete-project-guard")).toBeNull();
      fireEvent.click(screen.getByTestId("delete-project-cancel"));
      fireEvent.click(screen.getByTestId("settings-project-delete-ferroloop"));
      expect(screen.getByTestId("delete-project-guard")).toBeInTheDocument();
      const options = within(screen.getByTestId("delete-project-reassign-select"))
        .getAllByRole("option")
        .map((o) => o.getAttribute("value"));
      expect(options).toEqual(["", "sonora", "research-group"]);
    });

    it("cancel, Escape and backdrop send nothing", async () => {
      const { client, calls } = makeClient();
      await renderModal(client);
      await openProjectsTab();
      const closers: Array<() => void> = [
        () => fireEvent.keyDown(window, { key: "Escape" }),
        () => fireEvent.click(screen.getByTestId("delete-project-cancel")),
        () => fireEvent.click(screen.getByTestId("delete-project-modal-backdrop")),
      ];
      for (const close of closers) {
        fireEvent.click(screen.getByTestId("settings-project-delete-sonora"));
        close();
        expect(screen.queryByTestId("delete-project-modal")).toBeNull();
      }
      expect(calls.filter((c) => c.method === "DELETE")).toHaveLength(0);
      expect(screen.getByTestId("settings-project-card-sonora")).toBeInTheDocument();
    });

    it("removes the card, shows a summary, calls onProjectsChanged once and focuses Create", async () => {
      const onProjectsChanged = vi.fn();
      const { client, calls } = makeClient(
        [PLAIN, GIT, GROUP],
        deleteHandler("ferroloop", () => json(FULL)),
      );
      await renderModal(client, { onProjectsChanged });
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-delete-ferroloop"));
      fireEvent.click(screen.getByTestId("delete-project-reassign-radio"));
      fireEvent.change(screen.getByTestId("delete-project-reassign-select"), {
        target: { value: "sonora" },
      });
      fireEvent.click(screen.getByTestId("delete-project-delete-store"));
      await act(async () => {
        fireEvent.click(screen.getByTestId("delete-project-submit"));
      });
      expect(screen.queryByTestId("delete-project-modal")).toBeNull();
      expect(screen.queryByTestId("settings-project-card-ferroloop")).toBeNull();
      expect(screen.getByTestId("settings-project-card-sonora")).toBeInTheDocument();
      expect(
        calls.find((c) => c.method === "DELETE")?.body,
      ).toEqual({ reassign_to: "sonora", delete_store: true });
      expect(onProjectsChanged).toHaveBeenCalledTimes(1);
      expect(screen.getByTestId("settings-projects-delete-status")).toHaveTextContent(
        "Deleted FerroLoop. Reassigned 3 agents. Deleted the local FerroStore database.",
      );
      // The Delete button went away with its card.
      expect(screen.getByTestId("settings-projects-create-btn")).toHaveFocus();
    });

    it("resets the Agents filter to all when it pointed at the deleted project", async () => {
      const { client } = makeClient(
        [PLAIN, GIT, GROUP],
        deleteHandler("sonora", () =>
          json({ ...FULL, deleted: "sonora", reassigned_agents: 0, store_deleted: false }),
        ),
      );
      await renderModal(client);
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-view-agents-sonora"));
      expect(screen.getByTestId("settings-project-filter-select")).toHaveValue("Sonora");
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-delete-sonora"));
      await act(async () => {
        fireEvent.click(screen.getByTestId("delete-project-submit"));
      });
      fireEvent.click(screen.getByTestId("settings-tab-agents"));
      expect(screen.getByTestId("settings-project-filter-select")).toHaveValue("all");
    });

    it("leaves the Agents filter alone when it pointed at another project", async () => {
      const { client } = makeClient(
        [PLAIN, GIT, GROUP],
        deleteHandler("sonora", () => json({ ...FULL, deleted: "sonora" })),
      );
      await renderModal(client);
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-view-agents-research-group"));
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-delete-sonora"));
      await act(async () => {
        fireEvent.click(screen.getByTestId("delete-project-submit"));
      });
      fireEvent.click(screen.getByTestId("settings-tab-agents"));
      expect(screen.getByTestId("settings-project-filter-select")).toHaveValue(
        "research-group",
      );
    });

    it("a 409 shows the active agents and reveals the guard on a stale count", async () => {
      const onProjectsChanged = vi.fn();
      const { client } = makeClient(
        [PLAIN, GIT, GROUP],
        deleteHandler("sonora", () =>
          json(
            { error: "active agents", active_agent_count: 2, active_agents: ["ada", "grace"] },
            409,
          ),
        ),
      );
      await renderModal(client, { onProjectsChanged });
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-delete-sonora"));
      expect(screen.queryByTestId("delete-project-guard")).toBeNull();
      await act(async () => {
        fireEvent.click(screen.getByTestId("delete-project-submit"));
      });
      expect(screen.getByTestId("delete-project-error")).toHaveTextContent(
        /2 active agents.*ada, grace/,
      );
      expect(screen.getByTestId("delete-project-guard")).toBeInTheDocument();
      expect(screen.getByTestId("delete-project-submit")).toBeDisabled();
      expect(screen.getByTestId("settings-project-card-sonora")).toBeInTheDocument();
      expect(onProjectsChanged).not.toHaveBeenCalled();
    });

    it("keeps the dialog open on any other error", async () => {
      const onProjectsChanged = vi.fn();
      const { client } = makeClient(
        [PLAIN],
        deleteHandler("sonora", () => json({ error: "store is locked" }, 500)),
      );
      await renderModal(client, { onProjectsChanged });
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-delete-sonora"));
      fireEvent.click(screen.getByTestId("delete-project-delete-store"));
      await act(async () => {
        fireEvent.click(screen.getByTestId("delete-project-submit"));
      });
      expect(screen.getByTestId("delete-project-error")).toHaveTextContent(
        /store is locked/,
      );
      expect(screen.getByTestId("delete-project-delete-store")).toBeChecked();
      expect(screen.getByTestId("settings-project-card-sonora")).toBeInTheDocument();
      expect(onProjectsChanged).not.toHaveBeenCalled();
    });

    it("returns focus to the Delete button after each way of closing", async () => {
      const closers: Array<() => void> = [
        () => fireEvent.keyDown(window, { key: "Escape" }),
        () => fireEvent.click(screen.getByTestId("delete-project-cancel")),
        () => fireEvent.click(screen.getByTestId("delete-project-modal-backdrop")),
      ];
      for (const close of closers) {
        const { client } = makeClient();
        const view = await renderModal(client);
        await openProjectsTab();
        const btn = screen.getByTestId("settings-project-delete-sonora");
        expect(btn).not.toHaveFocus();
        fireEvent.click(btn);
        close();
        expect(screen.queryByTestId("delete-project-modal")).toBeNull();
        expect(btn).toHaveFocus();
        view.unmount();
      }
    });

    it("Escape closes only the Delete dialog, then Settings", async () => {
      const onClose = vi.fn();
      const { client } = makeClient();
      await renderModal(client, { onClose });
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-delete-sonora"));
      fireEvent.keyDown(window, { key: "Escape" });
      expect(screen.queryByTestId("delete-project-modal")).toBeNull();
      expect(onClose).not.toHaveBeenCalled();
      fireEvent.keyDown(window, { key: "Escape" });
      expect(onClose).toHaveBeenCalledTimes(1);
    });

    it("does not reopen when Settings closes and reopens", async () => {
      const { client } = makeClient();
      const view = await renderModal(client);
      await openProjectsTab();
      fireEvent.click(screen.getByTestId("settings-project-delete-sonora"));
      for (const isOpen of [false, true]) {
        view.rerender(
          <SettingsModal
            isOpen={isOpen}
            onClose={vi.fn()}
            authority={{ tier: "owner", login: "lmcfarlin" }}
            apiClient={client}
          />,
        );
      }
      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(screen.queryByTestId("delete-project-modal")).toBeNull();
    });
  });
});
