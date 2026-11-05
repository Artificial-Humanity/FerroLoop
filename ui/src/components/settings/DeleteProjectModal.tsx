import { useEffect, useId, useRef, useState } from "react";
import type { ReactNode, RefObject } from "react";
import { ProjectDeleteConflictError } from "../../api/client";
import type { HubApiClient } from "../../api/client";
import type {
  DeleteProjectRequest,
  DeleteProjectResponse,
  ProjectSummary,
} from "../../types/api";
import {
  ALERT_STYLE,
  BACKDROP_STYLE,
  DIALOG_STYLE,
  FIELD_STYLE,
  HELP_STYLE,
  LABEL_STYLE,
  trapTabKey,
  useDialogBehavior,
} from "./projectModalShared";

export interface DeleteProjectModalProps {
  isOpen: boolean;
  project: ProjectSummary | null;
  /** Candidates for reassigning active agents; the project itself is ignored. */
  otherProjects: ProjectSummary[];
  apiClient: Pick<HubApiClient, "deleteProject">;
  /**
   * Element that opened the dialog, captured at click time by the caller.
   * Focus returns to it when the dialog closes.
   */
  returnFocusTo?: RefObject<HTMLElement | null>;
  onClose: () => void;
  onDeleted: (slug: string, response: DeleteProjectResponse) => void;
}

type AgentChoice = "" | "reassign" | "decommission";

/** One-line summary of what a delete did, for the Projects tab status line. */
export function describeProjectDeletion(
  name: string,
  res: DeleteProjectResponse,
): string {
  const plural = (n: number) => `${n} agent${n === 1 ? "" : "s"}`;
  const failed = typeof res.error === "string" && res.error !== "";
  const parts = [
    failed
      ? `Unregistered ${name}, but the cleanup failed: ${res.error}`
      : `Deleted ${name}.`,
  ];
  if (res.reassigned_agents > 0)
    parts.push(`Reassigned ${plural(res.reassigned_agents)}.`);
  if (res.decommissioned_agents > 0)
    parts.push(`Decommissioned ${plural(res.decommissioned_agents)}.`);
  if (failed) {
    parts.push(
      `Store deleted: ${res.store_deleted ? "yes" : "no"}.`,
      `Directory deleted: ${res.dir_deleted ? "yes" : "no"}.`,
    );
    return parts.join(" ");
  }
  if (res.store_deleted) parts.push("Deleted the local FerroStore database.");
  if (res.dir_deleted) parts.push("Deleted the project directory.");
  return parts.join(" ");
}

