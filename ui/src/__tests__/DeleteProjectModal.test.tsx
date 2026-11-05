import { useRef, useState } from "react";
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ProjectDeleteConflictError } from "../api/client";
import {
  DeleteProjectModal,
  describeProjectDeletion,
} from "../components/settings/DeleteProjectModal";
import type {
  DeleteProjectRequest,
  DeleteProjectResponse,
  ProjectSummary,
} from "../types/api";

const base = {
  has_git: false,
  purpose: null,
  tracker: null,
  store_path: null,
};

const IDLE: ProjectSummary = {
  ...base,
  id: "alpha",
  slug: "alpha",
  name: "Alpha",
  kind: "project",
  path: "/work/alpha",
  active_agent_count: 0,
};

const BUSY: ProjectSummary = {
  ...IDLE,
  active_agent_count: 2,
  store_path: "/work/alpha/.ferro/alpha.fls",
};

const GROUP: ProjectSummary = {
  ...base,
  id: "council",
  slug: "council",
  name: "Council",
  kind: "virtual",
  path: null,
  active_agent_count: 0,
};

const BETA: ProjectSummary = { ...IDLE, id: "beta", slug: "beta", name: "Beta" };

const OK: DeleteProjectResponse = {
  deleted: "alpha",
  reassigned_agents: 0,
  decommissioned_agents: 0,
  store_deleted: false,
  dir_deleted: false,
};

function setup(
  project: ProjectSummary = IDLE,
  deleteProject: (
    slug: string,
    req: DeleteProjectRequest,
  ) => Promise<DeleteProjectResponse> = async () => OK,
  others: ProjectSummary[] = [BUSY, BETA, GROUP],
) {
  const del = vi.fn(deleteProject);
  const onClose = vi.fn();
  const onDeleted = vi.fn();
  const view = render(
    <DeleteProjectModal
      isOpen
      project={project}
      otherProjects={others}
      apiClient={{ deleteProject: del }}
      onClose={onClose}
      onDeleted={onDeleted}
    />,
  );
  return { del, onClose, onDeleted, ...view };
}

const tid = (id: string) => screen.getByTestId(`delete-project-${id}`);
const maybe = (id: string) => screen.queryByTestId(`delete-project-${id}`);

async function confirm() {
  await act(async () => {
    fireEvent.click(tid("submit"));
  });
}

