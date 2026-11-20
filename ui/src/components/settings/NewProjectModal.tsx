import { useEffect, useId, useRef, useState } from "react";
import type { ReactNode, RefObject } from "react";
import type { HubApiClient } from "../../api/client";
import type {
  CreateProjectRequest,
  ProjectKind,
  ProjectSummary,
} from "../../types/api";
import {
  ERROR_TEXT_STYLE,
  FIELD_STYLE,
  HELP_STYLE,
  LABEL_STYLE,
  trapTabKey,
  validateGithubTracker,
  validateProjectPath,
} from "./projectModalShared";

export interface NewProjectModalProps {
  isOpen: boolean;
  apiClient: Pick<HubApiClient, "createProject">;
  /** Slugs already registered; a new entry may not reuse one. */
  existingSlugs: string[];
  /**
   * Element that opened the modal, captured at click time by the caller.
   * Focus returns to it when the modal closes (reading document.activeElement
   * is unreliable: Safari and Firefox on macOS do not focus a button on click).
   */
  returnFocusTo?: RefObject<HTMLElement | null>;
  onClose: () => void;
  onCreated: (project: ProjectSummary) => void;
}

const SLUG_PATTERN = /^[a-z][a-z0-9_-]{0,63}$/;
const SLUG_HELP =
  "Enter a slug that starts with a lowercase letter and uses at most 64 lowercase letters, digits, hyphens, or underscores.";
const MAX_NAME_CHARS = 80;
const MAX_NAME_SEGMENTS = 4;

export function deriveProjectSlug(name: string): string {
  const normalized = name
    .normalize("NFD")
    .replace(/([A-Za-z])\p{Mark}+/gu, "$1");
  if (/[^\x00-\x7f]/.test(normalized.replace(/[^\p{Alphabetic}]/gu, ""))) {
    return "";
  }
  const parts = normalized.split("/");
  // Like the Hub, retain underscores in flat names and use kebab-case in paths.
  const separators = parts.length === 1 ? /[^a-z0-9_]+/g : /[^a-z0-9]+/g;
  const segments = parts.map((segment) => segment
    .toLowerCase()
    .replace(separators, "-")
    .replace(/^-+|-+$/g, ""),
  );
  return segments.every(Boolean) ? segments.join("-") : "";
}

