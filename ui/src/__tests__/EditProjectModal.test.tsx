import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { EditProjectModal } from "../components/settings/EditProjectModal";
import type { ProjectSummary, UpdateProjectRequest } from "../types/api";

const PROJECT: ProjectSummary = {
  id: "alpha",
  slug: "alpha",
  name: "Alpha",
  kind: "project",
  path: "/work/alpha",
  has_git: false,
  purpose: "Original purpose",
  tracker: { kind: "github", repo: "acme/alpha", credential: "app" },
  store_path: null,
  active_agent_count: 0,
};

const BARE: ProjectSummary = { ...PROJECT, tracker: null, purpose: null };

const GROUP: ProjectSummary = {
  ...PROJECT,
  id: "council",
  slug: "council",
  name: "Council",
  kind: "virtual",
  path: null,
  tracker: null,
};

function setup(
  project: ProjectSummary = PROJECT,
  updateProject: (
    slug: string,
    req: UpdateProjectRequest,
  ) => Promise<ProjectSummary> = async (_slug, req) => ({ ...project, ...req }) as ProjectSummary,
) {
  const update = vi.fn(updateProject);
  const onClose = vi.fn();
  const onSaved = vi.fn();
  const view = render(
    <EditProjectModal
      isOpen
      project={project}
      apiClient={{ updateProject: update }}
      onClose={onClose}
      onSaved={onSaved}
    />,
  );
  return { update, onClose, onSaved, ...view };
}

function type(testId: string, value: string) {
  fireEvent.change(screen.getByTestId(testId), { target: { value } });
}

async function save() {
  await act(async () => {
    fireEvent.click(screen.getByTestId("edit-project-submit"));
  });
}

