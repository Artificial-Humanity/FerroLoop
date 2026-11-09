import { act, renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HubApiError } from "../api/client";
import {
  findProject,
  projectLabel,
  projectLabels,
  useProjects,
} from "../hooks/useProjects";
import type { ProjectSummary } from "../types/api";

function project(slug: string, name = slug, kind: "project" | "virtual" = "project"): ProjectSummary {
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

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

describe("useProjects", () => {
  it("starts with an empty list (never a static fallback) and loads the hub list once on mount", async () => {
    const listProjects = vi.fn(async () => [project("alpha", "Alpha"), project("council", "Council", "virtual")]);
    const { result, rerender } = renderHook(() => useProjects({ listProjects }));
    expect(result.current.projects).toEqual([]);
    expect(result.current.loaded).toBe(false);
    expect(result.current.fromHub).toBe(false);

    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.projects.map((p) => p.slug)).toEqual(["alpha", "council"]);
    expect(result.current.error).toBeNull();
    expect(result.current.fromHub).toBe(true);

    rerender();
    expect(listProjects).toHaveBeenCalledTimes(1);
  });

  it("returns an empty list and reports the error when the initial request fails", async () => {
    const listProjects = vi.fn(async () => {
      throw new Error("hub unreachable");
    });
    const { result } = renderHook(() => useProjects({ listProjects }));
    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.projects).toEqual([]);
    expect(result.current.error).toBe("hub unreachable");
    expect(result.current.fromHub).toBe(false);
  });

  it("returns an empty list quietly on a 404 from an older hub", async () => {
    const listProjects = vi.fn(async () => {
      throw new HubApiError(404, "Not Found");
    });
    const { result } = renderHook(() => useProjects({ listProjects }));
    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.projects).toEqual([]);
    expect(result.current.error).toBeNull();
  });

  it("returns an empty list when the hub answers with an empty list", async () => {
    const { result } = renderHook(() => useProjects({ listProjects: async () => [] }));
    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.projects).toEqual([]);
    expect(result.current.fromHub).toBe(false);
  });

  it("returns an empty list when the client has no listProjects method or there is no client", async () => {
    const missing = renderHook(() => useProjects({}));
    await waitFor(() => expect(missing.result.current.loaded).toBe(true));
    expect(missing.result.current.projects).toEqual([]);
    expect(missing.result.current.error).toBeNull();

    const none = renderHook(() => useProjects(undefined));
    await waitFor(() => expect(none.result.current.loaded).toBe(true));
    expect(none.result.current.projects).toEqual([]);
  });

  it("refresh() swaps in the new list, including after an initial 401", async () => {
    let authed = false;
    const listProjects = vi.fn(async () => {
      if (!authed) throw new HubApiError(401, "Unauthorized");
      return [project("ferroloop", "FerroLoop"), project("the-council", "The Council", "virtual")];
    });
    const { result } = renderHook(() => useProjects({ listProjects }));
    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.projects).toEqual([]);
    expect(result.current.fromHub).toBe(false);

    authed = true;
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.projects.map((p) => p.slug)).toEqual(["ferroloop", "the-council"]);
    expect(result.current.fromHub).toBe(true);
    expect(listProjects).toHaveBeenCalledTimes(2);
  });

  it("keeps the last hub list when a transient non-auth refresh fails, and clears on 401", async () => {
    let mode: "ok" | "500" | "401" = "ok";
    const listProjects = vi.fn(async () => {
      if (mode === "500") throw new Error("boom");
      if (mode === "401") throw new HubApiError(401, "Unauthorized");
      return [project("alpha")];
    });
    const { result } = renderHook(() => useProjects({ listProjects }));
    await waitFor(() => expect(result.current.fromHub).toBe(true));
    mode = "500";
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.projects.map((p) => p.slug)).toEqual(["alpha"]);
    expect(result.current.error).toBe("boom");
    expect(result.current.fromHub).toBe(true);

    mode = "401";
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.projects).toEqual([]);
    expect(result.current.fromHub).toBe(false);
  });

  it("lets the latest of overlapping refreshes win, whatever order they finish in", async () => {
    const first = deferred<ProjectSummary[]>();
    const second = deferred<ProjectSummary[]>();
    const third = deferred<ProjectSummary[]>();
    const queue = [first, second, third];
    const listProjects = vi.fn(() => queue.shift()!.promise);
    const { result } = renderHook(() => useProjects({ listProjects }));
    // The mount load is `first`; start two more refreshes before any settles.
    let p2!: Promise<void>;
    let p3!: Promise<void>;
    act(() => {
      p2 = result.current.refresh();
      p3 = result.current.refresh();
    });
    await act(async () => {
      third.resolve([project("newest")]);
      await p3;
    });
    expect(result.current.projects.map((p) => p.slug)).toEqual(["newest"]);

    await act(async () => {
      second.resolve([project("stale-two")]);
      first.resolve([project("stale-one")]);
      await p2;
    });
    expect(result.current.projects.map((p) => p.slug)).toEqual(["newest"]);
  });

  it("ignores results that arrive after unmount", async () => {
    const pending = deferred<ProjectSummary[]>();
    const errors = vi.spyOn(console, "error").mockImplementation(() => {});
    const { result, unmount } = renderHook(() =>
      useProjects({ listProjects: () => pending.promise }),
    );
    const refresh = result.current.refresh;
    unmount();
    await act(async () => {
      pending.resolve([project("late")]);
      await refresh();
    });
    expect(errors).not.toHaveBeenCalled();
    errors.mockRestore();
  });
});

