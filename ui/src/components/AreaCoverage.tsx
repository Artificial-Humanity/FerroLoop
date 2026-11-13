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
  const area = areas.find(a => key(a.area_key) === key(slug) || key(a.name) === key(slug));
  const project = projects.find(p =>
    key(p.slug) === key(area?.area_key ?? slug) || key(projectLabel(p)) === key(slug),
  );
  return {
    label: area?.name || (project ? projectLabel(project) : slug),
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

export function coversArea(agent: AgentRecord, target: string): boolean {
  const normalized = key(target);
  return key(agent.project) === normalized || extraAreaKeys(agent).some(area => key(area) === normalized);
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
