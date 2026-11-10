import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { roleLabel, roleOptions, selectedRoleOptions } from "../hooks/useRoles";
import { SPIN_UP_VENDORS, spinUpProjectOptions } from "../components/SpinUpAgentModal";
import { THINKING_LEVEL_OPTIONS } from "../components/AgentSettingsModal";
import { SettingsModal } from "../components/SettingsModal";

afterEach(() => cleanup());

describe("operator labels and settings navigation", () => {
  it("formats fallback roles and sorts catalog labels while retaining values", () => {
    expect(roleLabel("custom:council-chair")).toBe("Council Chair");
    expect(roleLabel("web-developer")).toBe("Web Developer");
    const priorDefault = { slug: "assistant", label: "Assisting Engineer", description: "", builtin: true, review_policy: "lead-review" as const, prompt_template: "" };
    expect(roleLabel("assistant", [priorDefault])).toBe("Software Engineer");
    expect(roleLabel("assistant", [{ ...priorDefault, label: "My Engineer" }])).toBe("My Engineer");
    const options = roleOptions([{ value: "web-developer", label: "web-developer" }, { value: "assistant", label: "assistant" }], [
      { slug: "assistant", label: "Software Engineer", description: "", builtin: true, review_policy: "lead-review", prompt_template: "" },
    ]);
    expect(options).toEqual([{ value: "assistant", label: "Software Engineer" }, { value: "web-developer", label: "Web Developer" }]);
    expect(selectedRoleOptions(options, "custom:alpha-role", []).map(o => o.label)).toEqual(["Alpha Role", "Software Engineer", "Web Developer"]);
  });
  it("shows human labels for projects, vendors and thinking levels", () => {
    expect(spinUpProjectOptions([{ slug: "ferroloop", name: "FerroLoop" } as never])[0]).toEqual({ value: "ferroloop", label: "FerroLoop" });
    expect(SPIN_UP_VENDORS.find((v) => v.value === "codex")?.label).toBe("OpenAI Codex");
    expect(THINKING_LEVEL_OPTIONS.find((v) => v.value === "medium")?.label).toBe("Balanced");
  });
  it("offers keyboard-accessible section buttons", () => {
    render(<SettingsModal isOpen onClose={vi.fn()} />);
    const agents = screen.getByTestId("settings-tab-agents");
    const projects = screen.getByTestId("settings-tab-projects");
    expect(agents).toHaveAccessibleName("Agents");
    expect(agents).toHaveAttribute("aria-selected", "true");
    agents.focus();
    fireEvent.keyDown(agents, { key: "ArrowRight" });
    expect(projects).toHaveFocus();
    expect(projects).toHaveAttribute("aria-selected", "true");
  });
  it("shows the runtime category only to owners and hides fleet categories in project settings", () => {
    const { rerender } = render(<SettingsModal isOpen onClose={vi.fn()} authority={{ tier: "owner", login: "owner" }} />);
    expect(screen.getByTestId("settings-tab-runtimes")).toHaveAccessibleName("Runtimes & Quotas");
    rerender(<SettingsModal isOpen onClose={vi.fn()} />);
    expect(screen.queryByTestId("settings-tab-runtimes")).not.toBeInTheDocument();
    rerender(<SettingsModal isOpen onClose={vi.fn()} initialProject="FerroLoop" />);
    expect(screen.queryByTestId("settings-tab-projects")).not.toBeInTheDocument();
  });

});
