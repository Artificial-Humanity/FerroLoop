import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import "@testing-library/jest-dom/vitest";
import { HubApiClient } from "../api/client";
import { AgentSettingsModal } from "../components/AgentSettingsModal";
import { SettingsModal } from "../components/SettingsModal";
import type { AgentRecord, ProjectSummary } from "../types/api";

function summary(slug: string, name: string): ProjectSummary {
  return {
    id: slug,
    slug,
    name,
    kind: "project",
    path: `/work/${slug}`,
    has_git: false,
    purpose: null,
    tracker: null,
    store_path: null,
    active_agent_count: 0,
  };
}

const FELIX = summary("felix-ui", "Felix UI");
const SONORA = summary("sonora", "Sonora");

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function makeClient() {
  const calls: { url: string; method: string; body: any }[] = [];
  const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const method = init?.method ?? "GET";
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    calls.push({ url, method, body });
    return json({});
  });
  const client = new HubApiClient({
    baseUrl: "http://hub.test",
    fetchImpl: fetchImpl as unknown as typeof fetch,
  });
  return { client, calls };
}

const recastCall = (calls: { url: string; method: string; body: any }[]) =>
  calls.find((c) => c.url.endsWith("/recast"));

function agent(project: string): AgentRecord {
  return {
    id: "a1",
    name_key: "ferris",
    display_name: "Ferris",
    project,
    role: "assistant",
    status: "active",
  };
}

const optionsOf = (select: HTMLElement) =>
  within(select)
    .getAllByRole("option")
    .map((o) => [(o as HTMLOptionElement).value, o.textContent]);

async function flush() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

describe("SettingsModal Re-Cast project slugs", () => {
  async function openRecast(project: string) {
    const { client, calls } = makeClient();
    render(
      <SettingsModal
        isOpen={true}
        onClose={vi.fn()}
        authority={{ tier: "owner", login: "lmcfarlin" }}
        apiClient={client}
        agents={[agent(project)]}
        projects={[FELIX, SONORA]}
      />,
    );
    await flush();
    fireEvent.click(screen.getByTestId("recast-agent-btn-ferris"));
    return { calls, select: screen.getByTestId("recast-project-select-ferris") };
  }

  it("uses the slug as the option value and the name as its text", async () => {
    const { select } = await openRecast("felix-ui");
    expect(optionsOf(select)).toContainEqual(["felix-ui", "Felix UI"]);
    expect(optionsOf(select)).toContainEqual(["sonora", "Sonora"]);
    expect(select).toHaveValue("felix-ui");
  });

  it("sends the slug of the chosen project", async () => {
    const { calls, select } = await openRecast("sonora");
    expect(select).toHaveValue("sonora");
    fireEvent.change(select, { target: { value: "felix-ui" } });
    await act(async () => {
      fireEvent.click(screen.getByTestId("recast-submit-btn-ferris"));
    });
    expect(recastCall(calls)?.body.project).toBe("felix-ui");
  });

  it("sends the current project slug when it is left alone", async () => {
    const { calls } = await openRecast("felix-ui");
    await act(async () => {
      fireEvent.click(screen.getByTestId("recast-submit-btn-ferris"));
    });
    expect(recastCall(calls)?.body.project).toBe("felix-ui");
  });

  it("still shows and sends a legacy project that is not in the list", async () => {
    const { calls, select } = await openRecast("old-lab");
    expect(optionsOf(select)).toContainEqual(["old-lab", "old-lab"]);
    expect(select).toHaveValue("old-lab");
    await act(async () => {
      fireEvent.click(screen.getByTestId("recast-submit-btn-ferris"));
    });
    expect(recastCall(calls)?.body.project).toBe("old-lab");
  });
});

