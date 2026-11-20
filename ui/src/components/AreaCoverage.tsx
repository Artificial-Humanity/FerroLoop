import React from "react";
import type { AgentRecord, AreaRecord, ProjectSummary } from "../types/api";
import { projectLabel } from "../hooks/useProjects";

const key = (value: string) => value.trim().toLowerCase();

/** Resolve display names without inferring that unknown areas are virtual. */
export function areaInfo(
  slug: string,
  areas: readonly AreaRecord[],
  projects: readonly ProjectSummary[],
) {
  const normalized = key(slug);
  const exactProject = projects.find(project => key(project.slug) === normalized);
  const exactArea = areas.find(area => key(area.area_key) === normalized);
  // A known area key cannot refer to a different project's display name.
  const project = exactProject ?? (exactArea ? undefined : projects.find(
    project => key(projectLabel(project)) === normalized,
  ));
  const area = exactArea ?? (project ? undefined : areas.find(
    area => key(area.name) === normalized,
  ));
  return {
    label: project ? projectLabel(project) : area?.name || slug,
    // The project registry also knows configured projects whose area has no path.
    virtual: project ? project.kind === "virtual" : area?.path === null,
  };
}

export function extraAreaKeys(agent: AgentRecord): string[] {
  const seen = new Set([key(agent.project)]);
  return (agent.extra_areas ?? []).filter(area => {
    const normalized = key(area);
    if (!normalized || seen.has(normalized)) return false;
    seen.add(normalized);
    return true;
  });
}

export function AreaBadge({ slug, areas, projects, primary = false }: {
  slug: string;
  areas: readonly AreaRecord[];
  projects: readonly ProjectSummary[];
  primary?: boolean;
}) {
  const info = areaInfo(slug, areas, projects);
  return (
    <span
      className="fd-badge fd-mono"
      data-virtual={info.virtual ? "true" : "false"}
      title={info.virtual ? "Virtual area" : primary ? "Primary area" : "Area"}
      style={{ borderStyle: info.virtual ? "dashed" : "solid", fontSize: "10px" }}
    >
      {primary ? "Primary: " : ""}{info.label}{info.virtual ? " · virtual" : ""}
    </span>
  );
}

export function CoverageChips({ agent, areas, projects }: { agent: AgentRecord; areas: readonly AreaRecord[]; projects: readonly ProjectSummary[] }) {
  const extra = extraAreaKeys(agent);
  if (!extra.length) return null;
  return <span aria-label="Additional coverage" style={{ display: "inline-flex", gap: "4px", flexWrap: "wrap" }}>
    {extra.map(slug => <AreaBadge key={key(slug)} slug={slug} areas={areas} projects={projects} />)}
  </span>;
}