describe("DeleteProjectModal", () => {
  it("renders nothing when closed or without a project", () => {
    const { rerender } = render(
      <DeleteProjectModal
        isOpen={false}
        project={IDLE}
        otherProjects={[]}
        apiClient={{ deleteProject: vi.fn() }}
        onClose={vi.fn()}
        onDeleted={vi.fn()}
      />,
    );
    expect(maybe("modal")).toBeNull();
    rerender(
      <DeleteProjectModal
        isOpen
        project={null}
        otherProjects={[]}
        apiClient={{ deleteProject: vi.fn() }}
        onClose={vi.fn()}
        onDeleted={vi.fn()}
      />,
    );
    expect(maybe("modal")).toBeNull();
  });

  it("is an accessible dialog titled with the name and says delete only unregisters", () => {
    setup();
    const dialog = screen.getByRole("dialog");
    expect(dialog).toHaveAttribute("aria-modal", "true");
    expect(dialog).toHaveAccessibleName("Delete Alpha");
    expect(dialog).toHaveAccessibleDescription(
      /only unregisters the project from FerroLoop.*not delete any files on disk/,
    );
  });

  it("calls a virtual group a group in the explanation", () => {
    setup(GROUP);
    expect(screen.getByRole("dialog")).toHaveAccessibleDescription(
      /unregisters the group/,
    );
  });

  describe("agent guard", () => {
    it("is absent with 0 active agents, and Delete is enabled at once", () => {
      setup(IDLE);
      expect(maybe("guard")).toBeNull();
      expect(maybe("reassign-radio")).toBeNull();
      expect(tid("submit")).toBeEnabled();
    });

    it("is present with active agents; nothing is preselected and Delete is disabled", () => {
      setup(BUSY);
      expect(tid("guard")).toHaveTextContent("2 active agents");
      expect(tid("reassign-radio")).not.toBeChecked();
      expect(tid("decommission-radio")).not.toBeChecked();
      expect(tid("reassign-select")).toHaveValue("");
      expect(tid("submit")).toBeDisabled();
    });

    it("uses the singular for one agent", () => {
      setup({ ...BUSY, active_agent_count: 1 });
      expect(tid("guard")).toHaveTextContent("1 active agent in");
    });

    it("lists other entries by name and slug, never the project itself", () => {
      setup(BUSY);
      const options = within(tid("reassign-select"))
        .getAllByRole("option")
        .map((o) => o.getAttribute("value"));
      expect(options).toEqual(["", "beta", "council"]);
      expect(tid("reassign-select")).toHaveTextContent("Beta (beta)");
      expect(tid("reassign-select")).toHaveTextContent("Council (council)");
    });

    it("keeps Delete disabled for reassign until a target is chosen", () => {
      setup(BUSY);
      fireEvent.click(tid("reassign-radio"));
      expect(tid("reassign-select")).toBeEnabled();
      expect(tid("submit")).toBeDisabled();
      fireEvent.change(tid("reassign-select"), { target: { value: "beta" } });
      expect(tid("submit")).toBeEnabled();
      // Going back to the empty option disables it again.
      fireEvent.change(tid("reassign-select"), { target: { value: "" } });
      expect(tid("submit")).toBeDisabled();
    });

    it("enables Delete as soon as decommission is chosen", () => {
      setup(BUSY);
      fireEvent.click(tid("decommission-radio"));
      expect(tid("submit")).toBeEnabled();
      expect(tid("reassign-select")).toBeDisabled();
    });

    it("ignores a stale reassign target after switching to decommission", async () => {
      const { del } = setup(BUSY);
      fireEvent.click(tid("reassign-radio"));
      fireEvent.change(tid("reassign-select"), { target: { value: "beta" } });
      fireEvent.click(tid("decommission-radio"));
      await confirm();
      expect(del).toHaveBeenCalledWith("alpha", { decommission_agents: true });
    });

    it("disables reassign and says why when there is nothing to reassign to", () => {
      setup(BUSY, undefined, [BUSY]);
      expect(tid("reassign-radio")).toBeDisabled();
      expect(tid("guard")).toHaveTextContent(
        "There is no other project or group to reassign to.",
      );
      fireEvent.click(tid("decommission-radio"));
      expect(tid("submit")).toBeEnabled();
    });
  });

  describe("exact request bodies", () => {
    it("plain delete sends an empty body", async () => {
      const { del, onDeleted, onClose } = setup(IDLE);
      await confirm();
      expect(del).toHaveBeenCalledTimes(1);
      expect(del).toHaveBeenCalledWith("alpha", {});
      expect(onDeleted).toHaveBeenCalledWith("alpha", OK);
      expect(onClose).toHaveBeenCalledTimes(1);
    });

    it("reassign", async () => {
      const { del } = setup(BUSY);
      fireEvent.click(tid("reassign-radio"));
      fireEvent.change(tid("reassign-select"), { target: { value: "council" } });
      await confirm();
      expect(del).toHaveBeenCalledWith("alpha", { reassign_to: "council" });
    });

    it("decommission", async () => {
      const { del } = setup(BUSY);
      fireEvent.click(tid("decommission-radio"));
      await confirm();
      expect(del).toHaveBeenCalledWith("alpha", { decommission_agents: true });
    });

    it("store only", async () => {
      const { del } = setup(IDLE);
      fireEvent.click(tid("delete-store"));
      await confirm();
      expect(del).toHaveBeenCalledWith("alpha", { delete_store: true });
    });

    it("directory only", async () => {
      const { del } = setup(IDLE);
      fireEvent.click(tid("delete-dir"));
      await confirm();
      expect(del).toHaveBeenCalledWith("alpha", { delete_dir: true });
    });

    it("store and directory", async () => {
      const { del } = setup(IDLE);
      fireEvent.click(tid("delete-store"));
      fireEvent.click(tid("delete-dir"));
      await confirm();
      expect(del).toHaveBeenCalledWith("alpha", {
        delete_store: true,
        delete_dir: true,
      });
    });

    it("guard choice combined with both checkboxes", async () => {
      const { del } = setup(BUSY);
      fireEvent.click(tid("reassign-radio"));
      fireEvent.change(tid("reassign-select"), { target: { value: "beta" } });
      fireEvent.click(tid("delete-store"));
      fireEvent.click(tid("delete-dir"));
      await confirm();
      expect(del).toHaveBeenCalledWith("alpha", {
        reassign_to: "beta",
        delete_store: true,
        delete_dir: true,
      });
    });

    it("does not send a box that was checked and then unchecked", async () => {
      const { del } = setup(IDLE);
      fireEvent.click(tid("delete-store"));
      fireEvent.click(tid("delete-store"));
      fireEvent.click(tid("delete-dir"));
      fireEvent.click(tid("delete-dir"));
      await confirm();
      expect(del).toHaveBeenCalledWith("alpha", {});
    });

    it("a virtual group never sends file options and has no checkboxes", async () => {
      const { del } = setup({ ...GROUP, active_agent_count: 1 });
      expect(maybe("delete-store")).toBeNull();
      expect(maybe("delete-dir")).toBeNull();
      fireEvent.click(tid("decommission-radio"));
      await confirm();
      expect(del).toHaveBeenCalledWith("council", { decommission_agents: true });
    });
  });

  describe("file deletion options", () => {
    it("both checkboxes default to unchecked", () => {
      setup(IDLE);
      expect(tid("delete-store")).not.toBeChecked();
      expect(tid("delete-dir")).not.toBeChecked();
    });

    it("labels the store with store_path, or <slug>.fls when it is null", () => {
      const { unmount } = setup(BUSY);
      expect(
        screen.getByText("/work/alpha/.ferro/alpha.fls"),
      ).toBeInTheDocument();
      unmount();
      setup(IDLE);
      expect(screen.getByText("alpha.fls")).toBeInTheDocument();
    });

    it("labels the directory checkbox with the path", () => {
      setup(IDLE);
      expect(tid("delete-dir").closest("label")).toHaveTextContent(
        "Also delete project directory on disk (/work/alpha)",
      );
      expect(tid("delete-store").closest("label")).toHaveTextContent(
        "Also delete local FerroStore database",
      );
    });

    it("shows the warning only while delete_dir is checked and renames the confirm", () => {
      setup(IDLE);
      expect(maybe("dir-warning")).toBeNull();
      expect(tid("submit")).toHaveTextContent(/^Delete$/);
      expect(tid("submit")).not.toHaveClass("fd-btn-danger");
      fireEvent.click(tid("delete-store"));
      expect(maybe("dir-warning")).toBeNull();
      fireEvent.click(tid("delete-dir"));
      expect(tid("dir-warning")).toHaveTextContent(
        "Permanent. This deletes files on disk.",
      );
      expect(tid("delete-dir")).toHaveAccessibleDescription(
        "Permanent. This deletes files on disk.",
      );
      expect(tid("submit")).toHaveTextContent("Delete project and directory");
      expect(tid("submit")).toHaveClass("fd-btn-danger");
      fireEvent.click(tid("delete-dir"));
      expect(maybe("dir-warning")).toBeNull();
      expect(tid("submit")).toHaveTextContent(/^Delete$/);
    });
  });

  describe("errors", () => {
    it("a 409 names the agents and the count, reveals the guard and keeps state", async () => {
      const { del, onDeleted, onClose } = setup(IDLE, async () => {
        throw new ProjectDeleteConflictError("active agents", {
          active_agent_count: 2,
          active_agents: ["ada", "grace"],
        });
      });
      fireEvent.click(tid("delete-store"));
      expect(maybe("guard")).toBeNull();
      await confirm();
      const alert = tid("error");
      expect(alert).toHaveAttribute("role", "alert");
      expect(alert).toHaveTextContent("2 active agents");
      expect(alert).toHaveTextContent("ada, grace");
      // The stale count of 0 is overridden: the guard now shows and gates Delete.
      expect(tid("guard")).toBeInTheDocument();
      expect(tid("submit")).toBeDisabled();
      expect(tid("delete-store")).toBeChecked();
      expect(tid("reassign-radio")).toHaveFocus();
      expect(onDeleted).not.toHaveBeenCalled();
      expect(onClose).not.toHaveBeenCalled();
      // Choosing now retries with the choice.
      fireEvent.click(tid("decommission-radio"));
      del.mockResolvedValueOnce(OK);
      await confirm();
      expect(del).toHaveBeenLastCalledWith("alpha", {
        decommission_agents: true,
        delete_store: true,
      });
      expect(onDeleted).toHaveBeenCalledTimes(1);
    });

    it("a 409 with one agent uses the singular", async () => {
      setup(IDLE, async () => {
        throw new ProjectDeleteConflictError("active agents", {
          active_agent_count: 1,
          active_agents: ["ada"],
        });
      });
      await confirm();
      expect(tid("error")).toHaveTextContent("1 active agent (ada)");
    });

    it("any other error shows the server text and keeps the dialog and choices", async () => {
      const { onDeleted, onClose } = setup(BUSY, async () => {
        throw new Error("disk is read-only");
      });
      fireEvent.click(tid("reassign-radio"));
      fireEvent.change(tid("reassign-select"), { target: { value: "beta" } });
      fireEvent.click(tid("delete-dir"));
      await confirm();
      expect(tid("error")).toHaveAttribute("role", "alert");
      expect(tid("error")).toHaveTextContent("disk is read-only");
      expect(tid("modal")).toBeInTheDocument();
      expect(tid("reassign-radio")).toBeChecked();
      expect(tid("reassign-select")).toHaveValue("beta");
      expect(tid("delete-dir")).toBeChecked();
      expect(tid("submit")).toBeEnabled();
      expect(tid("submit")).toHaveFocus();
      expect(onDeleted).not.toHaveBeenCalled();
      expect(onClose).not.toHaveBeenCalled();
    });

    it("clears the previous error on a retry", async () => {
      const { del } = setup(IDLE, async () => {
        throw new Error("first failure");
      });
      await confirm();
      expect(tid("error")).toBeInTheDocument();
      del.mockResolvedValueOnce(OK);
      await confirm();
      expect(maybe("error")).toBeNull();
    });
  });

  describe("busy state", () => {
    it("blocks a double submit and locks every way out while deleting", async () => {
      let resolve!: (r: DeleteProjectResponse) => void;
      const { del, onClose, onDeleted } = setup(
        IDLE,
        () => new Promise<DeleteProjectResponse>((r) => (resolve = r)),
      );
      const form = tid("submit").closest("form")!;
      await act(async () => {
        fireEvent.submit(form);
        fireEvent.submit(form);
      });
      expect(del).toHaveBeenCalledTimes(1);
      expect(tid("submit")).toBeDisabled();
      expect(tid("submit")).toHaveTextContent("Deleting...");
      expect(tid("cancel")).toBeDisabled();
      expect(tid("delete-store")).toBeDisabled();
      fireEvent.keyDown(window, { key: "Escape" });
      fireEvent.click(tid("modal-backdrop"));
      expect(onClose).not.toHaveBeenCalled();
      await act(async () => resolve(OK));
      expect(onDeleted).toHaveBeenCalledTimes(1);
      expect(onClose).toHaveBeenCalledTimes(1);
    });

    it("never sends while the guard is unsatisfied, even on a form submit", async () => {
      const { del } = setup(BUSY);
      await act(async () => {
        fireEvent.submit(tid("submit").closest("form")!);
      });
      expect(del).not.toHaveBeenCalled();
    });
  });

  describe("closing and focus", () => {
    it("cancel, Escape and backdrop close without sending anything", () => {
      const { del, onClose } = setup(BUSY);
      fireEvent.click(tid("cancel"));
      expect(onClose).toHaveBeenCalledTimes(1);
      fireEvent.keyDown(window, { key: "Escape" });
      expect(onClose).toHaveBeenCalledTimes(2);
      fireEvent.click(tid("modal-backdrop"));
      expect(onClose).toHaveBeenCalledTimes(3);
      fireEvent.click(tid("modal"));
      expect(onClose).toHaveBeenCalledTimes(3);
      expect(del).not.toHaveBeenCalled();
    });

    it("puts initial focus on the first control, not the destructive confirm", () => {
      const { unmount } = setup(BUSY);
      expect(tid("reassign-radio")).toHaveFocus();
      unmount();
      const second = setup(BUSY, undefined, [BUSY]);
      expect(tid("decommission-radio")).toHaveFocus();
      second.unmount();
      const third = setup(IDLE);
      expect(tid("delete-store")).toHaveFocus();
      third.unmount();
      setup(GROUP);
      expect(tid("cancel")).toHaveFocus();
    });

    it("traps Tab and skips unchecked radios", () => {
      setup(BUSY);
      const dialog = tid("modal");
      fireEvent.click(tid("decommission-radio"));
      // Last control wraps to the first; the unchecked reassign radio is skipped.
      tid("submit").focus();
      fireEvent.keyDown(dialog, { key: "Tab" });
      expect(tid("decommission-radio")).toHaveFocus();
      fireEvent.keyDown(dialog, { key: "Tab", shiftKey: true });
      expect(tid("submit")).toHaveFocus();
    });

    function Host() {
      const [open, setOpen] = useState(false);
      const openerRef = useRef<HTMLElement | null>(null);
      return (
        <>
          <button
            data-testid="opener"
            onClick={(e) => {
              openerRef.current = e.currentTarget;
              setOpen(true);
            }}
          >
            open
          </button>
          <DeleteProjectModal
            isOpen={open}
            project={IDLE}
            otherProjects={[]}
            apiClient={{ deleteProject: async () => OK }}
            returnFocusTo={openerRef}
            onClose={() => setOpen(false)}
            onDeleted={vi.fn()}
          />
        </>
      );
    }

    it.each([
      ["Escape", () => fireEvent.keyDown(window, { key: "Escape" })],
      ["Cancel", () => fireEvent.click(tid("cancel"))],
      ["backdrop", () => fireEvent.click(tid("modal-backdrop"))],
    ] as const)("returns focus to the opener on %s", (_name, close) => {
      render(<Host />);
      const opener = screen.getByTestId("opener");
      expect(opener).not.toHaveFocus();
      fireEvent.click(opener);
      expect(tid("delete-store")).toHaveFocus();
      close();
      expect(maybe("modal")).toBeNull();
      expect(opener).toHaveFocus();
    });
  });
});

describe("describeProjectDeletion", () => {
  it("summarizes only what happened", () => {
    expect(describeProjectDeletion("Alpha", OK)).toBe("Deleted Alpha.");
    expect(
      describeProjectDeletion("Alpha", {
        deleted: "alpha",
        reassigned_agents: 2,
        decommissioned_agents: 1,
        store_deleted: true,
        dir_deleted: true,
      }),
    ).toBe(
      "Deleted Alpha. Reassigned 2 agents. Decommissioned 1 agent. Deleted the local FerroStore database. Deleted the project directory.",
    );
  });

  it("reports a partial cleanup failure with the server text", () => {
    expect(
      describeProjectDeletion("Alpha", {
        ...OK,
        store_deleted: true,
        error: "could not remove /p/x: permission denied",
      }),
    ).toBe(
      "Unregistered Alpha, but the cleanup failed: could not remove /p/x: permission denied Store deleted: yes. Directory deleted: no.",
    );
  });
});
