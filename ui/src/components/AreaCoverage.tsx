import React from "react";
import type { AgentRecord, AreaRecord, ProjectSummary } from "../types/api";
import { projectLabel } from "../hooks/useProjects";

export const projectReferenceKey = (value: string) => value.trim().toLowerCase();

interface AreaReference {
  slug: string;
  label: string;
  virtual: boolean;
  project?: ProjectSummary;
}

/** Index project identities once and prefer the registry when both sources report a slug. */
export function areaReferenceIndex(
  projects: readonly ProjectSummary[],
  areas: readonly AreaRecord[],
) {
  const bySlug = new Map<string, AreaReference>();
  const byLabel = new Map<string, AreaReference | null>();
  function add(reference: AreaReference) {
    if (bySlug.has(reference.slug)) return;
    bySlug.set(reference.slug, reference);
    const label = projectReferenceKey(reference.label);
    byLabel.set(label, byLabel.has(label) ? null : reference);
  }
  for (const project of projects) {
    add({
      slug: projectReferenceKey(project.slug),
      label: projectLabel(project),
      virtual: project.kind === "virtual",
      project,
    });
  }
  for (const area of areas) {
    add({
      slug: projectReferenceKey(area.area_key),
      label: area.name.trim() || area.area_key,
      virtual: area.path === null,
    });
  }

  return {
    bySlug,
    resolveLabel: (raw: string) => byLabel.get(projectReferenceKey(raw)) ?? undefined,
    isAmbiguousLabel: (raw: string) => byLabel.get(projectReferenceKey(raw)) === null,
    resolve(raw: string) {
      const key = projectReferenceKey(raw);
      return bySlug.get(key) ?? byLabel.get(key) ?? undefined;
    },
  };
}

type AreaReferences = ReturnType<typeof areaReferenceIndex>;

export function extraAreaKeys(agent: AgentRecord): string[] {
  const seen = new Set([projectReferenceKey(agent.project)]);
  return (agent.extra_areas ?? []).filter(area => {
    const normalized = projectReferenceKey(area);
    if (!normalized || seen.has(normalized)) return false;
    seen.add(normalized);
    return true;
  });
}

export function AreaBadge({ slug, references, primary = false }: {
  slug: string;
  references: AreaReferences;
  primary?: boolean;
}) {
  const reference = references.resolve(slug);
  const label = reference?.label ?? slug.trim();
  const virtual = reference?.virtual ?? false;
  return (
    <span
      className="fd-badge fd-mono"
      data-virtual={virtual ? "true" : "false"}
      title={virtual ? "Virtual area" : primary ? "Primary area" : "Area"}
      style={{ borderStyle: virtual ? "dashed" : "solid", fontSize: "10px" }}
    >
      {primary ? "Primary: " : ""}{label}{virtual ? " · virtual" : ""}
    </span>
  );
}

export function CoverageChips({ agent, references }: { agent: AgentRecord; references: AreaReferences }) {
  const extra = extraAreaKeys(agent);
  if (!extra.length) return null;
  return <span aria-label="Additional coverage" style={{ display: "inline-flex", gap: "4px", flexWrap: "wrap" }}>
    {extra.map(slug => <AreaBadge key={projectReferenceKey(slug)} slug={slug} references={references} />)}
  </span>;
}