describe("EditProjectModal", () => {
  it("renders nothing when closed or without a project", () => {
    const { rerender } = render(
      <EditProjectModal
        isOpen={false}
        project={PROJECT}
        apiClient={{ updateProject: vi.fn() }}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />,
    );
    expect(screen.queryByTestId("edit-project-modal")).toBeNull();
    rerender(
      <EditProjectModal
        isOpen
        project={null}
        apiClient={{ updateProject: vi.fn() }}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />,
    );
    expect(screen.queryByTestId("edit-project-modal")).toBeNull();
  });

  it("is an accessible dialog prefilled from the project, focused on Name", () => {
    setup();
    const dialog = screen.getByRole("dialog");
    expect(dialog).toHaveAttribute("aria-modal", "true");
    expect(dialog).toHaveAccessibleName("Edit Alpha");
    expect(screen.getByTestId("edit-project-name")).toHaveFocus();
    expect(screen.getByTestId("edit-project-name")).toHaveValue("Alpha");
    expect(screen.getByTestId("edit-project-purpose")).toHaveValue("Original purpose");
    expect(screen.getByTestId("edit-project-path")).toHaveValue("/work/alpha");
    expect(screen.getByTestId("edit-project-github")).toHaveValue("acme/alpha");
    expect(screen.getByTestId("edit-project-credential")).toHaveValue("app");
  });

  it("shows kind and slug read-only and warns that files are not moved", () => {
    setup();
    expect(screen.getByTestId("edit-project-kind")).toHaveTextContent(
      "Directory Project",
    );
    expect(screen.getByTestId("edit-project-slug")).toHaveTextContent("alpha");
    expect(screen.getByTestId("edit-project-path")).toHaveAccessibleDescription(
      "Changes the registered directory. Files on disk are not moved.",
    );
  });

  it("disables Save and sends nothing when nothing changed", async () => {
    const { update } = setup();
    const submit = screen.getByTestId("edit-project-submit");
    expect(submit).toBeDisabled();
    fireEvent.submit(submit.closest("form")!);
    await act(async () => {});
    expect(update).not.toHaveBeenCalled();
    // Editing and reverting is still "no change"; whitespace alone is too.
    type("edit-project-name", "Beta");
    expect(submit).toBeEnabled();
    type("edit-project-name", "  Alpha ");
    expect(submit).toBeDisabled();
  });

  it("sends only the name when only the name changed", async () => {
    const { update, onSaved, onClose } = setup();
    type("edit-project-name", "  Alpha Two ");
    await save();
    expect(update).toHaveBeenCalledTimes(1);
    expect(update).toHaveBeenCalledWith("alpha", { name: "Alpha Two" });
    expect(onSaved).toHaveBeenCalledTimes(1);
    expect(onSaved.mock.calls[0][0]).toMatchObject({ slug: "alpha", name: "Alpha Two" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("sends only the purpose when only the purpose changed", async () => {
    const { update } = setup();
    type("edit-project-purpose", " New purpose ");
    await save();
    expect(update).toHaveBeenLastCalledWith("alpha", { purpose: "New purpose" });
  });

  it("sends an empty purpose when it is cleared", async () => {
    const { update } = setup();
    type("edit-project-purpose", "");
    await save();
    expect(update).toHaveBeenCalledWith("alpha", { purpose: "" });
  });

  it("sends only the path when only the path changed", async () => {
    const { update } = setup();
    type("edit-project-path", " /work/moved ");
    await save();
    expect(update).toHaveBeenCalledWith("alpha", { path: "/work/moved" });
  });

  it("sends the new repo only when the tracker changes and the credential does not", async () => {
    const { update } = setup();
    type("edit-project-github", "acme/other");
    await save();
    expect(update).toHaveBeenCalledWith("alpha", { github: "acme/other" });
  });

  it("sends only the credential when only the credential changed", async () => {
    const { update } = setup();
    fireEvent.change(screen.getByTestId("edit-project-credential"), {
      target: { value: "env" },
    });
    await save();
    expect(update).toHaveBeenCalledWith("alpha", { credential: "env" });
  });

  it("clearing the tracker sends an empty github and nothing else", async () => {
    const { update } = setup();
    type("edit-project-github", "");
    expect(screen.queryByTestId("edit-project-credential")).toBeNull();
    await save();
    expect(update).toHaveBeenCalledWith("alpha", { github: "" });
  });

  it("adding a tracker sends the repo with its credential", async () => {
    const { update } = setup(BARE);
    expect(screen.queryByTestId("edit-project-credential")).toBeNull();
    type("edit-project-github", "acme/alpha");
    fireEvent.change(screen.getByTestId("edit-project-credential"), {
      target: { value: "env" },
    });
    await save();
    expect(update).toHaveBeenCalledWith("alpha", {
      github: "acme/alpha",
      credential: "env",
    });
  });

  it("adding a tracker with the default credential still names the credential", async () => {
    const { update } = setup(BARE);
    type("edit-project-github", "acme/alpha");
    await save();
    expect(update).toHaveBeenCalledWith("alpha", {
      github: "acme/alpha",
      credential: "app",
    });
  });

  it("sends every changed field together", async () => {
    const { update } = setup();
    type("edit-project-name", "Gamma");
    type("edit-project-purpose", "P");
    type("edit-project-path", "/work/gamma");
    type("edit-project-github", "acme/gamma");
    await save();
    expect(update).toHaveBeenCalledWith("alpha", {
      name: "Gamma",
      purpose: "P",
      path: "/work/gamma",
      github: "acme/gamma",
    });
  });

  it("shows only Name and Purpose for a Virtual Group", async () => {
    const { update } = setup(GROUP);
    for (const id of ["name", "purpose", "kind", "slug"]) {
      expect(screen.getByTestId(`edit-project-${id}`)).toBeInTheDocument();
    }
    for (const id of ["path", "github", "credential"]) {
      expect(screen.queryByTestId(`edit-project-${id}`)).toBeNull();
    }
    expect(screen.getByTestId("edit-project-kind")).toHaveTextContent("Virtual Group");
    type("edit-project-name", "Council 2");
    await save();
    expect(update).toHaveBeenCalledWith("council", { name: "Council 2" });
  });

  describe("validation", () => {
    it("requires a name", async () => {
      const { update } = setup();
      type("edit-project-name", "   ");
      await save();
      expect(update).not.toHaveBeenCalled();
      expect(screen.getByTestId("edit-project-name")).toHaveAttribute(
        "aria-invalid",
        "true",
      );
      expect(screen.getByTestId("edit-project-name")).toHaveAccessibleDescription(
        "Name is required.",
      );
      expect(screen.getByTestId("edit-project-name")).toHaveFocus();
    });

    it("requires an absolute path", async () => {
      const { update } = setup();
      type("edit-project-path", "~/work/alpha");
      await save();
      expect(update).not.toHaveBeenCalled();
      expect(screen.getByTestId("edit-project-path")).toHaveAccessibleDescription(
        /must be absolute/,
      );
      type("edit-project-path", "");
      await save();
      expect(update).not.toHaveBeenCalled();
      expect(screen.getByTestId("edit-project-path")).toHaveAccessibleDescription(
        /Directory path is required/,
      );
    });

    it.each(["acme", "acme/alpha.git", "a b/c", "../x", "acme/.."])(
      "rejects the tracker %s",
      async (value) => {
        const { update } = setup();
        type("edit-project-github", value);
        await save();
        expect(update).not.toHaveBeenCalled();
        expect(screen.getByTestId("edit-project-github")).toHaveAccessibleDescription(
          "GitHub tracker must look like owner/repo.",
        );
      },
    );

    describe("legacy stored values", () => {
      const LEGACY: ProjectSummary = {
        ...PROJECT,
        path: "relative/alpha",
        tracker: { kind: "github", repo: "acme/alpha.git", credential: "app" },
      };

      it("renames a project whose stored path and tracker would not validate", async () => {
        const { update, onSaved } = setup(LEGACY);
        type("edit-project-name", "Alpha Two");
        await save();
        expect(update).toHaveBeenCalledTimes(1);
        expect(update).toHaveBeenCalledWith("alpha", { name: "Alpha Two" });
        expect(onSaved).toHaveBeenCalledTimes(1);
      });

      it("still validates a legacy path once it is edited", async () => {
        const { update } = setup(LEGACY);
        type("edit-project-path", "still/relative");
        await save();
        expect(update).not.toHaveBeenCalled();
        expect(screen.getByTestId("edit-project-path")).toHaveAccessibleDescription(
          /must be absolute/,
        );
      });

      it("still validates a legacy tracker once it is edited", async () => {
        const { update } = setup(LEGACY);
        type("edit-project-github", "acme/other.git");
        await save();
        expect(update).not.toHaveBeenCalled();
        expect(screen.getByTestId("edit-project-github")).toHaveAccessibleDescription(
          "GitHub tracker must look like owner/repo.",
        );
      });
    });

    it("does not show errors before the first save attempt", () => {
      setup();
      type("edit-project-name", "");
      expect(screen.getByTestId("edit-project-name")).not.toHaveAttribute(
        "aria-invalid",
      );
    });
  });

  describe("failures and busy state", () => {
    it("shows the server error, keeps the dialog and its input, and parks focus on Save", async () => {
      const { onSaved, onClose } = setup(PROJECT, async () => {
        throw new Error("path does not exist");
      });
      type("edit-project-name", "Beta");
      screen.getByTestId("edit-project-name").focus();
      await save();
      expect(screen.getByTestId("edit-project-error")).toHaveAttribute("role", "alert");
      expect(screen.getByTestId("edit-project-error")).toHaveTextContent(
        "path does not exist",
      );
      expect(screen.getByTestId("edit-project-name")).toHaveValue("Beta");
      expect(onSaved).not.toHaveBeenCalled();
      expect(onClose).not.toHaveBeenCalled();
      expect(screen.getByTestId("edit-project-submit")).toHaveFocus();
    });

    it("blocks a double submit and locks Escape, backdrop and Cancel while saving", async () => {
      let resolve!: (p: ProjectSummary) => void;
      const { update, onClose, onSaved } = setup(
        PROJECT,
        () => new Promise<ProjectSummary>((r) => (resolve = r)),
      );
      type("edit-project-name", "Beta");
      const form = screen.getByTestId("edit-project-submit").closest("form")!;
      await act(async () => {
        fireEvent.submit(form);
        fireEvent.submit(form);
      });
      expect(update).toHaveBeenCalledTimes(1);
      expect(screen.getByTestId("edit-project-cancel")).toBeDisabled();
      fireEvent.keyDown(window, { key: "Escape" });
      fireEvent.click(screen.getByTestId("edit-project-modal-backdrop"));
      expect(onClose).not.toHaveBeenCalled();
      await act(async () => resolve({ ...PROJECT, name: "Beta" }));
      expect(onSaved).toHaveBeenCalledTimes(1);
      expect(onClose).toHaveBeenCalledTimes(1);
    });
  });

  describe("closing", () => {
    it("closes on Escape, backdrop click and Cancel without sending anything", () => {
      const { update, onClose } = setup();
      type("edit-project-name", "Beta");
      fireEvent.keyDown(window, { key: "Escape" });
      expect(onClose).toHaveBeenCalledTimes(1);
      fireEvent.click(screen.getByTestId("edit-project-modal-backdrop"));
      expect(onClose).toHaveBeenCalledTimes(2);
      fireEvent.click(screen.getByTestId("edit-project-cancel"));
      expect(onClose).toHaveBeenCalledTimes(3);
      fireEvent.click(screen.getByTestId("edit-project-modal"));
      expect(onClose).toHaveBeenCalledTimes(3);
      expect(update).not.toHaveBeenCalled();
    });

    it("traps Tab inside the dialog", () => {
      setup();
      type("edit-project-name", "Beta");
      const dialog = screen.getByTestId("edit-project-modal");
      const submit = screen.getByTestId("edit-project-submit");
      submit.focus();
      fireEvent.keyDown(dialog, { key: "Tab" });
      expect(screen.getByTestId("edit-project-name")).toHaveFocus();
      fireEvent.keyDown(dialog, { key: "Tab", shiftKey: true });
      expect(submit).toHaveFocus();
    });
  });
});
