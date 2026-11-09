import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import { SpinUpAgentModal } from "../components/SpinUpAgentModal";
import {
  AgentSettingsModal,
  type AgentSettingsModalProps,
} from "../components/AgentSettingsModal";
import {
  SettingsModal,
  canonicalizeDefaultRole,
  CREATE_CUSTOM_ROLE_VALUE,
  normalizeRecastRole,
} from "../components/SettingsModal";

const chair = {
  slug: "council-chair",
  label: "Council Chair",
  description: "Synthesize evidence",
  builtin: false,
  review_policy: "lead-review" as const,
  prompt_template: "Offer bounded advice.",
};
const reviewer = { ...chair, slug: "reviewer", label: "Evidence Reviewer" };
const agent = {
  id: "ag-felix",
  name_key: "felix",
  display_name: "Felix",
  project: "FerroLoop",
  role: "assistant",
  status: "idle",
};
const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}
function server(
  options: {
    get?: () => Promise<Response>;
    save?: (body: typeof chair) => Promise<Response>;
    remove?: () => Promise<Response>;
  } = {},
) {
  const fetchImpl = vi.fn(
    async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input),
        method = init?.method ?? "GET";
      if (url.endsWith("/api/v1/roles")) {
        if (method === "POST")
          return options.save
            ? options.save(JSON.parse(String(init?.body)))
            : json(JSON.parse(String(init?.body)));
        return options.get ? options.get() : json([chair]);
      }
      if (url.endsWith("/api/v1/roles/council-chair") && method === "DELETE")
        return options.remove
          ? options.remove()
          : json({ deleted: true, slug: "council-chair" });
      if (url.endsWith("/recast"))
        return json({
          agent: { ...agent, ...JSON.parse(String(init?.body)) },
          guidance_requires_reforge: true,
        });
      if (url.endsWith("/sessions")) return json({ sessions: [] });
      if (url.endsWith("/agents")) return json({ agents: [agent] });
      return json({});
    },
  );
  const client = new HubApiClient({ fetchImpl: fetchImpl as typeof fetch });
  client.setToken("catalog-token");
  return { client, fetchImpl };
}
function agentProps(
  apiClient: HubApiClient,
  role = "assistant",
): AgentSettingsModalProps {
  return {
    isOpen: true,
    onClose: vi.fn(),
    agentName: "felix",
    agentDisplayName: "Felix",
    agentRole: role,
    agentProject: "FerroLoop",
    adapter: "claude",
    sessionId: "sess-felix",
    selectedModel: "",
    selectedThinkingLevel: "medium",
    onChangeModelAndThinking: vi.fn(),
    contextEstimate: { usedTokens: 0, maxTokens: 200000, percent: 0 },
    canCompact: false,
    compacting: false,
    onCompactContext: vi.fn(),
    canClear: true,
    clearing: false,
    onOpenClearModal: vi.fn(),
    canDecommission: true,
    decommissioning: false,
    onOpenDecommissionModal: vi.fn(),
    apiClient,
  };
}
const fillDraft = (slug = "council-chair") => {
  fireEvent.change(screen.getByTestId("role-registry-slug-input"), {
    target: { value: slug },
  });
  fireEvent.change(screen.getByTestId("role-registry-label-input"), {
    target: { value: "Draft Chair" },
  });
  fireEvent.change(screen.getByTestId("role-registry-prompt-input"), {
    target: { value: "My draft guidance" },
  });
};
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe("authenticated editable role catalog", () => {
  it("uses bearer authentication and actual array/record responses for catalog reads and saves", async () => {
    const { client, fetchImpl } = server({
      save: async (body) =>
        json({ ...body, label: "Saved label", builtin: false }),
    });
    expect(await client.getRoles()).toEqual([chair]);
    expect(await client.upsertRole(chair)).toEqual({
      ...chair,
      label: "Saved label",
    });
    for (const [, init] of fetchImpl.mock.calls)
      expect(new Headers(init?.headers).get("Authorization")).toBe(
        "Bearer catalog-token",
      );
    expect(fetchImpl.mock.calls[1][1]?.method).toBe("POST");
    expect(JSON.parse(String(fetchImpl.mock.calls[1][1]?.body))).toEqual(chair);
  });

  it("creates with a neutral catalog Chair choice and retains executable legacy choices", async () => {
    const { client } = server();
    const onSpinUp = vi.fn().mockResolvedValue({});
    render(
      <SpinUpAgentModal
        isOpen
        onClose={vi.fn()}
        apiClient={client}
        projects={[{ id: "ferroloop", slug: "ferroloop", name: "FerroLoop", kind: "project", path: "/work/ferroloop", has_git: true, purpose: null, tracker: null, store_path: null, active_agent_count: 0 }]}
        onSpinUp={onSpinUp}
      />,
    );
    const option = await screen.findByRole("option", { name: "Council Chair" });
    expect(option).toHaveValue("custom:council-chair");
    const select = screen.getByTestId("spin-up-role-select");
    for (const value of [
      "web-developer",
      "mobile-developer",
      "system-developer",
      "data-engineer",
      "tester",
    ])
      expect(
        within(select).getByRole("option", { name: new RegExp(value) }),
      ).toHaveValue(value);
    fireEvent.change(select, { target: { value: "custom:council-chair" } });
    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "Advisor" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));
    await waitFor(() =>
      expect(onSpinUp).toHaveBeenCalledWith(
        expect.objectContaining({ role: "custom:council-chair" }),
      ),
    );
  });

  it.each(["fleet", "agent"])(
    "recasts through the %s control using the catalog ID and requires explicit Reforge",
    async (path) => {
      const { client, fetchImpl } = server();
      if (path === "fleet")
        render(
          <SettingsModal
            isOpen
            onClose={vi.fn()}
            agents={[agent]}
            apiClient={client}
          />,
        );
      else render(<AgentSettingsModal {...agentProps(client)} />);
      fireEvent.click(screen.getByTestId("recast-agent-btn-felix"));
      const select = screen.getByTestId("recast-role-select-felix");
      expect(
        await within(select).findByRole("option", { name: "Council Chair" }),
      ).toHaveValue("custom:council-chair");
      expect(
        within(select).getByRole("option", { name: "web-developer" }),
      ).toHaveValue("web-developer");
      fireEvent.change(select, { target: { value: "custom:council-chair" } });
      fireEvent.click(screen.getByTestId("recast-submit-btn-felix"));
      await waitFor(() =>
        expect(screen.getByTestId("recast-status-banner")).toHaveTextContent(
          /explicit Reforge.*guidance/i,
        ),
      );
      expect(screen.getByTestId("recast-status-banner")).toHaveTextContent(
        "Council Chair",
      );
      const call = fetchImpl.mock.calls.find(([url]) =>
        String(url).endsWith("/recast"),
      );
      expect(JSON.parse(String(call?.[1]?.body)).role).toBe(
        "custom:council-chair",
      );
      expect(
        fetchImpl.mock.calls.some(([url]) => String(url).endsWith("/clear")),
      ).toBe(false);
    },
  );

  it("preserves structural custom identities and user-created lead stays nonstructural", () => {
    for (const value of [
      "custom:lead",
      "custom:assistant",
      "custom:lab_manager",
      "custom:lab-manager",
    ])
      expect(canonicalizeDefaultRole(value)).toBe(value);
    for (const value of ["lead", "assistant", "lab_manager", "lab-manager"])
      expect(normalizeRecastRole(CREATE_CUSTOM_ROLE_VALUE, value)).toBe(
        `custom:${value}`,
      );
  });

  it("keeps a selected legacy choice when the creation catalog arrives late", async () => {
    const read = deferred<Response>();
    const { client } = server({ get: () => read.promise });
    render(<SpinUpAgentModal isOpen onClose={vi.fn()} apiClient={client} />);
    fireEvent.change(screen.getByTestId("spin-up-role-select"), {
      target: { value: "tester" },
    });
    await act(async () => read.resolve(json([chair])));
    expect(screen.getByTestId("spin-up-role-select")).toHaveValue("tester");
    expect(
      screen.getByRole("option", { name: "Council Chair" }),
    ).toBeInTheDocument();
  });

  it.each(["fleet", "agent"])(
    "keeps unknown selected roles on a delayed %s catalog without a Custom prefix",
    async (path) => {
      const read = deferred<Response>();
      const { client } = server({ get: () => read.promise });
      if (path === "fleet")
        render(
          <SettingsModal
            isOpen
            onClose={vi.fn()}
            agents={[{ ...agent, role: "custom:lead" }]}
            apiClient={client}
          />,
        );
      else
        render(<AgentSettingsModal {...agentProps(client, "custom:lead")} />);
      fireEvent.click(screen.getByTestId("recast-agent-btn-felix"));
      const select = screen.getByTestId("recast-role-select-felix");
      expect(select).toHaveValue("custom:lead");
      await act(async () =>
        read.resolve(
          json([
            chair,
            {
              ...chair,
              slug: "reviewer",
              label: "Project Lead",
              builtin: true,
            },
            { ...chair, slug: "lead", label: "Workspace Lead", builtin: true },
          ]),
        ),
      );
      expect(select).toHaveValue("custom:lead");
      expect(
        within(select).getByRole("option", { name: "Project Lead" }),
      ).toHaveValue("custom:reviewer");
      expect(
        within(select).getByRole("option", {
          name: "Workspace Lead",
          selected: true,
        }),
      ).toHaveValue("custom:lead");
    },
  );

  it("shows server catalog data and keeps drafts and no saved success on refusal", async () => {
    const { client } = server({
      save: async () => json({ error: "Owner required" }, 403),
    });
    render(<SettingsModal isOpen onClose={vi.fn()} apiClient={client} />);
    await screen.findByTestId("role-registry-row-council-chair");
    expect(
      screen.queryByTestId("role-registry-row-lead"),
    ).not.toBeInTheDocument();
    fireEvent.click(screen.getByTestId("role-registry-edit-btn-council-chair"));
    fillDraft();
    fireEvent.click(screen.getByTestId("role-registry-save-btn"));
    await waitFor(() =>
      expect(screen.getByTestId("role-registry-error")).toHaveTextContent(
        "Owner required",
      ),
    );
    expect(
      screen.queryByTestId("role-registry-status-banner"),
    ).not.toBeInTheDocument();
    expect(screen.getByTestId("role-registry-label-input")).toHaveValue(
      "Draft Chair",
    );
    expect(screen.getByTestId("role-registry-prompt-input")).toHaveValue(
      "My draft guidance",
    );
    expect(
      screen.getByTestId("role-registry-row-council-chair"),
    ).toHaveTextContent("Council Chair");
  });

  it("uses the acknowledged record without replacing edits made while saving", async () => {
    const save = deferred<Response>();
    const { client } = server({ save: () => save.promise });
    render(<SettingsModal isOpen onClose={vi.fn()} apiClient={client} />);
    await screen.findByTestId("role-registry-row-council-chair");
    fillDraft();
    fireEvent.click(screen.getByTestId("role-registry-save-btn"));
    expect(
      screen.queryByTestId("role-registry-status-banner"),
    ).not.toBeInTheDocument();
    fireEvent.change(screen.getByTestId("role-registry-label-input"), {
      target: { value: "Newer draft" },
    });
    await act(async () =>
      save.resolve(json({ ...chair, label: "Server acknowledged" })),
    );
    expect(
      screen.getByTestId("role-registry-row-council-chair"),
    ).toHaveTextContent("Server acknowledged");
    expect(screen.getByTestId("role-registry-label-input")).toHaveValue(
      "Newer draft",
    );
    expect(screen.getByTestId("role-registry-status-banner")).toHaveTextContent(
      "Saved",
    );
  });

  it("does not let a late catalog read overwrite a saved record or an editor draft", async () => {
    const read = deferred<Response>();
    const { client } = server({ get: () => read.promise });
    render(<SettingsModal isOpen onClose={vi.fn()} apiClient={client} />);
    fillDraft();
    fireEvent.click(screen.getByTestId("role-registry-save-btn"));
    await screen.findByTestId("role-registry-row-council-chair");
    await act(async () => read.resolve(json([chair])));
    expect(
      screen.getByTestId("role-registry-row-council-chair"),
    ).toHaveTextContent("Draft Chair");
    expect(screen.getByTestId("role-registry-label-input")).toHaveValue(
      "Draft Chair",
    );
  });

  it.each(["before", "after"])(
    "shows initial catalog failure %s save acknowledgement while retaining feedback and drafts",
    async (order) => {
      const read = deferred<Response>();
      const save = deferred<Response>();
      const { client } = server({ get: () => read.promise, save: () => save.promise });
      render(<SettingsModal isOpen onClose={vi.fn()} apiClient={client} />);
      fillDraft();
      fireEvent.click(screen.getByTestId("role-registry-save-btn"));
      fireEvent.change(screen.getByTestId("role-registry-label-input"), {
        target: { value: "Newer unsaved draft" },
      });
      const failLoad = () => act(async () => read.resolve(json({ error: "Catalog unavailable" }, 503)));
      if (order === "before") await failLoad();
      await act(async () => save.resolve(json({ ...chair, label: "Server acknowledged" })));
      if (order === "after") {
        expect(screen.getByText("Loading roles…")).toBeInTheDocument();
        await failLoad();
      }
      expect(screen.getByTestId("role-registry-error")).toHaveTextContent("Catalog unavailable");
      expect(screen.getByTestId("role-registry-status-banner")).toHaveTextContent("Saved");
      expect(screen.getByTestId("role-registry-row-council-chair")).toHaveTextContent("Server acknowledged");
      expect(screen.getByTestId("role-registry-label-input")).toHaveValue("Newer unsaved draft");
      expect(screen.getByTestId("role-registry-prompt-input")).toHaveValue("My draft guidance");
      expect(screen.queryByText("Loading roles…")).not.toBeInTheDocument();
    },
  );

  it.each(["before", "after"])(
    "retains both catalog failure and mutation refusal when load fails %s save",
    async (order) => {
      const read = deferred<Response>();
      const save = deferred<Response>();
      const { client } = server({ get: () => read.promise, save: () => save.promise });
      render(<SettingsModal isOpen onClose={vi.fn()} apiClient={client} />);
      fillDraft();
      const failLoad = () => act(async () => read.resolve(json({ error: "Catalog unavailable" }, 503)));
      if (order === "before") await failLoad();
      fireEvent.click(screen.getByTestId("role-registry-save-btn"));
      await act(async () => save.resolve(json({ error: "Owner required" }, 403)));
      if (order === "after") await failLoad();
      expect(screen.getByTestId("role-registry-error")).toHaveTextContent("Catalog unavailable");
      expect(screen.getByTestId("role-registry-error")).toHaveTextContent("Owner required");
      expect(screen.queryByTestId("role-registry-status-banner")).not.toBeInTheDocument();
      expect(screen.getByTestId("role-registry-label-input")).toHaveValue("Draft Chair");
    },
  );

  it("ignores an initial catalog failure from a closed editor session", async () => {
    const read = deferred<Response>();
    let reads = 0;
    const { client } = server({ get: () => ++reads === 1 ? read.promise : Promise.resolve(json([chair])) });
    const onClose = vi.fn();
    const { rerender } = render(<SettingsModal isOpen onClose={onClose} apiClient={client} />);
    rerender(<SettingsModal isOpen={false} onClose={onClose} apiClient={client} />);
    rerender(<SettingsModal isOpen onClose={onClose} apiClient={client} />);
    await screen.findByTestId("role-registry-row-council-chair");
    await act(async () => read.resolve(json({ error: "Old catalog failure" }, 503)));
    expect(screen.queryByTestId("role-registry-error")).not.toBeInTheDocument();
    expect(screen.getByTestId("role-registry-row-council-chair")).toHaveTextContent("Council Chair");
  });

  it.each(["before", "after"])(
    "retains unrelated catalog rows when the initial read resolves %s a save acknowledgement",
    async (order) => {
      const read = deferred<Response>();
      const save = deferred<Response>();
      const { client } = server({
        get: () => read.promise,
        save: () => save.promise,
      });
      render(<SettingsModal isOpen onClose={vi.fn()} apiClient={client} />);
      fillDraft();
      fireEvent.click(screen.getByTestId("role-registry-save-btn"));
      fireEvent.change(screen.getByTestId("role-registry-label-input"), {
        target: { value: "Newer unsaved draft" },
      });
      const load = () => act(async () => read.resolve(json([chair, reviewer])));
      if (order === "before") await load();
      await act(async () =>
        save.resolve(json({ ...chair, label: "Server acknowledged" })),
      );
      if (order === "after") await load();
      expect(screen.getByTestId("role-registry-row-reviewer")).toHaveTextContent(
        "Evidence Reviewer",
      );
      expect(screen.getByTestId("role-registry-row-council-chair")).toHaveTextContent(
        "Server acknowledged",
      );
      expect(screen.getByTestId("role-registry-count-badge")).toHaveTextContent("2");
      expect(screen.getByTestId("role-registry-label-input")).toHaveValue(
        "Newer unsaved draft",
      );
      expect(screen.getByTestId("role-registry-prompt-input")).toHaveValue(
        "My draft guidance",
      );
      expect(screen.getByTestId("role-registry-status-banner")).toHaveTextContent("Saved");
      expect(screen.queryByTestId("role-registry-error")).not.toBeInTheDocument();
    },
  );

  it.each(["before", "after"])(
    "retains the catalog and refusal when the initial read resolves %s a failed save",
    async (order) => {
      const read = deferred<Response>();
      const save = deferred<Response>();
      const { client } = server({
        get: () => read.promise,
        save: () => save.promise,
      });
      render(<SettingsModal isOpen onClose={vi.fn()} apiClient={client} />);
      fillDraft();
      fireEvent.click(screen.getByTestId("role-registry-save-btn"));
      const load = () => act(async () => read.resolve(json([chair, reviewer])));
      if (order === "before") await load();
      await act(async () => save.resolve(json({ error: "Owner required" }, 403)));
      if (order === "after") await load();
      expect(screen.getByTestId("role-registry-row-reviewer")).toHaveTextContent(
        "Evidence Reviewer",
      );
      expect(screen.getByTestId("role-registry-row-council-chair")).toHaveTextContent(
        "Council Chair",
      );
      expect(screen.getByTestId("role-registry-count-badge")).toHaveTextContent("2");
      expect(screen.getByTestId("role-registry-label-input")).toHaveValue("Draft Chair");
      expect(screen.getByTestId("role-registry-prompt-input")).toHaveValue(
        "My draft guidance",
      );
      expect(screen.getByTestId("role-registry-error")).toHaveTextContent("Owner required");
      expect(screen.queryByTestId("role-registry-status-banner")).not.toBeInTheDocument();
    },
  );

  it.each(["before", "after"])(
    "keeps a deletion without losing unrelated rows when the initial read resolves %s delete acknowledgement",
    async (order) => {
      const read = deferred<Response>();
      const remove = deferred<Response>();
      const { client } = server({
        get: () => read.promise,
        remove: () => remove.promise,
      });
      render(<SettingsModal isOpen onClose={vi.fn()} apiClient={client} />);
      fillDraft();
      fireEvent.click(screen.getByTestId("role-registry-save-btn"));
      await screen.findByTestId("role-registry-row-council-chair");
      fireEvent.click(screen.getByTestId("role-registry-delete-btn-council-chair"));
      const load = () => act(async () => read.resolve(json([chair, reviewer])));
      if (order === "before") {
        await load();
        expect(screen.getByTestId("role-registry-row-council-chair")).toHaveTextContent(
          "Draft Chair",
        );
      }
      await act(async () =>
        remove.resolve(json({ deleted: true, slug: "council-chair" })),
      );
      if (order === "after") await load();
      expect(screen.queryByTestId("role-registry-row-council-chair")).not.toBeInTheDocument();
      expect(screen.getByTestId("role-registry-row-reviewer")).toHaveTextContent(
        "Evidence Reviewer",
      );
      expect(screen.getByTestId("role-registry-count-badge")).toHaveTextContent("1");
      expect(screen.getByTestId("role-registry-status-banner")).toHaveTextContent("Removed");
      expect(screen.queryByTestId("role-registry-error")).not.toBeInTheDocument();
    },
  );

  it("ignores a save response from a closed editor session", async () => {
    const save = deferred<Response>();
    const { client } = server({ save: () => save.promise });
    const onClose = vi.fn();
    const { rerender } = render(
      <SettingsModal isOpen onClose={onClose} apiClient={client} />,
    );
    await screen.findByTestId("role-registry-row-council-chair");
    fillDraft();
    fireEvent.click(screen.getByTestId("role-registry-save-btn"));
    rerender(
      <SettingsModal isOpen={false} onClose={onClose} apiClient={client} />,
    );
    rerender(<SettingsModal isOpen onClose={onClose} apiClient={client} />);
    await screen.findByTestId("role-registry-row-council-chair");
    await act(async () =>
      save.resolve(json({ ...chair, label: "Old session" })),
    );
    expect(
      screen.getByTestId("role-registry-row-council-chair"),
    ).toHaveTextContent("Council Chair");
    expect(
      screen.queryByTestId("role-registry-status-banner"),
    ).not.toBeInTheDocument();
  });

  it("removes the optional starter through the authenticated API", async () => {
    const { client, fetchImpl } = server();
    render(<SettingsModal isOpen onClose={vi.fn()} apiClient={client} />);
    await screen.findByTestId("role-registry-row-council-chair");
    fireEvent.click(
      screen.getByTestId("role-registry-delete-btn-council-chair"),
    );
    await waitFor(() =>
      expect(
        screen.queryByTestId("role-registry-row-council-chair"),
      ).not.toBeInTheDocument(),
    );
    const call = fetchImpl.mock.calls.find(
      ([url, init]) =>
        String(url).endsWith("/roles/council-chair") &&
        init?.method === "DELETE",
    );
    expect(new Headers(call?.[1]?.headers).get("Authorization")).toBe(
      "Bearer catalog-token",
    );
  });
});
