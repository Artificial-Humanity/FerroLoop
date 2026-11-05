import { useState } from "react";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import {
  NewProjectModal,
  deriveProjectSlug,
} from "../components/settings/NewProjectModal";
import type { CreateProjectRequest, ProjectSummary } from "../types/api";

const CREATED: ProjectSummary = {
  id: "alpha",
  slug: "alpha",
  name: "Alpha",
  kind: "project",
  path: "/work/alpha",
  has_git: false,
  purpose: null,
  tracker: null,
  store_path: null,
  active_agent_count: 0,
};

function setup(
  createProject: (req: CreateProjectRequest) => Promise<ProjectSummary> = async () =>
    CREATED,
  existingSlugs: string[] = ["taken"],
) {
  const create = vi.fn(createProject);
  const onClose = vi.fn();
  const onCreated = vi.fn();
  const view = render(
    <NewProjectModal
      isOpen
      apiClient={{ createProject: create }}
      existingSlugs={existingSlugs}
      onClose={onClose}
      onCreated={onCreated}
    />,
  );
  return { create, onClose, onCreated, ...view };
}

function type(testId: string, value: string) {
  fireEvent.change(screen.getByTestId(testId), { target: { value } });
}

async function submit() {
  await act(async () => {
    fireEvent.click(screen.getByTestId("new-project-submit"));
  });
}

function fillProject() {
  type("new-project-name", "Alpha");
  type("new-project-path", "/work/alpha");
}

describe("deriveProjectSlug", () => {
  it("lowercases, collapses non-alphanumerics and trims dashes", () => {
    expect(deriveProjectSlug("  My Cool_Project!! ")).toBe("my-cool-project");
    expect(deriveProjectSlug("--A  b--")).toBe("a-b");
    expect(deriveProjectSlug("***")).toBe("");
  });
});