describe("project helpers", () => {
  it("labels projects by name and falls back to the slug", () => {
    expect(projectLabel(project("council", "Council"))).toBe("Council");
    expect(projectLabel({ slug: "council", name: "  " })).toBe("council");
    expect(projectLabels([project("a", "Alpha"), project("b", "Beta")])).toEqual(["Alpha", "Beta"]);
  });

  it("finds a project by slug or name, case-insensitively", () => {
    const list = [project("felix-ui", "Felix UI"), project("council", "Council", "virtual")];
    expect(findProject(list, "FELIX-UI")?.slug).toBe("felix-ui");
    expect(findProject(list, "council")?.slug).toBe("council");
    expect(findProject(list, " felix ui ")?.slug).toBe("felix-ui");
    expect(findProject(list, "all")).toBeUndefined();
    expect(findProject(list, null)).toBeUndefined();
  });

  describe("projectManagement", () => {
    const legacy = (slug: string): ProjectSummary => ({ ...project(slug), name: slug, legacy: true });

    it("is available before the first load completes and for a new-style reply", async () => {
      const pending = deferred<ProjectSummary[]>();
      const { result } = renderHook(() => useProjects({ listProjects: () => pending.promise }));
      expect(result.current.loaded).toBe(false);
      expect(result.current.projectManagement).toBe("available");
      await act(async () => {
        pending.resolve([project("alpha", "Alpha")]);
      });
      expect(result.current.projectManagement).toBe("available");
      expect(result.current.fromHub).toBe(true);
    });

    it("treats a reply where every entry is legacy as no management support", async () => {
      const listProjects = vi.fn(async () => [legacy("ferroloop"), legacy("sonora")]);
      const { result } = renderHook(() => useProjects({ listProjects }));
      await waitFor(() => expect(result.current.loaded).toBe(true));
      expect(result.current.projectManagement).toBe("unsupported");
      expect(result.current.fromHub).toBe(false);
      expect(result.current.error).toBeNull();
      expect(result.current.projects).toEqual([]);
    });

    it("keeps a mixed reply as a normal list", async () => {
      const listProjects = vi.fn(async () => [legacy("ferroloop"), project("alpha", "Alpha")]);
      const { result } = renderHook(() => useProjects({ listProjects }));
      await waitFor(() => expect(result.current.loaded).toBe(true));
      expect(result.current.projectManagement).toBe("available");
      expect(result.current.fromHub).toBe(true);
    });

    it.each([404, 405])("is unsupported on a %i", async (status) => {
      const listProjects = vi.fn(async () => {
        throw new HubApiError(status, "nope");
      });
      const { result } = renderHook(() => useProjects({ listProjects }));
      await waitFor(() => expect(result.current.loaded).toBe(true));
      expect(result.current.projectManagement).toBe("unsupported");
    });

    it("is unsupported when the client has no listProjects method", async () => {
      const { result } = renderHook(() => useProjects({}));
      await waitFor(() => expect(result.current.loaded).toBe(true));
      expect(result.current.projectManagement).toBe("unsupported");
    });

    it("stays available for an empty reply and for a server error", async () => {
      const empty = renderHook(() => useProjects({ listProjects: async () => [] }));
      await waitFor(() => expect(empty.result.current.loaded).toBe(true));
      expect(empty.result.current.projectManagement).toBe("available");
      const failing = renderHook(() =>
        useProjects({
          listProjects: async () => {
            throw new HubApiError(500, "boom");
          },
        }),
      );
      await waitFor(() => expect(failing.result.current.loaded).toBe(true));
      expect(failing.result.current.projectManagement).toBe("available");
    });
  });
});
