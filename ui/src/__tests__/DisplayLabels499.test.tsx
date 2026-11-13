import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
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
  it("retains established runtime labels when the catalog is empty or incomplete", () => {
    const expected = new Map([
      ["assistant", "Software Engineer"], ["director", "Project Director"], ["lead", "Project Lead"],
      ["lab_manager", "Lab Manager"], ["ui-developer", "UI & Full-Stack Application Developer"],
      ["web-developer", "Web Developer"], ["mobile-developer", "Mobile Developer"],
      ["system-developer", "System / Backend Developer"], ["data-engineer", "Data Engineer"],
      ["tester", "QA & Tester"], ["strategist", "Research & Architecture Strategist"],
      ["journalist", "Journalist & Technical Scribe"],
    ]);
    for (const [value, label] of expected) expect(roleLabel(value)).toBe(label);
    const lead = { slug: "lead", label: "Custom Lead Label", builtin: true, description: "", review_policy: "lead-review" as const, prompt_template: "" };
    const options = roleOptions([...expected.keys()].map(value => ({ value, label: value })), [lead]);
    expect(options.find(o => o.value === "lead")?.label).toBe("Custom Lead Label");
    expect(options.find(o => o.value === "tester")?.label).toBe("QA & Tester");
    expect(options.find(o => o.value === "system-developer")?.label).toBe("System / Backend Developer");
    expect(options.map(o => o.label)).toEqual(options.map(o => o.label).sort((a, b) => a.localeCompare(b)));
    const completeCatalog = ["director", "lead", "assistant", "strategist", "ui-developer", "lab-manager", "qa-tester", "security-engineer", "devops-release", "data-ml-engineer", "technical-writer", "journalist"].map(slug => ({ ...lead, slug, label: `Catalog ${slug}` }));
    const completeOptions = roleOptions([...expected.keys()].map(value => ({ value, label: value })), completeCatalog);
    expect(completeOptions.find(o => o.value === "strategist")?.label).toBe("Catalog strategist");
    expect(completeOptions.find(o => o.value === "tester")?.label).toBe("QA & Tester");
    expect(completeOptions.find(o => o.value === "system-developer")?.label).toBe("System / Backend Developer");
  });
  it("keeps reserved custom roles distinct from built-in labels", () => {
    const builtins = [
      { slug: "assistant", label: "Software Engineer", builtin: true, description: "", review_policy: "lead-review" as const, prompt_template: "" },
      { slug: "lead", label: "Project Lead", builtin: true, description: "", review_policy: "lab-manager-review" as const, prompt_template: "" },
      { slug: "qa-tester", label: "QA & Verification Engineer", builtin: true, description: "", review_policy: "lead-review" as const, prompt_template: "" },
      { slug: "council-chair", label: "Chair of Council", builtin: false, description: "", review_policy: "peer-review" as const, prompt_template: "" },
    ];
    expect(roleLabel("custom:lead", builtins)).toBe("Lead");
    expect(roleLabel("custom:assistant", builtins)).toBe("Assistant");
    expect(roleLabel("custom:council-chair", builtins)).toBe("Chair of Council");
    expect(roleLabel("custom:qa-tester", builtins)).toBe("QA & Verification Engineer");
  });
  it("dismisses a focused category tooltip with Escape without closing settings", () => {
    const onClose = vi.fn();
    render(<SettingsModal isOpen onClose={onClose} />);
    const agents = screen.getByTestId("settings-tab-agents");
    const tooltip = screen.getByTestId("settings-tooltip-agents");
    expect(tooltip).toHaveAttribute("hidden");
    act(() => agents.focus());
    expect(tooltip).not.toHaveAttribute("hidden");
    fireEvent.keyDown(agents, { key: "Escape" });
    expect(tooltip).toHaveAttribute("hidden");
    expect(onClose).not.toHaveBeenCalled();
    expect(agents).toHaveFocus();
  });
  it("dismisses a hovered tooltip even when focus is outside its icon", () => {
    const onClose = vi.fn();
    render(<SettingsModal isOpen onClose={onClose} />);
    const agents = screen.getByTestId("settings-tab-agents");
    const tooltip = screen.getByTestId("settings-tooltip-agents");
    fireEvent.mouseEnter(agents);
    expect(tooltip).not.toHaveAttribute("hidden");
    fireEvent.keyDown(window, { key: "Escape" });
    expect(tooltip).toHaveAttribute("hidden");
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.mouseLeave(agents);
    fireEvent.mouseEnter(agents);
    expect(tooltip).not.toHaveAttribute("hidden");
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