describe("NewProjectModal", () => {
  it("renders nothing when closed", () => {
    render(
      <NewProjectModal
        isOpen={false}
        apiClient={{ createProject: vi.fn() }}
        existingSlugs={[]}
        onClose={vi.fn()}
        onCreated={vi.fn()}
      />,
    );
    expect(screen.queryByTestId("new-project-modal")).toBeNull();
  });

  it("is an accessible dialog with initial focus on Name", () => {
    setup();
    const dialog = screen.getByRole("dialog");
    expect(dialog).toHaveAttribute("aria-modal", "true");
    expect(dialog).toHaveAccessibleName("New Project / Virtual Group");
    expect(screen.getByTestId("new-project-name")).toHaveFocus();
  });

  it("defaults to Directory Project with its full field set", () => {
    setup();
    expect(screen.getByTestId("new-project-kind-project")).toBeChecked();
    expect(screen.getByTestId("new-project-kind-virtual")).not.toBeChecked();
    for (const id of [
      "name",
      "slug",
      "path",
      "purpose",
      "git-init",
      "github",
    ]) {
      expect(screen.getByTestId(`new-project-${id}`)).toBeInTheDocument();
    }
    expect(screen.getByTestId("new-project-git-init")).not.toBeChecked();
    expect(screen.getByText(/Git is not required/)).toBeInTheDocument();
    // Credential only appears once a tracker is entered.
    expect(screen.queryByTestId("new-project-credential")).toBeNull();
    type("new-project-github", "acme/repo");
    expect(screen.getByTestId("new-project-credential")).toHaveValue("app");
  });

  it("renders only name, slug and purpose for a Virtual Group", () => {
    setup();
    fireEvent.click(screen.getByTestId("new-project-kind-virtual"));
    for (const id of ["name", "slug", "purpose"]) {
      expect(screen.getByTestId(`new-project-${id}`)).toBeInTheDocument();
    }
    for (const id of ["path", "git-init", "github", "credential"]) {
      expect(screen.queryByTestId(`new-project-${id}`)).toBeNull();
    }
  });

  it("derives the slug from the name until the slug is edited", () => {
    setup();
    type("new-project-name", "Research Lab 2");
    expect(screen.getByTestId("new-project-slug")).toHaveValue("research-lab-2");
    type("new-project-slug", "my-slug");
    type("new-project-name", "Something Else");
    expect(screen.getByTestId("new-project-slug")).toHaveValue("my-slug");
  });

  it("sends the exact body for a full Directory Project", async () => {
    const { create, onCreated, onClose } = setup();
    type("new-project-name", "  Alpha Project ");
    type("new-project-path", " /work/alpha ");
    type("new-project-purpose", "  Does alpha things ");
    fireEvent.click(screen.getByTestId("new-project-git-init"));
    type("new-project-github", " acme/alpha.repo ");
    fireEvent.change(screen.getByTestId("new-project-credential"), {
      target: { value: "env" },
    });
    await submit();
    expect(create).toHaveBeenCalledTimes(1);
    expect(create.mock.calls[0][0]).toStrictEqual({
      name: "Alpha Project",
      slug: "alpha-project",
      kind: "project",
      purpose: "Does alpha things",
      path: "/work/alpha",
      git_init: true,
      github: "acme/alpha.repo",
      credential: "env",
    });
    expect(onCreated).toHaveBeenCalledWith(CREATED);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("sends only name, slug, kind and path for a minimal Directory Project", async () => {
    const { create } = setup();
    fillProject();
    await submit();
    expect(create.mock.calls[0][0]).toStrictEqual({
      name: "Alpha",
      slug: "alpha",
      kind: "project",
      path: "/work/alpha",
    });
  });

  it("accepts a home-relative path", async () => {
    const { create } = setup();
    type("new-project-name", "Alpha");
    type("new-project-path", "~/work/alpha");
    await submit();
    expect(create).toHaveBeenCalledTimes(1);
  });

  it("omits git_init and credential when git init is off and no tracker is set", async () => {
    const { create } = setup();
    fillProject();
    fireEvent.click(screen.getByTestId("new-project-git-init"));
    fireEvent.click(screen.getByTestId("new-project-git-init"));
    await submit();
    const body = create.mock.calls[0][0];
    expect(body).not.toHaveProperty("git_init");
    expect(body).not.toHaveProperty("credential");
    expect(body).not.toHaveProperty("github");
  });

  it("sends a Virtual Group without the Directory Project fields, even if typed before", async () => {
    const { create } = setup();
    fillProject();
    fireEvent.click(screen.getByTestId("new-project-git-init"));
    type("new-project-github", "acme/alpha");
    fireEvent.change(screen.getByTestId("new-project-credential"), {
      target: { value: "env" },
    });
    type("new-project-purpose", "Council scope");
    fireEvent.click(screen.getByTestId("new-project-kind-virtual"));
    // Name, slug and purpose survive the switch.
    expect(screen.getByTestId("new-project-name")).toHaveValue("Alpha");
    expect(screen.getByTestId("new-project-slug")).toHaveValue("alpha");
    expect(screen.getByTestId("new-project-purpose")).toHaveValue("Council scope");
    await submit();
    expect(create.mock.calls[0][0]).toStrictEqual({
      name: "Alpha",
      slug: "alpha",
      kind: "virtual",
      purpose: "Council scope",
    });
    // Switching back would show the hidden fields empty again.
  });

  it("discards hidden field values when switching back to Directory Project", () => {
    setup();
    type("new-project-path", "/work/alpha");
    fireEvent.click(screen.getByTestId("new-project-kind-virtual"));
    fireEvent.click(screen.getByTestId("new-project-kind-project"));
    expect(screen.getByTestId("new-project-path")).toHaveValue("");
    expect(screen.getByTestId("new-project-git-init")).not.toBeChecked();
  });

  describe("validation blocks the request", () => {
    it("requires a name", async () => {
      const { create } = setup();
      type("new-project-path", "/work/x");
      await submit();
      expect(create).not.toHaveBeenCalled();
      const name = screen.getByTestId("new-project-name");
      expect(name).toHaveAttribute("aria-invalid", "true");
      expect(name).toHaveAccessibleDescription("Name is required.");
      expect(name).toHaveFocus();
    });

    it("requires a slug", async () => {
      const { create } = setup();
      fillProject();
      type("new-project-slug", "");
      await submit();
      expect(create).not.toHaveBeenCalled();
      expect(screen.getByTestId("new-project-slug")).toHaveAccessibleDescription(
        "Slug is required.",
      );
    });

    it("rejects a malformed slug", async () => {
      const { create } = setup();
      fillProject();
      type("new-project-slug", "-Bad_Slug");
      await submit();
      expect(create).not.toHaveBeenCalled();
      expect(screen.getByTestId("new-project-slug")).toHaveAttribute(
        "aria-invalid",
        "true",
      );
    });

    it("rejects a duplicate slug client-side", async () => {
      const { create } = setup();
      type("new-project-name", "Taken");
      type("new-project-path", "/work/taken");
      await submit();
      expect(create).not.toHaveBeenCalled();
      expect(screen.getByTestId("new-project-slug")).toHaveAccessibleDescription(
        /already exists/,
      );
    });

    it("requires a path for a Directory Project", async () => {
      const { create } = setup();
      type("new-project-name", "Alpha");
      await submit();
      expect(create).not.toHaveBeenCalled();
      expect(screen.getByTestId("new-project-path")).toHaveAccessibleDescription(
        "Directory path is required.",
      );
    });

    it("asks for an absolute path", async () => {
      const { create } = setup();
      type("new-project-name", "Alpha");
      type("new-project-path", "relative/dir");
      await submit();
      expect(create).not.toHaveBeenCalled();
      expect(screen.getByTestId("new-project-path")).toHaveAccessibleDescription(
        /absolute/,
      );
    });

    it("rejects a malformed GitHub tracker but allows it empty", async () => {
      const { create } = setup();
      fillProject();
      type("new-project-github", "not a repo");
      await submit();
      expect(create).not.toHaveBeenCalled();
      expect(screen.getByTestId("new-project-github")).toHaveAttribute(
        "aria-invalid",
        "true",
      );
      type("new-project-github", "");
      await submit();
      expect(create).toHaveBeenCalledTimes(1);
    });

    it("does not require a path for a Virtual Group", async () => {
      const { create } = setup();
      fireEvent.click(screen.getByTestId("new-project-kind-virtual"));
      type("new-project-name", "Council");
      await submit();
      expect(create).toHaveBeenCalledTimes(1);
    });
  });

  it("shows the server error and keeps the values", async () => {
    const { create, onClose, onCreated } = setup(async () => {
      throw new Error("slug alpha already exists (409)");
    });
    fillProject();
    type("new-project-purpose", "Keep me");
    await submit();
    expect(create).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("alert")).toHaveTextContent(
      "slug alpha already exists (409)",
    );
    expect(screen.getByTestId("new-project-error")).toBeInTheDocument();
    expect(screen.getByTestId("new-project-name")).toHaveValue("Alpha");
    expect(screen.getByTestId("new-project-path")).toHaveValue("/work/alpha");
    expect(screen.getByTestId("new-project-purpose")).toHaveValue("Keep me");
    expect(screen.getByTestId("new-project-submit")).not.toBeDisabled();
    expect(onClose).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
  });

  it("disables the form and buttons while busy and ignores close gestures", async () => {
    let resolve!: (p: ProjectSummary) => void;
    const { onClose } = setup(
      () => new Promise<ProjectSummary>((r) => (resolve = r)),
    );
    fillProject();
    await submit();
    for (const id of [
      "kind-project",
      "kind-virtual",
      "name",
      "slug",
      "path",
      "purpose",
      "git-init",
      "github",
      "submit",
      "cancel",
    ]) {
      expect(screen.getByTestId(`new-project-${id}`)).toBeDisabled();
    }
    fireEvent.keyDown(window, { key: "Escape" });
    fireEvent.click(screen.getByTestId("new-project-modal-backdrop"));
    expect(onClose).not.toHaveBeenCalled();
    await act(async () => resolve(CREATED));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("closes on Escape, backdrop click and Cancel but not on dialog clicks", () => {
    const { onClose } = setup();
    fireEvent.click(screen.getByTestId("new-project-modal"));
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByTestId("new-project-modal-backdrop"));
    expect(onClose).toHaveBeenCalledTimes(2);
    fireEvent.click(screen.getByTestId("new-project-cancel"));
    expect(onClose).toHaveBeenCalledTimes(3);
  });

  it("traps Tab inside the dialog", () => {
    setup();
    const dialog = screen.getByTestId("new-project-modal");
    const submitBtn = screen.getByTestId("new-project-submit");
    const first = screen.getByTestId("new-project-kind-project");
    submitBtn.focus();
    fireEvent.keyDown(dialog, { key: "Tab" });
    expect(first).toHaveFocus();
    fireEvent.keyDown(dialog, { key: "Tab", shiftKey: true });
    expect(submitBtn).toHaveFocus();
  });

  it("returns focus to the opener when it closes", () => {
    function Host() {
      const [open, setOpen] = useState(false);
      return (
        <>
          <button data-testid="opener" onClick={() => setOpen(true)}>
            open
          </button>
          <NewProjectModal
            isOpen={open}
            apiClient={{ createProject: vi.fn() }}
            existingSlugs={[]}
            onClose={() => setOpen(false)}
            onCreated={vi.fn()}
          />
        </>
      );
    }
    render(<Host />);
    const opener = screen.getByTestId("opener");
    opener.focus();
    fireEvent.click(opener);
    expect(screen.getByTestId("new-project-name")).toHaveFocus();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByTestId("new-project-modal")).toBeNull();
    expect(opener).toHaveFocus();
  });
});