function DeleteProjectForm({
  project,
  otherProjects,
  apiClient,
  returnFocusTo,
  onClose,
  onDeleted,
}: Omit<DeleteProjectModalProps, "isOpen" | "project"> & {
  project: ProjectSummary;
}): ReactNode {
  const uid = useId();
  const isProject = project.kind === "project";
  const candidates = otherProjects.filter((p) => p.slug !== project.slug);

  const [choice, setChoice] = useState<AgentChoice>("");
  const [reassignTo, setReassignTo] = useState("");
  const [deleteStore, setDeleteStore] = useState(false);
  const [deleteDir, setDeleteDir] = useState(false);
  // A 409 proves agents exist even when the cached count said none.
  const [forceGuard, setForceGuard] = useState(false);
  const [busy, setBusy] = useState(false);
  const [serverError, setServerError] = useState<string | null>(null);

  const dialogRef = useRef<HTMLDivElement>(null);
  const submitRef = useRef<HTMLButtonElement>(null);
  const [errorFocus, setErrorFocus] = useState<{
    tick: number;
    toGuard: boolean;
  }>({ tick: 0, toGuard: false });
  const busyRef = useRef(busy);
  busyRef.current = busy;

  const guardVisible = project.active_agent_count > 0 || forceGuard;
  const guardSatisfied =
    !guardVisible ||
    choice === "decommission" ||
    (choice === "reassign" && candidates.some((p) => p.slug === reassignTo));

  // Initial focus goes to the first control, never to the destructive confirm.
  useDialogBehavior({
    getInitialFocus: () =>
      dialogRef.current?.querySelector<HTMLElement>(
        "input:not(:disabled), select:not(:disabled), button:not(:disabled)",
      ),
    returnFocusTo,
    busyRef,
    onClose,
  });

  // After a failed delete the focused control was disabled while busy, so
  // focus would be lost: a 409 moves it to the (now visible) guard, anything
  // else to the confirm button.
  useEffect(() => {
    if (errorFocus.tick === 0) return;
    const target = errorFocus.toGuard
      ? dialogRef.current?.querySelector<HTMLElement>(
          '[data-testid="delete-project-guard"] input:not(:disabled)',
        )
      : submitRef.current;
    (target ?? submitRef.current)?.focus();
  }, [errorFocus]);

  const submit = async () => {
    // busyRef flips synchronously so a fast double submit cannot slip through
    // before the busy state re-renders.
    if (busyRef.current || !guardSatisfied) return;
    setServerError(null);
    // Unchecked boxes are omitted, so the hub keeps its own defaults.
    const req: DeleteProjectRequest = {};
    if (guardVisible) {
      if (choice === "reassign") req.reassign_to = reassignTo;
      else if (choice === "decommission") req.decommission_agents = true;
    }
    if (isProject) {
      if (deleteStore) req.delete_store = true;
      if (deleteDir) req.delete_dir = true;
    }
    busyRef.current = true;
    setBusy(true);
    let response: DeleteProjectResponse;
    try {
      response = await apiClient.deleteProject(project.slug, req);
    } catch (err) {
      let toGuard = false;
      if (err instanceof ProjectDeleteConflictError) {
        toGuard = true;
        setForceGuard(true);
        const names = err.active_agents.join(", ");
        setServerError(
          `${project.name} still has ${err.active_agent_count} active agent${
            err.active_agent_count === 1 ? "" : "s"
          }${names ? ` (${names})` : ""}. Reassign them or decommission them first.`,
        );
      } else {
        setServerError(
          err instanceof Error && err.message
            ? err.message
            : "Could not delete the project.",
        );
      }
      busyRef.current = false;
      setBusy(false);
      setErrorFocus((f) => ({ tick: f.tick + 1, toGuard }));
      return;
    }
    // The entry is gone on the hub even if the dialog unmounted mid-request,
    // so onDeleted still runs.
    onDeleted(project.slug, response);
    onClose();
  };

  const kindWord = isProject ? "project" : "group";
  const storeLabel = project.store_path ?? `${project.slug}.fls`;

  return (
    <div
      data-testid="delete-project-modal-backdrop"
      onClick={() => {
        if (!busy) onClose();
      }}
      style={BACKDROP_STYLE}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={`${uid}-title`}
        aria-describedby={`${uid}-intro`}
        data-testid="delete-project-modal"
        className="fd-card"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={(e) => trapTabKey(e, dialogRef.current)}
        style={DIALOG_STYLE}
      >
        <strong id={`${uid}-title`} style={{ fontSize: "14px" }}>
          Delete {project.name}
        </strong>
        <p id={`${uid}-intro`} style={{ ...HELP_STYLE, fontSize: "12px" }}>
          By default this only unregisters the {kindWord} from FerroLoop. It does
          not delete any files on disk.
        </p>

        <form
          noValidate
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
          style={{ display: "flex", flexDirection: "column", gap: "12px" }}
        >
          {guardVisible && (
            <div
              role="radiogroup"
              aria-labelledby={`${uid}-guard`}
              data-testid="delete-project-guard"
              style={FIELD_STYLE}
            >
              <span id={`${uid}-guard`} style={LABEL_STYLE}>
                {project.active_agent_count > 0
                  ? `${project.active_agent_count} active agent${
                      project.active_agent_count === 1 ? "" : "s"
                    } in this ${kindWord}. Choose what happens to them.`
                  : `This ${kindWord} still has active agents. Choose what happens to them.`}
              </span>

              <label
                style={{ display: "flex", gap: "8px", alignItems: "flex-start" }}
              >
                <input
                  type="radio"
                  name={`${uid}-agents`}
                  data-testid="delete-project-reassign-radio"
                  checked={choice === "reassign"}
                  disabled={busy || candidates.length === 0}
                  onChange={() => setChoice("reassign")}
                />
                <span style={{ fontSize: "12px" }}>Reassign active agents to:</span>
              </label>
              <select
                aria-label="Reassign active agents to"
                className="fd-select"
                data-testid="delete-project-reassign-select"
                value={reassignTo}
                disabled={busy || choice !== "reassign"}
                onChange={(e) => setReassignTo(e.target.value)}
              >
                <option value="">Select a project or group</option>
                {candidates.map((p) => (
                  <option key={p.slug} value={p.slug}>
                    {p.name} ({p.slug})
                  </option>
                ))}
              </select>
              {candidates.length === 0 && (
                <p style={HELP_STYLE}>
                  There is no other project or group to reassign to.
                </p>
              )}

              <label
                style={{ display: "flex", gap: "8px", alignItems: "flex-start" }}
              >
                <input
                  type="radio"
                  name={`${uid}-agents`}
                  data-testid="delete-project-decommission-radio"
                  checked={choice === "decommission"}
                  disabled={busy}
                  onChange={() => setChoice("decommission")}
                />
                <span style={{ fontSize: "12px" }}>
                  Decommission all active agents in this {kindWord} first
                </span>
              </label>
            </div>
          )}

          {isProject && (
            <div style={FIELD_STYLE}>
              <label
                style={{ display: "flex", alignItems: "center", gap: "8px", fontSize: "12px" }}
              >
                <input
                  type="checkbox"
                  data-testid="delete-project-delete-store"
                  checked={deleteStore}
                  disabled={busy}
                  onChange={(e) => setDeleteStore(e.target.checked)}
                />
                <span>
                  Also delete local FerroStore database (
                  <code className="fd-mono">{storeLabel}</code>)
                </span>
              </label>
              <label
                style={{ display: "flex", alignItems: "center", gap: "8px", fontSize: "12px" }}
              >
                <input
                  type="checkbox"
                  data-testid="delete-project-delete-dir"
                  checked={deleteDir}
                  disabled={busy}
                  aria-describedby={deleteDir ? `${uid}-dir-warning` : undefined}
                  onChange={(e) => setDeleteDir(e.target.checked)}
                />
                <span>
                  Also delete project directory on disk (
                  <code className="fd-mono">{project.path ?? ""}</code>)
                </span>
              </label>
              {deleteDir && (
                <span
                  id={`${uid}-dir-warning`}
                  data-testid="delete-project-dir-warning"
                  className="fd-badge"
                  style={{
                    alignSelf: "flex-start",
                    fontSize: "11px",
                    color: "var(--diff-del-text)",
                    borderColor: "var(--diff-del-border)",
                    backgroundColor: "var(--diff-del-bg)",
                  }}
                >
                  Permanent. This deletes files on disk.
                </span>
              )}
            </div>
          )}

          {serverError && (
            <div
              role="alert"
              data-testid="delete-project-error"
              className="fd-card"
              style={ALERT_STYLE}
            >
              {serverError}
            </div>
          )}

          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "flex-end",
              gap: "8px",
            }}
          >
            <button
              type="button"
              data-testid="delete-project-cancel"
              className="fd-btn"
              disabled={busy}
              onClick={onClose}
            >
              Cancel
            </button>
            <button
              ref={submitRef}
              type="submit"
              data-testid="delete-project-submit"
              className={deleteDir ? "fd-btn fd-btn-danger" : "fd-btn fd-btn-primary"}
              disabled={busy || !guardSatisfied}
            >
              {busy
                ? "Deleting..."
                : deleteDir
                  ? "Delete project and directory"
                  : "Delete"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

export function DeleteProjectModal(props: DeleteProjectModalProps): ReactNode {
  const { isOpen, project, ...rest } = props;
  if (!isOpen || !project) return null;
  return <DeleteProjectForm key={project.slug} project={project} {...rest} />;
}
