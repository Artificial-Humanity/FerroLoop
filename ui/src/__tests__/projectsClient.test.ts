import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  HubApiClient,
  HubApiError,
  ProjectDeleteConflictError,
} from "../api/client";
import type { ProjectSummary } from "../types/api";

type Call = { url: string; init: RequestInit | undefined };

function makeClient(
  respond: (url: string, init?: RequestInit) => Response,
): { client: HubApiClient; calls: Call[] } {
  const calls: Call[] = [];
  const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    calls.push({ url, init });
    return respond(url, init);
  });
  const client = new HubApiClient({
    baseUrl: "http://hub.test",
    fetchImpl: fetchImpl as unknown as typeof fetch,
  });
  return { client, calls };
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

const FULL: ProjectSummary = {
  id: "ferro",
  slug: "ferro",
  name: "Ferro",
  kind: "project",
  path: "/work/ferro",
  has_git: true,
  purpose: "Core",
  tracker: { kind: "github", repo: "acme/ferro", credential: "app" },
  store_path: "/work/ferro/.ferro/ferro.fls",
  active_agent_count: 2,
  artifacts_dir: "/art/ferro",
  default_artifacts_dir: "/art/default/ferro",
  effective_artifacts_dir: "/art/ferro",
};

describe("HubApiClient projects", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("listProjects GETs /api/v1/projects and returns the entries", async () => {
    const { client, calls } = makeClient(() => json({ projects: [FULL] }));
    const out = await client.listProjects();
    expect(out).toEqual([FULL]);
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe("http://hub.test/api/v1/projects");
    expect(calls[0].init?.method ?? "GET").toBe("GET");
    expect(new Headers(calls[0].init?.headers).get("Accept")).toBe("application/json");
  });

  it("listProjects maps a legacy {id} entry to a full summary", async () => {
    const { client } = makeClient(() =>
      json({
        projects: [
          {
            id: "legacy",
            artifacts_dir: null,
            default_artifacts_dir: "/a",
            effective_artifacts_dir: "/a",
          },
          FULL,
        ],
      }),
    );
    const out = await client.listProjects();
    expect(out).toEqual([
      {
        id: "legacy",
        slug: "legacy",
        name: "legacy",
        kind: "project",
        path: null,
        has_git: false,
        purpose: null,
        tracker: null,
        store_path: null,
        active_agent_count: 0,
        artifacts_dir: null,
        default_artifacts_dir: "/a",
        effective_artifacts_dir: "/a",
        legacy: true,
      },
      FULL,
    ]);
  });

  it("listProjects marks only entries with no slug, name and kind as legacy", async () => {
    const { client } = makeClient(() =>
      json({
        projects: [
          { id: "old", effective_artifacts_dir: "/a/old" },
          { id: "named", name: "Named" },
          FULL,
        ],
      }),
    );
    const out = await client.listProjects();
    expect(out.map((p) => [p.slug, p.legacy === true])).toEqual([
      ["old", true],
      ["named", false],
      ["ferro", false],
    ]);
  });

  it("listProjects round-trips a new-style entry with id and all artifact fields", async () => {
    const { client } = makeClient(() => json({ projects: [FULL] }));
    const [p] = await client.listProjects();
    expect(p.id).toBe("ferro");
    expect(p.artifacts_dir).toBe("/art/ferro");
    expect(p.default_artifacts_dir).toBe("/art/default/ferro");
    expect(p.effective_artifacts_dir).toBe("/art/ferro");
  });

  it("listProjects sets id = slug when the entry has no id and invents no artifact paths", async () => {
    const { id: _id, artifacts_dir: _a, default_artifacts_dir: _d, effective_artifacts_dir: _e, ...bare } = FULL;
    const { client } = makeClient(() => json({ projects: [bare] }));
    const [p] = await client.listProjects();
    expect(p.id).toBe("ferro");
    expect(p.slug).toBe("ferro");
    expect(p.artifacts_dir).toBeUndefined();
    expect(p.default_artifacts_dir).toBeUndefined();
    expect(p.effective_artifacts_dir).toBeUndefined();
  });

  it("listProjects returns [] on a malformed body", async () => {
    for (const body of [{}, { projects: "nope" }, { projects: null }, []]) {
      const { client } = makeClient(() => json(body));
      expect(await client.listProjects()).toEqual([]);
    }
    const { client } = makeClient(
      () => new Response("not json", { status: 200 }),
    );
    expect(await client.listProjects()).toEqual([]);
  });

  it("listProjects skips entries that have neither slug nor id", async () => {
    const { client } = makeClient(() =>
      json({ projects: [null, 7, { name: "x" }, FULL] }),
    );
    expect(await client.listProjects()).toEqual([FULL]);
  });

  it("listProjectsArtifactsDirs keeps returning the raw entries", async () => {
    const raw = {
      id: "p",
      artifacts_dir: null,
      default_artifacts_dir: "/d",
      effective_artifacts_dir: "/d",
    };
    const { client } = makeClient(() => json({ projects: [raw] }));
    expect(await client.listProjectsArtifactsDirs()).toEqual({ projects: [raw] });
  });

  it("createProject POSTs the body as JSON and returns the summary", async () => {
    const req = {
      name: "Ferro",
      kind: "project" as const,
      path: "/work/ferro",
      git_init: false,
    };
    const { client, calls } = makeClient(() => json(FULL, 201));
    const out = await client.createProject(req);
    expect(out).toEqual(FULL);
    expect(calls[0].url).toBe("http://hub.test/api/v1/projects");
    expect(calls[0].init?.method).toBe("POST");
    expect(JSON.parse(String(calls[0].init?.body))).toEqual(req);
    expect(new Headers(calls[0].init?.headers).get("Content-Type")).toBe(
      "application/json",
    );
  });

  it("createProject surfaces the server error text", async () => {
    const { client } = makeClient(() =>
      json({ error: "slug already exists" }, 400),
    );
    await expect(
      client.createProject({ name: "x", kind: "virtual" }),
    ).rejects.toMatchObject({
      name: "HubApiError",
      status: 400,
      message: "slug already exists",
    });
  });

  it("updateProject PATCHes /api/v1/projects/:slug with the body", async () => {
    const { client, calls } = makeClient(() => json(FULL));
    const out = await client.updateProject("ferro", {
      purpose: "New",
      github: "",
    });
    expect(out).toEqual(FULL);
    expect(calls[0].url).toBe("http://hub.test/api/v1/projects/ferro");
    expect(calls[0].init?.method).toBe("PATCH");
    expect(JSON.parse(String(calls[0].init?.body))).toEqual({
      purpose: "New",
      github: "",
    });
    expect(new Headers(calls[0].init?.headers).get("Content-Type")).toBe(
      "application/json",
    );
  });

  it("updateProject surfaces the server error text", async () => {
    const { client } = makeClient(() => json({ error: "no such project" }, 404));
    const err = await client.updateProject("zz", { name: "n" }).catch((e) => e);
    expect(err).toBeInstanceOf(HubApiError);
    expect(err).not.toBeInstanceOf(ProjectDeleteConflictError);
    expect(err.status).toBe(404);
    expect(err.message).toBe("no such project");
  });

  it("deleteProject DELETEs with a JSON body and returns the response", async () => {
    const res = {
      deleted: "ferro",
      reassigned_agents: 2,
      decommissioned_agents: 0,
      store_deleted: true,
      dir_deleted: false,
    };
    const { client, calls } = makeClient(() => json(res));
    const out = await client.deleteProject("ferro", {
      reassign_to: "other",
      delete_store: true,
    });
    expect(out).toEqual(res);
    expect(calls[0].url).toBe("http://hub.test/api/v1/projects/ferro");
    expect(calls[0].init?.method).toBe("DELETE");
    expect(JSON.parse(String(calls[0].init?.body))).toEqual({
      reassign_to: "other",
      delete_store: true,
    });
    expect(new Headers(calls[0].init?.headers).get("Content-Type")).toBe(
      "application/json",
    );
  });

  it("URL-encodes the slug for update and delete", async () => {
    const { client, calls } = makeClient((_u, init) =>
      init?.method === "DELETE"
        ? json({
            deleted: "a/b c",
            reassigned_agents: 0,
            decommissioned_agents: 0,
            store_deleted: false,
            dir_deleted: false,
          })
        : json(FULL),
    );
    await client.updateProject("a/b c", { name: "n" });
    await client.deleteProject("a/b c", {});
    expect(calls[0].url).toBe("http://hub.test/api/v1/projects/a%2Fb%20c");
    expect(calls[1].url).toBe("http://hub.test/api/v1/projects/a%2Fb%20c");
  });

  it("deleteProject maps a 409 to ProjectDeleteConflictError with the active agents", async () => {
    const { client } = makeClient(() =>
      json(
        {
          error: "project has active agents",
          active_agent_count: 2,
          active_agents: ["felix", "feira"],
        },
        409,
      ),
    );
    const err = await client.deleteProject("ferro", {}).catch((e) => e);
    expect(err).toBeInstanceOf(ProjectDeleteConflictError);
    expect(err).toBeInstanceOf(HubApiError);
    expect(err.status).toBe(409);
    expect(err.message).toBe("project has active agents");
    expect(err.active_agent_count).toBe(2);
    expect(err.active_agents).toEqual(["felix", "feira"]);
  });

  it("deleteProject 409 tolerates missing agent fields", async () => {
    const { client } = makeClient(() => json({ error: "conflict" }, 409));
    const err = await client.deleteProject("ferro", {}).catch((e) => e);
    expect(err).toBeInstanceOf(ProjectDeleteConflictError);
    expect(err.active_agent_count).toBe(0);
    expect(err.active_agents).toEqual([]);
  });

  it("deleteProject surfaces other error statuses with the server error text", async () => {
    for (const status of [400, 403, 404, 500]) {
      const { client } = makeClient(() => json({ error: `boom ${status}` }, status));
      const err = await client.deleteProject("ferro", {}).catch((e) => e);
      expect(err).toBeInstanceOf(HubApiError);
      expect(err).not.toBeInstanceOf(ProjectDeleteConflictError);
      expect(err.status).toBe(status);
      expect(err.message).toBe(`boom ${status}`);
    }
  });
});
