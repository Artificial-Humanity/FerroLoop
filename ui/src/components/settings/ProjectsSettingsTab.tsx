import type { ReactNode } from "react";
import type { ProjectSummary } from "../../types/api";

export interface ProjectArtifactsDirView {
  effectiveDir: string;
  /** Explicit override, or `null` when the default applies. */
  artifactsDir: string | null;
}

export interface ProjectsArtifactsDirProps {
  getState: (slug: string) => ProjectArtifactsDirView;
  /** `slug` keys lookup and API calls; `name` is the display text for the modal and banner. */
  onChange: (slug: string, name: string) => void;
  /** Result banner of the last change, shown on the card it belongs to. */
  status: { slug: string; message: string } | null;
}

export interface ProjectsSettingsTabProps {
  projects: ProjectSummary[];
  loading: boolean;
  error: string | null;
  onViewAgents: (slug: string) => void;
  artifactsDir: ProjectsArtifactsDirProps;
  /** Receives the clicked button so the caller can return focus to it. */
  onCreate?: (opener: HTMLElement) => void;
  onEdit?: (project: ProjectSummary) => void;
  onDelete?: (project: ProjectSummary) => void;
}

const LABEL_STYLE = {
  fontSize: "11.5px",
  fontWeight: 600,
  color: "var(--text-secondary)",
} as const;

const BUTTON_STYLE = { fontSize: "11px", padding: "3px 8px" } as const;

export function ProjectArtifactsDirRow({
  slug,
  displayName,
  artifactsDir,
}: {
  slug: string;
  /** Text shown to the user for this project; defaults to the slug. */
  displayName?: string;
  artifactsDir: ProjectsArtifactsDirProps;
}): ReactNode {
  const info = artifactsDir.getState(slug);
  const status =
    artifactsDir.status?.slug === slug ? artifactsDir.status.message : null;
  return (
    <div
      data-testid="settings-project-artifacts-dir-row"
      style={{
        display: "flex",
        flexDirection: "column",
        gap: "6px",
        paddingTop: "8px",
        borderTop: "1px solid var(--border-subtle)",
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: "8px",
          flexWrap: "wrap",
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "8px",
            flexWrap: "wrap",
            minWidth: 0,
          }}
        >
          <span style={LABEL_STYLE}>Artifacts Directory:</span>
          <code
            data-testid="settings-project-artifacts-dir-value"
            className="fd-badge fd-mono"
            title={info.effectiveDir}
            style={{
              fontSize: "11px",
              maxWidth: "100%",
              overflow: "hidden",
              textOverflow: "ellipsis",
            }}
          >
            {info.effectiveDir}
          </code>
          <span
            data-testid="settings-project-artifacts-hierarchy-badge"
            className="fd-badge fd-mono"
            title="Default project hierarchy under [home]/artifacts/[project-name]"
            style={{ fontSize: "10px", color: "var(--text-muted)" }}
          >
            [home]/artifacts/[project-name]
          </span>
          {info.artifactsDir && (
            <span
              data-testid="settings-project-artifacts-dir-override-badge"
              className="fd-badge fd-mono"
              style={{
                fontSize: "10px",
                color: "var(--accent-primary, #f59e0b)",
              }}
            >
              Override
            </span>
          )}
        </div>
        <button
          type="button"
          data-testid="settings-project-change-artifacts-dir-btn"
          className="fd-btn"
          style={BUTTON_STYLE}
          onClick={() => artifactsDir.onChange(slug, displayName ?? slug)}
        >
          Change Artifacts Directory
        </button>
      </div>
      {status && (
        <div
          role="status"
          data-testid="project-artifacts-dir-status"
          className="fd-card"
          style={{
            borderColor: "var(--diff-add-border)",
            backgroundColor: "var(--diff-add-bg)",
            color: "var(--diff-add-text)",
            fontSize: "11.5px",
            padding: "6px 10px",
          }}
        >
          {status}
        </div>
      )}
    </div>
  );
}

