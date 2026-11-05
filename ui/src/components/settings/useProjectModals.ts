import { useEffect, useRef, useState } from "react";
import type { Dispatch, RefObject, SetStateAction } from "react";
import type { DeleteProjectResponse, ProjectSummary } from "../../types/api";
import { describeProjectDeletion } from "./DeleteProjectModal";

const CREATE_BUTTON = '[data-testid="settings-projects-create-btn"]';

/**
 * State and handlers for the Edit and Delete project dialogs of the Settings
 * Projects tab. Both are reset when Settings closes.
 */
export function useProjectModals({
  isOpen,
  projectList,
  setProjectList,
  selectedProjectFilter,
  setSelectedProjectFilter,
  onProjectsChanged,
}: {
  isOpen: boolean;
  projectList: ProjectSummary[];
  setProjectList: Dispatch<SetStateAction<ProjectSummary[]>>;
  selectedProjectFilter: string;
  setSelectedProjectFilter: (value: string) => void;
  onProjectsChanged?: () => void;
}): {
  editTarget: ProjectSummary | null;
  deleteTarget: ProjectSummary | null;
  /** True while either dialog is open (it handles its own Escape). */
  anyOpen: boolean;
  deleteStatus: string | null;
  /** True when the last delete unregistered the project but its cleanup failed. */
  deleteWarning: boolean;
  editOpenerRef: RefObject<HTMLElement | null>;
  deleteOpenerRef: RefObject<HTMLElement | null>;
  openEdit: (project: ProjectSummary, opener: HTMLElement) => void;
  openDelete: (project: ProjectSummary, opener: HTMLElement) => void;
  closeEdit: () => void;
  closeDelete: () => void;
  onSaved: (saved: ProjectSummary) => void;
  onDeleted: (slug: string, response: DeleteProjectResponse) => void;
} {
  const [editTarget, setEditTarget] = useState<ProjectSummary | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<ProjectSummary | null>(null);
  const [deleteStatus, setDeleteStatus] = useState<string | null>(null);
  const [deleteWarning, setDeleteWarning] = useState(false);
  const editOpenerRef = useRef<HTMLElement | null>(null);
  const deleteOpenerRef = useRef<HTMLElement | null>(null);

  useEffect(() => {
    // Closing Settings must not leave a dialog armed for reopen.
    if (!isOpen) {
      setEditTarget(null);
      setDeleteTarget(null);
      setDeleteStatus(null);
      setDeleteWarning(false);
    }
  }, [isOpen]);

  return {
    editTarget,
    deleteTarget,
    anyOpen: editTarget !== null || deleteTarget !== null,
    deleteStatus,
    deleteWarning,
    editOpenerRef,
    deleteOpenerRef,
    openEdit: (project, opener) => {
      editOpenerRef.current = opener;
      setDeleteStatus(null);
      setDeleteWarning(false);
      setEditTarget(project);
    },
    openDelete: (project, opener) => {
      deleteOpenerRef.current = opener;
      setDeleteStatus(null);
      setDeleteWarning(false);
      setDeleteTarget(project);
    },
    closeEdit: () => setEditTarget(null),
    closeDelete: () => setDeleteTarget(null),
    onSaved: (saved) => {
      setProjectList((prev) =>
        prev.some((p) => p.slug === saved.slug)
          ? prev.map((p) => (p.slug === saved.slug ? saved : p))
          : [...prev, saved],
      );
      onProjectsChanged?.();
    },
    onDeleted: (slug, response) => {
      const gone = projectList.find((p) => p.slug === slug);
      setProjectList((prev) => prev.filter((p) => p.slug !== slug));
      // The Delete button is removed with its card; focus the tab's Create
      // button instead so it does not fall back to the page.
      const opener = deleteOpenerRef.current;
      const fallback = opener
        ?.closest('[data-testid="settings-projects-tab"]')
        ?.querySelector<HTMLElement>(CREATE_BUTTON);
      if (fallback) deleteOpenerRef.current = fallback;
      // The Agents filter may hold the slug or the display name.
      const filter = selectedProjectFilter.trim().toLowerCase();
      if (
        filter !== "all" &&
        (filter === slug.toLowerCase() ||
          (gone && filter === gone.name.trim().toLowerCase()))
      ) {
        setSelectedProjectFilter("all");
      }
      setDeleteStatus(describeProjectDeletion(gone?.name ?? slug, response));
      setDeleteWarning(Boolean(response.error));
      onProjectsChanged?.();
    },
  };
}
