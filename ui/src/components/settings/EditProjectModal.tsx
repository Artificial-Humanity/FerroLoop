import { useEffect, useId, useRef, useState } from "react";
import type { ReactNode, RefObject } from "react";
import type { HubApiClient } from "../../api/client";
import type { ProjectSummary, UpdateProjectRequest } from "../../types/api";
import {
  ALERT_STYLE,
  BACKDROP_STYLE,
  DIALOG_STYLE,
  ERROR_TEXT_STYLE,
  FIELD_STYLE,
  HELP_STYLE,
  LABEL_STYLE,
  trapTabKey,
  useDialogBehavior,
  validateGithubTracker,
  validateProjectPath,
} from "./projectModalShared";

export interface EditProjectModalProps {
  isOpen: boolean;
  project: ProjectSummary | null;
  apiClient: Pick<HubApiClient, "updateProject">;
  /**
   * Element that opened the modal, captured at click time by the caller.
   * Focus returns to it when the modal closes.
   */
  returnFocusTo?: RefObject<HTMLElement | null>;
  onClose: () => void;
  onSaved: (project: ProjectSummary) => void;
}

function EditProjectForm({
  project,
  apiClient,
  returnFocusTo,
  onClose,
  onSaved,
}: Omit<EditProjectModalProps, "isOpen" | "project"> & {
  project: ProjectSummary;
}): ReactNode {
  const uid = useId();
  const isProject = project.kind === "project";
  const originalPurpose = project.purpose ?? "";
  const originalPath = project.path ?? "";
  const originalGithub = project.tracker?.repo ?? "";
  const originalCredential = project.tracker?.credential ?? "app";

  const [name, setName] = useState(project.name);
  const [purpose, setPurpose] = useState(originalPurpose);
  const [path, setPath] = useState(originalPath);
  const [github, setGithub] = useState(originalGithub);
  const [credential, setCredential] = useState<"app" | "env">(originalCredential);
  const [attempted, setAttempted] = useState(false);
  const [busy, setBusy] = useState(false);
  const [serverError, setServerError] = useState<string | null>(null);

  const dialogRef = useRef<HTMLDivElement>(null);
  const nameRef = useRef<HTMLInputElement>(null);
  const submitRef = useRef<HTMLButtonElement>(null);
  const [errorFocusTick, setErrorFocusTick] = useState(0);
  const busyRef = useRef(busy);
  busyRef.current = busy;

  useDialogBehavior({
    getInitialFocus: () => nameRef.current,
    returnFocusTo,
    busyRef,
    onClose,
  });

  // After a failed save the focused field was disabled while busy, so focus
  // would be lost; park it on the submit button.
  useEffect(() => {
    if (errorFocusTick > 0) submitRef.current?.focus();
  }, [errorFocusTick]);

  const trimmedName = name.trim();
  const trimmedPurpose = purpose.trim();
  const trimmedPath = path.trim();
  const trimmedGithub = github.trim();

  const errors: { name?: string; path?: string; github?: string } = {};
  if (!trimmedName) errors.name = "Name is required.";
  if (isProject) {
    errors.path = validateProjectPath(trimmedPath);
    errors.github = validateGithubTracker(trimmedGithub);
  }
  const shown = attempted ? errors : {};

  // Only changed fields go to the hub.
  const body: UpdateProjectRequest = {};
  if (trimmedName !== project.name) body.name = trimmedName;
  if (trimmedPurpose !== originalPurpose) body.purpose = trimmedPurpose;
  if (isProject) {
    if (trimmedPath !== originalPath) body.path = trimmedPath;
    if (trimmedGithub !== originalGithub) {
      body.github = trimmedGithub === "" ? null : trimmedGithub;
      // A new tracker always names its credential; clearing needs none.
      if (trimmedGithub !== "" && originalGithub === "")
        body.credential = credential;
    }
    if (
      trimmedGithub !== "" &&
      originalGithub !== "" &&
      credential !== originalCredential
    )
      body.credential = credential;
  }
  const changed = Object.keys(body).length > 0;

  const submit = async () => {
    // busyRef flips synchronously so a fast double submit cannot slip through
    // before the busy state re-renders.
    if (busyRef.current || !changed) return;
    setAttempted(true);
    setServerError(null);
    if (errors.name || errors.path || errors.github) {
      const target = errors.name ? "name" : errors.path ? "path" : "github";
      dialogRef.current
        ?.querySelector<HTMLElement>(`[data-testid="edit-project-${target}"]`)
        ?.focus();
      return;
    }
    busyRef.current = true;
    setBusy(true);
    let saved: ProjectSummary;
    try {
      saved = await apiClient.updateProject(project.slug, body);
    } catch (err) {
      setServerError(
        err instanceof Error && err.message
          ? err.message
          : "Could not save the project.",
      );
      busyRef.current = false;
      setBusy(false);
      setErrorFocusTick((n) => n + 1);
      return;
    }
    // The change exists on the hub even if the form unmounted mid-request,
    // so onSaved still runs.
    onSaved(saved);
    onClose();
  };

  const describedBy = (key: string, hasError: boolean, extra?: string) =>
    [hasError ? `${uid}-${key}-err` : null, extra].filter(Boolean).join(" ") ||
    undefined;

  const fieldError = (key: string, message?: string): ReactNode =>
    message ? (
      <p id={`${uid}-${key}-err`} style={ERROR_TEXT_STYLE}>
        {message}
      </p>
    ) : null;

  return (
    <div
      data-testid="edit-project-modal-backdrop"
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
        data-testid="edit-project-modal"
        className="fd-card"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={(e) => trapTabKey(e, dialogRef.current)}
        style={DIALOG_STYLE}
      >
        <strong id={`${uid}-title`} style={{ fontSize: "14px" }}>
          Edit {project.name}
        </strong>

        <form
          noValidate
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
          style={{ display: "flex", flexDirection: "column", gap: "12px" }}
        >
          <div style={{ display: "flex", gap: "12px", flexWrap: "wrap" }}>
            <div style={{ ...FIELD_STYLE, flex: 1, minWidth: "160px" }}>
              <span style={LABEL_STYLE}>Kind</span>
              <span data-testid="edit-project-kind" style={{ fontSize: "12px" }}>
                {isProject ? "Directory Project" : "Virtual Group / Scope"}
              </span>
            </div>
            <div style={{ ...FIELD_STYLE, flex: 1, minWidth: "160px" }}>
              <span style={LABEL_STYLE}>Slug</span>
              <code
                data-testid="edit-project-slug"
                className="fd-mono"
                style={{ fontSize: "12px" }}
              >
                {project.slug}
              </code>
            </div>
          </div>

          <div style={FIELD_STYLE}>
            <label htmlFor={`${uid}-name`} style={LABEL_STYLE}>
              Name
            </label>
            <input
              id={`${uid}-name`}
              ref={nameRef}
              type="text"
              className="fd-input"
              data-testid="edit-project-name"
              value={name}
              disabled={busy}
              aria-invalid={shown.name ? true : undefined}
              aria-describedby={describedBy("name", Boolean(shown.name))}
              onChange={(e) => setName(e.target.value)}
            />
            {fieldError("name", shown.name)}
          </div>

          {isProject && (
            <div style={FIELD_STYLE}>
              <label htmlFor={`${uid}-path`} style={LABEL_STYLE}>
                Directory Path
              </label>
              <input
                id={`${uid}-path`}
                type="text"
                className="fd-input fd-mono"
                data-testid="edit-project-path"
                value={path}
                disabled={busy}
                aria-invalid={shown.path ? true : undefined}
                aria-describedby={describedBy(
                  "path",
                  Boolean(shown.path),
                  `${uid}-path-help`,
                )}
                onChange={(e) => setPath(e.target.value)}
              />
              <p id={`${uid}-path-help`} style={HELP_STYLE}>
                Changes the registered directory. Files on disk are not moved.
              </p>
              {fieldError("path", shown.path)}
            </div>
          )}

          <div style={FIELD_STYLE}>
            <label htmlFor={`${uid}-purpose`} style={LABEL_STYLE}>
              Purpose / Description
            </label>
            <textarea
              id={`${uid}-purpose`}
              rows={2}
              className="fd-textarea"
              data-testid="edit-project-purpose"
              value={purpose}
              disabled={busy}
              onChange={(e) => setPurpose(e.target.value)}
            />
          </div>

          {isProject && (
            <>
              <div style={FIELD_STYLE}>
                <label htmlFor={`${uid}-github`} style={LABEL_STYLE}>
                  GitHub Tracker (optional)
                </label>
                <input
                  id={`${uid}-github`}
                  type="text"
                  className="fd-input fd-mono"
                  data-testid="edit-project-github"
                  placeholder="owner/repo"
                  value={github}
                  disabled={busy}
                  aria-invalid={shown.github ? true : undefined}
                  aria-describedby={describedBy("github", Boolean(shown.github))}
                  onChange={(e) => setGithub(e.target.value)}
                />
                {fieldError("github", shown.github)}
              </div>

              {trimmedGithub && (
                <div style={FIELD_STYLE}>
                  <label htmlFor={`${uid}-credential`} style={LABEL_STYLE}>
                    Credential
                  </label>
                  <select
                    id={`${uid}-credential`}
                    className="fd-select"
                    data-testid="edit-project-credential"
                    value={credential}
                    disabled={busy}
                    onChange={(e) =>
                      setCredential(e.target.value === "env" ? "env" : "app")
                    }
                  >
                    <option value="app">app</option>
                    <option value="env">env</option>
                  </select>
                </div>
              )}
            </>
          )}

          {serverError && (
            <div
              role="alert"
              data-testid="edit-project-error"
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
              data-testid="edit-project-cancel"
              className="fd-btn"
              disabled={busy}
              onClick={onClose}
            >
              Cancel
            </button>
            <button
              ref={submitRef}
              type="submit"
              data-testid="edit-project-submit"
              className="fd-btn fd-btn-primary"
              disabled={busy || !changed}
            >
              {busy ? "Saving..." : "Save"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

export function EditProjectModal(props: EditProjectModalProps): ReactNode {
  const { isOpen, project, ...rest } = props;
  if (!isOpen || !project) return null;
  return <EditProjectForm key={project.slug} project={project} {...rest} />;
}
