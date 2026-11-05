import { useCallback, useEffect, useRef, useState } from "react";
import { HubApiError } from "../api/client";
import type { ProjectSummary } from "../types/api";

/**
 * The project list the app falls back to when the hub cannot serve
 * `GET /api/v1/projects` (an older hub, a 404, a failed request). It mirrors the
 * lists that used to be hard-coded in the roster, Settings and Spin-Up.
 */
const FALLBACK_PROJECT_NAMES = [
  "AI-Lab-AMD",
  "FerroLoop",
  "Kaggle-Gemma4",
  "Lucida",
  "Prosodia",
  "Sonora",
  "Website",
] as const;

function fallbackEntry(name: string): ProjectSummary {
  const slug = name.toLowerCase();
  return {
    id: slug,
    slug,
    name,
    kind: "project",
    path: null,
    has_git: false,
    purpose: null,
    tracker: null,
    store_path: null,
    active_agent_count: 0,
  };
}

export const FALLBACK_PROJECTS: readonly ProjectSummary[] = Object.freeze(
  FALLBACK_PROJECT_NAMES.map(fallbackEntry),
);

/** Display label of a project: its name, or the slug when the name is blank. */
export function projectLabel(project: Pick<ProjectSummary, "name" | "slug">): string {
  return project.name.trim() || project.slug;
}

/** The labels of a project list, for components that take plain option strings. */
export function projectLabels(
  projects: readonly Pick<ProjectSummary, "name" | "slug">[],
): string[] {
  return projects.map(projectLabel);
}

/**
 * Finds the project a free-form reference points at: a slug, a name or a label,
 * compared case-insensitively. Returns `undefined` for anything else.
 */
export function findProject<T extends Pick<ProjectSummary, "name" | "slug">>(
  projects: readonly T[],
  raw: string | null | undefined,
): T | undefined {
  const key = (raw ?? "").trim().toLowerCase();
  if (!key) return undefined;
  return (
    projects.find((p) => p.slug.trim().toLowerCase() === key) ??
    projects.find((p) => projectLabel(p).toLowerCase() === key)
  );
}

export interface ProjectsState {
  projects: ProjectSummary[];
  /** True once the first load has settled, whatever its outcome. */
  loaded: boolean;
  /** Message of the last failed load (not set when the endpoint is merely absent). */
  error: string | null;
  /** True when `projects` came from the hub, false for the built-in fallback. */
  fromHub: boolean;
  /** Reloads the list. Overlapping calls resolve in order; the latest one wins. */
  refresh: () => Promise<void>;
}

interface ProjectsClient {
  listProjects?: () => Promise<ProjectSummary[]>;
}

function isEndpointMissing(err: unknown): boolean {
  return err instanceof HubApiError && (err.status === 404 || err.status === 405);
}

/**
 * One project list for the whole app. Loads `listProjects()` on mount and on
 * `refresh()`; falls back to the built-in list when the hub does not offer it.
 */
export function useProjects(apiClient: ProjectsClient | undefined): ProjectsState {
  const [state, setState] = useState<Omit<ProjectsState, "refresh">>({
    projects: [...FALLBACK_PROJECTS],
    loaded: false,
    error: null,
    fromHub: false,
  });
  const clientRef = useRef(apiClient);
  clientRef.current = apiClient;
  const aliveRef = useRef(true);
  const seqRef = useRef(0);
  const hubListRef = useRef<ProjectSummary[] | null>(null);

  const refresh = useCallback(async (): Promise<void> => {
    const seq = ++seqRef.current;
    const client = clientRef.current;
    let next: Omit<ProjectsState, "refresh">;
    if (!client || typeof client.listProjects !== "function") {
      next = { projects: [...FALLBACK_PROJECTS], loaded: true, error: null, fromHub: false };
    } else {
      try {
        const list = await client.listProjects();
        // An empty or malformed answer is how an older hub (or a generic stub) replies;
        // a real hub always lists at least one project, so treat it as "no list".
        if (!Array.isArray(list) || list.length === 0) {
          throw new HubApiError(404, "No project list");
        }
        next = { projects: list, loaded: true, error: null, fromHub: true };
      } catch (err) {
        // Keep the last list the hub gave us over regressing to the built-in one.
        const kept = hubListRef.current;
        const missing = isEndpointMissing(err);
        next = {
          projects: kept ?? [...FALLBACK_PROJECTS],
          loaded: true,
          error: missing
            ? null
            : err instanceof Error && err.message
              ? err.message
              : "Failed to load projects",
          fromHub: kept !== null,
        };
      }
    }
    if (!aliveRef.current || seq !== seqRef.current) return;
    if (next.fromHub) hubListRef.current = next.projects;
    setState(next);
  }, []);

  // Loads once on mount; the app keeps one client for its lifetime.
  useEffect(() => {
    aliveRef.current = true;
    void refresh();
    return () => {
      aliveRef.current = false;
    };
  }, [refresh]);

  return { ...state, refresh };
}
