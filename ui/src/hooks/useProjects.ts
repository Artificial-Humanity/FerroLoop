import { useCallback, useEffect, useRef, useState } from "react";
import { HubApiError } from "../api/client";
import type { ProjectSummary } from "../types/api";

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

/** One option of a project dropdown: the slug the hub takes, and the name shown. */
export interface ProjectChoice {
  value: string;
  label: string;
}

/**
 * The options of a project dropdown that sends a slug (`value`) and shows a name (`label`),
 * plus the value that stands for `current`: the slug of the choice it matches by slug or, failing
 * that, by display name (case-insensitive). A `current` project that is not in the list (a
 * legacy agent project) is listed first, as itself, and kept as the raw value.
 */
export function projectChoices(
  projects: readonly Pick<ProjectSummary, "name" | "slug">[],
  current: string | null | undefined,
  extra: readonly ProjectChoice[] = [],
): { choices: ProjectChoice[]; value: string } {
  const choices: ProjectChoice[] = projects.map((p) => ({
    value: p.slug.trim().toLowerCase(),
    label: projectLabel(p),
  }));
  for (const e of extra) {
    if (!choices.some((c) => c.value === e.value)) choices.push(e);
  }
  const raw = current ?? "";
  const key = raw.trim().toLowerCase();
  // A slug first, then a display name: an agent may hold "Sonora Voice" for `sonora-voice`.
  const match =
    choices.find((c) => c.value === key) ??
    choices.find((c) => c.label.trim().toLowerCase() === key);
  if (match) return { choices, value: match.value };
  if (raw) choices.unshift({ value: raw, label: raw });
  return { choices, value: raw };
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
  /** True when `projects` came from the hub, false when no hub list is loaded. */
  fromHub: boolean;
  /**
   * Whether the hub can create, edit and delete projects. `unsupported` when it answered 404
   * or 405, replied with legacy entries only, or the client has no `listProjects`. It stays
   * `available` until the first load settles, so the UI does not flash a notice.
   */
  projectManagement: "available" | "unsupported";
  /** Reloads the list. Overlapping calls resolve in order; the latest one wins. */
  refresh: () => Promise<void>;
  /** Clears cached hub projects (e.g. on logout or 401). */
  clear: () => void;
}

interface ProjectsClient {
  listProjects?: () => Promise<ProjectSummary[]>;
}

const EMPTY_REPLY = "No project list";

type ProjectManagement = ProjectsState["projectManagement"];

function isEndpointMissing(err: unknown): boolean {
  return err instanceof HubApiError && (err.status === 404 || err.status === 405);
}

function isUnauthorized(err: unknown): boolean {
  return err instanceof HubApiError && (err.status === 401 || err.status === 403);
}

/**
 * One project list for the whole app. Loads `listProjects()` on mount and on
 * `refresh()`; never falls back to a hard-coded project roster.
 */
export function useProjects(apiClient: ProjectsClient | undefined): ProjectsState {
  const [state, setState] = useState<Omit<ProjectsState, "refresh" | "clear">>({
    projects: [],
    loaded: false,
    error: null,
    fromHub: false,
    projectManagement: "available",
  });
  const clientRef = useRef(apiClient);
  clientRef.current = apiClient;
  const aliveRef = useRef(true);
  const seqRef = useRef(0);
  const hubListRef = useRef<ProjectSummary[] | null>(null);

  const clear = useCallback((): void => {
    ++seqRef.current;
    hubListRef.current = null;
    if (!aliveRef.current) return;
    setState({
      projects: [],
      loaded: false,
      error: null,
      fromHub: false,
      projectManagement: "available",
    });
  }, []);

  const refresh = useCallback(async (): Promise<void> => {
    const seq = ++seqRef.current;
    const client = clientRef.current;
    let next: Omit<ProjectsState, "refresh" | "clear">;
    if (!client || typeof client.listProjects !== "function") {
      next = {
        projects: [],
        loaded: true,
        error: null,
        fromHub: false,
        projectManagement: "unsupported",
      };
    } else {
      try {
        const list = await client.listProjects();
        // An empty or malformed answer is how an older hub (or a generic stub) replies;
        // a real hub always lists at least one project, so treat it as "no list".
        // Known limitation: deleting the last hub project therefore keeps the previous
        // list until the next non-empty reply, because `listProjects` cannot tell an
        // empty list from a malformed one.
        if (!Array.isArray(list) || list.length === 0) {
          throw new HubApiError(404, EMPTY_REPLY);
        }
        if (list.every((p) => p.legacy === true)) {
          // The hub that serves only `{id, artifact dirs}` cannot manage projects:
          // never show those entries as managed projects.
          hubListRef.current = null;
          next = {
            projects: [],
            loaded: true,
            error: null,
            fromHub: false,
            projectManagement: "unsupported",
          };
        } else {
          next = {
            projects: list,
            loaded: true,
            error: null,
            fromHub: true,
            projectManagement: "available",
          };
        }
      } catch (err) {
        const unauthorized = isUnauthorized(err);
        if (unauthorized) {
          hubListRef.current = null;
        }
        // Keep the last list the hub gave us on transient non-auth errors.
        const kept = unauthorized ? null : hubListRef.current;
        const missing = isEndpointMissing(err);
        // An empty or malformed reply is thrown as a 404 above; only a real HubApiError
        // from the endpoint (or the shortcut message) tells an absent endpoint.
        const emptyReply = err instanceof HubApiError && err.message === EMPTY_REPLY;
        const management: ProjectManagement =
          missing && !emptyReply ? "unsupported" : "available";
        next = {
          projects: kept ?? [],
          loaded: true,
          error: missing
            ? null
            : err instanceof Error && err.message
              ? err.message
              : "Failed to load projects",
          fromHub: kept !== null,
          projectManagement: management,
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

  return { ...state, refresh, clear };
}