describe("AgentSettingsModal Re-Cast project slugs", () => {
  async function openRecast(project: string) {
    const { client, calls } = makeClient();
    render(
      <AgentSettingsModal
        isOpen={true}
        onClose={() => {}}
        agentName="ferris"
        agentDisplayName="Ferris"
        agentProject={project}
        agentRole="assistant"
        sessionId="sess-1"
        adapter="claude"
        selectedModel="claude-opus-5-5"
        selectedThinkingLevel="high"
        onChangeModelAndThinking={() => {}}
        contextEstimate={{ usedTokens: 1000, maxTokens: 200000, percent: 1 }}
        canCompact={false}
        compacting={false}
        onCompactContext={() => {}}
        canClear={true}
        clearing={false}
        onOpenClearModal={() => {}}
        canDecommission={true}
        decommissioning={false}
        onOpenDecommissionModal={() => {}}
        apiClient={client}
        projects={[FELIX, SONORA]}
      />,
    );
    await flush();
    fireEvent.click(screen.getByTestId("recast-agent-btn-ferris"));
    return { calls, select: screen.getByTestId("recast-project-select-ferris") };
  }

  it("uses the slug as the option value and the name as its text", async () => {
    const { select } = await openRecast("felix-ui");
    expect(optionsOf(select)).toEqual([
      ["felix-ui", "Felix UI"],
      ["sonora", "Sonora"],
    ]);
    expect(select).toHaveValue("felix-ui");
  });

  it("sends the slug of the chosen project", async () => {
    const { calls, select } = await openRecast("sonora");
    fireEvent.change(select, { target: { value: "felix-ui" } });
    await act(async () => {
      fireEvent.click(screen.getByTestId("recast-submit-btn-ferris"));
    });
    expect(recastCall(calls)?.body.project).toBe("felix-ui");
  });

  it("still shows and sends a legacy project that is not in the list", async () => {
    const { calls, select } = await openRecast("old-lab");
    expect(optionsOf(select)[0]).toEqual(["old-lab", "old-lab"]);
    expect(select).toHaveValue("old-lab");
    await act(async () => {
      fireEvent.click(screen.getByTestId("recast-submit-btn-ferris"));
    });
    expect(recastCall(calls)?.body.project).toBe("old-lab");
  });
});

describe("Projects tab load state in shared mode", () => {
  function renderShared(
    props: Partial<React.ComponentProps<typeof SettingsModal>>,
  ) {
    const { client } = makeClient();
    const base: React.ComponentProps<typeof SettingsModal> = {
      isOpen: true,
      onClose: vi.fn(),
      authority: { tier: "owner", login: "lmcfarlin" },
      apiClient: client,
      refreshProjects: vi.fn(async () => {}),
      ...props,
    };
    const view = render(<SettingsModal {...base} />);
    return { view, base };
  }

  it("shows the loading state until the shared list has loaded", async () => {
    const { view, base } = renderShared({
      projects: [],
      projectsFromHub: false,
      projectsLoaded: false,
    });
    await flush();
    fireEvent.click(screen.getByTestId("settings-tab-projects"));
    expect(screen.getByTestId("settings-projects-loading")).toBeInTheDocument();
    expect(screen.queryByTestId("settings-projects-empty")).toBeNull();

    view.rerender(
      <SettingsModal
        {...base}
        projects={[FELIX, SONORA]}
        projectsFromHub={true}
        projectsLoaded={true}
      />,
    );
    await flush();
    expect(screen.queryByTestId("settings-projects-loading")).toBeNull();
    expect(screen.getByTestId("settings-project-card-felix-ui")).toBeInTheDocument();
    expect(screen.getByTestId("settings-project-card-sonora")).toBeInTheDocument();
  });

  it("shows the load error instead of the empty state", async () => {
    renderShared({
      projects: [],
      projectsFromHub: false,
      projectsLoaded: true,
      projectsError: "boom",
    });
    await flush();
    fireEvent.click(screen.getByTestId("settings-tab-projects"));
    expect(screen.getByTestId("settings-projects-error")).toHaveTextContent(
      "Could not load projects: boom",
    );
    expect(screen.queryByTestId("settings-projects-empty")).toBeNull();
    expect(screen.queryByTestId("settings-projects-loading")).toBeNull();
  });
});
