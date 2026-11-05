import { act, renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HubApiError } from "../api/client";
import {
  FALLBACK_PROJECTS,
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

const FALLBACK_SLUGS = FALLBACK_PROJECTS.map((p) => p.slug);

describe("useProjects", () => {
  it("starts on the fallback list and loads the hub list once on mount", async () => {
    const listProjects = vi.fn(async () => [project("alpha", "Alpha"), project("council", "Council", "virtual")]);
    const { result, rerender } = renderHook(() => useProjects({ listProjects }));
    expect(result.current.projects.map((p) => p.slug)).toEqual(FALLBACK_SLUGS);
    expect(result.current.loaded).toBe(false);
    expect(result.current.fromHub).toBe(false);

    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.projects.map((p) => p.slug)).toEqual(["alpha", "council"]);
    expect(result.current.error).toBeNull();
    expect(result.current.fromHub).toBe(true);

    rerender();
    expect(listProjects).toHaveBeenCalledTimes(1);
  });

  it("falls back to the built-in list and reports the error when the request fails", async () => {
    const listProjects = vi.fn(async () => {
      throw new Error("hub unreachable");
    });
    const { result } = renderHook(() => useProjects({ listProjects }));
    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.projects.map((p) => p.slug)).toEqual(FALLBACK_SLUGS);
    expect(result.current.error).toBe("hub unreachable");
    expect(result.current.fromHub).toBe(false);
  });

  it("falls back quietly on a 404 from an older hub", async () => {
    const listProjects = vi.fn(async () => {
      throw new HubApiError(404, "Not Found");
    });
    const { result } = renderHook(() => useProjects({ listProjects }));
    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.projects.map((p) => p.slug)).toEqual(FALLBACK_SLUGS);
    expect(result.current.error).toBeNull();
  });

  it("falls back when the hub answers with an empty list", async () => {
    const { result } = renderHook(() => useProjects({ listProjects: async () => [] }));
    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.projects.map((p) => p.slug)).toEqual(FALLBACK_SLUGS);
    expect(result.current.fromHub).toBe(false);
  });

  it("falls back when the client has no listProjects method or there is no client", async () => {
    const missing = renderHook(() => useProjects({}));
    await waitFor(() => expect(missing.result.current.loaded).toBe(true));
    expect(missing.result.current.projects.map((p) => p.slug)).toEqual(FALLBACK_SLUGS);
    expect(missing.result.current.error).toBeNull();

    const none = renderHook(() => useProjects(undefined));
    await waitFor(() => expect(none.result.current.loaded).toBe(true));
    expect(none.result.current.projects.map((p) => p.slug)).toEqual(FALLBACK_SLUGS);
  });

  it("refresh() swaps in the new list", async () => {
    let current = [project("alpha")];
    const listProjects = vi.fn(async () => current);
    const { result } = renderHook(() => useProjects({ listProjects }));
    await waitFor(() => expect(result.current.projects.map((p) => p.slug)).toEqual(["alpha"]));

    current = [project("alpha"), project("council", "Council", "virtual")];
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.projects.map((p) => p.slug)).toEqual(["alpha", "council"]);
    expect(listProjects).toHaveBeenCalledTimes(2);
  });

  it("keeps the last hub list when a refresh fails", async () => {
    let fail = false;
    const listProjects = vi.fn(async () => {
      if (fail) throw new Error("boom");
      return [project("alpha")];
    });
    const { result } = renderHook(() => useProjects({ listProjects }));
    await waitFor(() => expect(result.current.fromHub).toBe(true));
    fail = true;
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.projects.map((p) => p.slug)).toEqual(["alpha"]);
    expect(result.current.error).toBe("boom");
    expect(result.current.fromHub).toBe(true);
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
});