function NewProjectForm({
  apiClient,
  existingSlugs,
  returnFocusTo,
  onClose,
  onCreated,
}: Omit<NewProjectModalProps, "isOpen">): ReactNode {
  const uid = useId();
  const [kind, setKind] = useState<ProjectKind>("project");
  const [name, setName] = useState("");
  const [slug, setSlug] = useState("");
  const [slugManual, setSlugManual] = useState(false);
  const [createParent, setCreateParent] = useState(false);
  const [path, setPath] = useState("");
  const [purpose, setPurpose] = useState("");
  const [gitInit, setGitInit] = useState(false);
  const [github, setGithub] = useState("");
  const [credential, setCredential] = useState<"app" | "env">("app");
  const [attempted, setAttempted] = useState(false);
  const [busy, setBusy] = useState(false);
  const [serverError, setServerError] = useState<string | null>(null);

  const dialogRef = useRef<HTMLDivElement>(null);
  const nameRef = useRef<HTMLInputElement>(null);
  const submitRef = useRef<HTMLButtonElement>(null);
  const [errorFocusTick, setErrorFocusTick] = useState(0);
  const returnFocusRef = useRef(returnFocusTo);
  returnFocusRef.current = returnFocusTo;
  const busyRef = useRef(busy);
  busyRef.current = busy;
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  const isProject = kind === "project";
  const trimmedName = name.trim();
  const nameSegments = trimmedName.split("/").map((segment) => segment.trim());
  const isNested = nameSegments.length > 1;
  const trimmedSlug = slug.trim();
  const trimmedPath = path.trim();
  const trimmedGithub = github.trim();
  const trimmedPurpose = purpose.trim();
  const slugTaken = existingSlugs.includes(trimmedSlug);
  const suggestedSlugTaken = !slugManual && slugTaken;

  const errors: {
    name?: string;
    slug?: string;
    path?: string;
    github?: string;
  } = {};
  if (!trimmedName) errors.name = "Name is required.";
  else if (Array.from(trimmedName).length > MAX_NAME_CHARS)
    errors.name = `Name must be at most ${MAX_NAME_CHARS} characters.`;
  else if (nameSegments.length > MAX_NAME_SEGMENTS || nameSegments.some((segment) => !segment))
    errors.name = `Use one to ${MAX_NAME_SEGMENTS} non-empty segments separated by "/".`;
  if (!trimmedSlug)
    errors.slug = trimmedName && !deriveProjectSlug(trimmedName) ? SLUG_HELP : "Slug is required.";
  else if (!SLUG_PATTERN.test(trimmedSlug)) errors.slug = SLUG_HELP;
  else if (slugManual && slugTaken)
    errors.slug = `A project or group with the slug "${trimmedSlug}" already exists.`;
  if (isProject) {
    errors.path = validateProjectPath(trimmedPath);
    errors.github = validateGithubTracker(trimmedGithub);
  }
  const shown = attempted ? errors : {};

  // Initial focus on Name; focus returns to the opener when the form unmounts.
  useEffect(() => {
    nameRef.current?.focus();
    return () => {
      returnFocusRef.current?.current?.focus();
    };
  }, []);

  // After a failed create the focused (now re-enabled) field was disabled
  // while busy, so focus would be lost; park it on the submit button.
  useEffect(() => {
    if (errorFocusTick > 0) submitRef.current?.focus();
  }, [errorFocusTick]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !busyRef.current) onCloseRef.current();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  const trapTab = (event: React.KeyboardEvent<HTMLDivElement>) =>
    trapTabKey(event, dialogRef.current);

  const changeName = (value: string) => {
    setName(value);
    if (!value.includes("/")) setCreateParent(false);
    // An emptied slug counts as "not chosen yet": editing the name refills it.
    if (!slugManual || slug === "") {
      setSlugManual(false);
      setSlug(deriveProjectSlug(value));
    }
  };

  // Leaving the slug field empty hands it back to the name.
  const refillEmptySlug = () => {
    if (slug !== "") return;
    setSlugManual(false);
    setSlug(deriveProjectSlug(name));
  };

  const submit = async () => {
    // busyRef flips synchronously so a fast double submit cannot slip through
    // before the busy state re-renders.
    if (busyRef.current) return;
    setAttempted(true);
    setServerError(null);
    if (errors.name || errors.slug || errors.path || errors.github) {
      const target = errors.name
        ? "name"
        : errors.slug
          ? "slug"
          : errors.path
            ? "path"
            : "github";
      dialogRef.current
        ?.querySelector<HTMLElement>(`[data-testid="new-project-${target}"]`)
        ?.focus();
      return;
    }
    // git_init is omitted (not sent as false) unless the box is checked, so
    // the hub keeps its own default.
    const req: CreateProjectRequest = {
      name: trimmedName,
      kind,
      create_only: true,
    };
    // Let the hub resolve an untouched preview, including historical project slugs.
    if (slugManual) req.slug = trimmedSlug;
    if (isNested && createParent) req.create_parent = true;
    if (trimmedPurpose) req.purpose = trimmedPurpose;
    if (isProject) {
      req.path = trimmedPath;
      if (gitInit) req.git_init = true;
      if (trimmedGithub) {
        req.github = trimmedGithub;
        req.credential = credential;
      }
    }
    busyRef.current = true;
    setBusy(true);
    let created: ProjectSummary;
    try {
      created = await apiClient.createProject(req);
    } catch (err) {
      setServerError(
        err instanceof Error && err.message
          ? err.message
          : "Could not create the project.",
      );
      busyRef.current = false;
      setBusy(false);
      setErrorFocusTick((n) => n + 1);
      return;
    }
    // The project exists on the hub even if the form unmounted mid-request,
    // so onCreated still runs.
    onCreated(created);
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
      data-testid="new-project-modal-backdrop"
      onClick={() => {
        if (!busy) onClose();
      }}
      style={{
        position: "fixed",
        inset: 0,
        backgroundColor: "rgba(9, 13, 22, 0.72)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        zIndex: 1300,
        padding: "16px",
      }}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={`${uid}-title`}
        data-testid="new-project-modal"
        className="fd-card"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={trapTab}
        style={{
          width: "min(520px, 100%)",
          maxHeight: "calc(100dvh - 32px)",
          overflowY: "auto",
          backgroundColor: "var(--bg-surface, #0f172a)",
          border: "1px solid var(--border-accent)",
          borderRadius: "8px",
          padding: "16px",
          display: "flex",
          flexDirection: "column",
          gap: "12px",
          boxShadow: "0 20px 44px rgba(0, 0, 0, 0.55)",
        }}
      >
        <strong id={`${uid}-title`} style={{ fontSize: "14px" }}>
          New Project / Virtual Group
        </strong>

        <form
          noValidate
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
          style={{ display: "flex", flexDirection: "column", gap: "12px" }}
        >
          <div
            role="radiogroup"
            aria-labelledby={`${uid}-kind`}
            style={FIELD_STYLE}
          >
            <span id={`${uid}-kind`} style={LABEL_STYLE}>
              Kind
            </span>
            <label style={{ display: "flex", gap: "8px", alignItems: "flex-start" }}>
              <input
                type="radio"
                name={`${uid}-kind`}
                data-testid="new-project-kind-project"
                checked={isProject}
                disabled={busy}
                onChange={() => setKind("project")}
              />
              <span style={{ display: "flex", flexDirection: "column" }}>
                <span style={{ fontSize: "12px" }}>Directory Project</span>
                <span style={HELP_STYLE}>
                  A real directory where agents write and build.
                </span>
              </span>
            </label>
            <label style={{ display: "flex", gap: "8px", alignItems: "flex-start" }}>
              <input
                type="radio"
                name={`${uid}-kind`}
                data-testid="new-project-kind-virtual"
                checked={!isProject}
                disabled={busy}
                onChange={() => {
                  setKind("virtual");
                  setPath("");
                  setGitInit(false);
                  setGithub("");
                  setCredential("app");
                }}
              />
              <span style={{ display: "flex", flexDirection: "column" }}>
                <span style={{ fontSize: "12px" }}>Virtual Group / Scope</span>
                <span style={HELP_STYLE}>
                  An organizational group such as Council. It has no dedicated
                  directory; the working directory defaults to the workspace
                  root.
                </span>
              </span>
            </label>
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
              data-testid="new-project-name"
              value={name}
              disabled={busy}
              aria-invalid={shown.name ? true : undefined}
              aria-describedby={shown.name ? `${uid}-name-err` : `${uid}-name-help`}
              onChange={(e) => changeName(e.target.value)}
            />
            <p id={`${uid}-name-help`} style={HELP_STYLE}>
              Use slashes to nest a project, for example Lab / Research / Mobile.
            </p>
            {fieldError("name", shown.name)}
          </div>

          <div style={FIELD_STYLE}>
            <label htmlFor={`${uid}-slug`} style={LABEL_STYLE}>
              Slug
            </label>
            <input
              id={`${uid}-slug`}
              type="text"
              className="fd-input fd-mono"
              data-testid="new-project-slug"
              value={slug}
              disabled={busy}
              aria-invalid={shown.slug ? true : undefined}
              aria-describedby={describedBy(
                "slug",
                Boolean(shown.slug),
                suggestedSlugTaken ? `${uid}-slug-advisory` : undefined,
              )}
              onBlur={refillEmptySlug}
              onChange={(e) => {
                // Any edit is manual; an empty value stays empty while typing.
                setSlugManual(true);
                setSlug(e.target.value);
              }}
            />
            {suggestedSlugTaken && (
              <p id={`${uid}-slug-advisory`} style={HELP_STYLE}>
                The suggested slug is in use. The Hub will check if it can use an existing project or group.
              </p>
            )}
            {fieldError("slug", shown.slug)}
          </div>

          {isNested && (
            <label style={{ display: "flex", gap: "8px", alignItems: "center", fontSize: "12px" }}>
              <input
                type="checkbox"
                checked={createParent}
                disabled={busy}
                onChange={(e) => setCreateParent(e.target.checked)}
              />
              Create missing parent groups
            </label>
          )}

          {isProject && (
            <div style={FIELD_STYLE}>
              <label htmlFor={`${uid}-path`} style={LABEL_STYLE}>
                Directory Path
              </label>
              <input
                id={`${uid}-path`}
                type="text"
                className="fd-input fd-mono"
                data-testid="new-project-path"
                placeholder="/home/you/work/my-project"
                value={path}
                disabled={busy}
                aria-invalid={shown.path ? true : undefined}
                aria-describedby={describedBy("path", Boolean(shown.path))}
                onChange={(e) => setPath(e.target.value)}
              />
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
              data-testid="new-project-purpose"
              value={purpose}
              disabled={busy}
              onChange={(e) => setPurpose(e.target.value)}
            />
          </div>

          {isProject && (
            <>
              <div style={FIELD_STYLE}>
                <label
                  htmlFor={`${uid}-git-init`}
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: "8px",
                    fontSize: "12px",
                  }}
                >
                  <input
                    id={`${uid}-git-init`}
                    type="checkbox"
                    data-testid="new-project-git-init"
                    checked={gitInit}
                    disabled={busy}
                    aria-describedby={`${uid}-git-help`}
                    onChange={(e) => setGitInit(e.target.checked)}
                  />
                  <span>Initialize Git repository (git init)</span>
                </label>
                <p id={`${uid}-git-help`} style={HELP_STYLE}>
                  Git is not required. Leave this unchecked to use the
                  directory as it is.
                </p>
              </div>

              <div style={FIELD_STYLE}>
                <label htmlFor={`${uid}-github`} style={LABEL_STYLE}>
                  GitHub Tracker (optional)
                </label>
                <input
                  id={`${uid}-github`}
                  type="text"
                  className="fd-input fd-mono"
                  data-testid="new-project-github"
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
                    data-testid="new-project-credential"
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
              data-testid="new-project-error"
              className="fd-card"
              style={{
                borderColor: "var(--diff-del-border)",
                backgroundColor: "var(--diff-del-bg)",
                color: "var(--diff-del-text)",
                fontSize: "11.5px",
                padding: "6px 10px",
              }}
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
              data-testid="new-project-cancel"
              className="fd-btn"
              disabled={busy}
              onClick={onClose}
            >
              Cancel
            </button>
            <button
              ref={submitRef}
              type="submit"
              data-testid="new-project-submit"
              className="fd-btn fd-btn-primary"
              disabled={busy}
            >
              {busy ? "Creating..." : "Create"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

export function NewProjectModal(props: NewProjectModalProps): ReactNode {
  const { isOpen, ...rest } = props;
  if (!isOpen) return null;
  return <NewProjectForm {...rest} />;
}