function ProjectCard({
  project,
  props,
}: {
  project: ProjectSummary;
  props: ProjectsSettingsTabProps;
}): ReactNode {
  const { slug } = project;
  const isVirtual = project.kind === "virtual";
  const count = project.active_agent_count;
  return (
    <div
      data-testid={`settings-project-card-${slug}`}
      className="fd-card"
      style={{
        display: "flex",
        flexDirection: "column",
        gap: "8px",
        padding: "10px 12px",
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: "8px",
          flexWrap: "wrap",
        }}
      >
        <h3
          style={{
            fontSize: "13px",
            fontWeight: 700,
            margin: 0,
            color: "inherit",
            lineHeight: "inherit",
          }}
        >
          {project.name}
        </h3>
        <span className="fd-badge fd-mono" style={{ fontSize: "10.5px" }}>
          {slug}
        </span>
        <span
          data-testid={`settings-project-kind-${slug}`}
          className="fd-badge"
          style={{ fontSize: "10.5px" }}
        >
          {isVirtual ? "Virtual Group" : "Project"}
        </span>
        {project.has_git && (
          <span className="fd-badge" style={{ fontSize: "10.5px" }}>
            Git
          </span>
        )}
        <span
          className="fd-badge fd-mono"
          style={{ fontSize: "10.5px", marginLeft: "auto" }}
        >
          {count} active agent{count === 1 ? "" : "s"}
        </span>
      </div>

      {project.purpose && (
        <div style={{ fontSize: "12px", color: "var(--text-secondary)" }}>
          {project.purpose}
        </div>
      )}

      <div style={{ display: "flex", gap: "6px", flexWrap: "wrap" }}>
        <span style={LABEL_STYLE}>Directory:</span>
        <code className="fd-mono" style={{ fontSize: "11.5px" }}>
          {isVirtual ? "Workspace Root" : (project.path ?? "")}
        </code>
      </div>

      {project.tracker && (
        <div style={{ display: "flex", gap: "6px", flexWrap: "wrap" }}>
          <span style={LABEL_STYLE}>GitHub:</span>
          <code className="fd-mono" style={{ fontSize: "11.5px" }}>
            {project.tracker.repo}
          </code>
          <span className="fd-badge fd-mono" style={{ fontSize: "10px" }}>
            {project.tracker.credential}
          </span>
        </div>
      )}

      <ProjectArtifactsDirRow
        slug={slug}
        displayName={project.name}
        artifactsDir={props.artifactsDir}
      />

      <div style={{ display: "flex", gap: "6px", justifyContent: "flex-end" }}>
        <button
          type="button"
          data-testid={`settings-project-edit-${slug}`}
          aria-label={`Edit ${project.name}`}
          className="fd-btn"
          style={BUTTON_STYLE}
          onClick={() => props.onEdit?.(project)}
        >
          Edit
        </button>
        <button
          type="button"
          data-testid={`settings-project-view-agents-${slug}`}
          aria-label={`View agents for ${project.name}`}
          className="fd-btn"
          style={BUTTON_STYLE}
          onClick={() => props.onViewAgents(slug)}
        >
          View Agents
        </button>
        <button
          type="button"
          data-testid={`settings-project-delete-${slug}`}
          aria-label={`Delete ${project.name}`}
          className="fd-btn"
          style={BUTTON_STYLE}
          onClick={() => props.onDelete?.(project)}
        >
          Delete
        </button>
      </div>
    </div>
  );
}

export function ProjectsSettingsTab(props: ProjectsSettingsTabProps): ReactNode {
  const { projects, loading, error } = props;
  return (
    <div
      data-testid="settings-projects-tab"
      style={{ display: "flex", flexDirection: "column", gap: "12px" }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "flex-end",
        }}
      >
        <button
          type="button"
          data-testid="settings-projects-create-btn"
          className="fd-btn fd-btn-active"
          style={BUTTON_STYLE}
          onClick={(e) => props.onCreate?.(e.currentTarget)}
        >
          + Create Project / Group
        </button>
      </div>

      {error ? (
        <div
          role="alert"
          data-testid="settings-projects-error"
          className="fd-card"
          style={{ fontSize: "12px", color: "var(--text-secondary)" }}
        >
          Could not load projects: {error}
        </div>
      ) : loading && projects.length === 0 ? (
        <div
          data-testid="settings-projects-loading"
          style={{ fontSize: "12px", color: "var(--text-muted)" }}
        >
          Loading projects...
        </div>
      ) : projects.length === 0 ? (
        <div
          data-testid="settings-projects-empty"
          className="fd-card"
          style={{ fontSize: "12px", color: "var(--text-muted)" }}
        >
          No projects registered yet.
        </div>
      ) : (
        projects.map((p) => <ProjectCard key={p.slug} project={p} props={props} />)
      )}
    </div>
  );
}
