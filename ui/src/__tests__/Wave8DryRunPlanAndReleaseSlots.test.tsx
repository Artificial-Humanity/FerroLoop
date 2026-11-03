// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  AgentSettingsModal,
  UNIVERSAL_PERMISSION_CAPABILITIES as ReExportedCapabilities,
  compileUniversalPermissions as ReExportedCompile,
  resolvePermissionChangeClass as ReExportedChangeClass,
  resolveVendorPermissionPreview as ReExportedVendorPreview,
} from "../components/AgentSettingsModal";
import {
  DEFAULT_RELEASE_SLOTS,
  SettingsModal,
  computeDryRunInstallPlan,
} from "../components/SettingsModal";
import {
  DEFAULT_RULE_PRESET_MATRICES,
  SpinUpAgentModal,
  UNIVERSAL_PERMISSION_CAPABILITIES,
  compileUniversalPermissions,
  resolvePermissionChangeClass,
  resolveVendorPermissionPreview,
} from "../components/SpinUpAgentModal";
import {
  DEFAULT_PENDING_PERMISSION_PROMPTS,
  TriageQueuePane,
} from "../components/TriageQueuePane";
import { DEFAULT_PENDING_PERMISSION_PROMPTS as ReExportedTurnPrompts } from "../components/TurnCard";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("Wave 8 Item 1 (Issue #205 Phase A2): Deterministic fl install --dry-run Plan Preview", () => {
  it("computes ready vs needs_decision dry-run plans and renders install-dry-run-plan-card in SettingsModal", () => {
    const readyPlan = computeDryRunInstallPlan(
      ["ferrodeck", "ferrowire"],
      ["ferrodeck.service"],
      "managed",
    );
    expect(readyPlan.status).toBe("ready");
    expect(readyPlan.conflicts).toHaveLength(0);

    const conflictPlan = computeDryRunInstallPlan(
      ["ferrodeck"],
      [],
      "unmanaged_adopt_required",
    );
    expect(conflictPlan.status).toBe("needs_decision");
    expect(conflictPlan.conflicts).toHaveLength(1);

    render(
      <SettingsModal
        isOpen={true}
        onClose={() => {}}
        initialTab="products"
      />,
    );

    expect(screen.getByTestId("install-dry-run-plan-card")).toBeTruthy();
    expect(
      screen.getByTestId("dry-run-plan-status-badge").textContent,
    ).toBe("status: ready");
    expect(screen.getByTestId("dry-run-planned-files").textContent).toContain(
      "releases/0.9.4+b0888c6bc1ed/bin/fl",
    );
    expect(
      screen.getByTestId("dry-run-planned-symlinks").textContent,
    ).toContain("~/.local/share/fl/current -> releases/0.9.4+b0888c6bc1ed");

    fireEvent.click(screen.getByTestId("receipt-simulate-unmanaged-btn"));
    expect(
      screen.getByTestId("dry-run-plan-status-badge").textContent,
    ).toBe("status: needs_decision");
    expect(screen.getByTestId("dry-run-conflicts").textContent).toContain(
      "--adopt required",
    );

    fireEvent.click(screen.getByTestId("dry-run-refresh-btn"));
    expect(screen.getByTestId("dry-run-plan-banner").textContent).toContain(
      "Computed deterministic dry-run install plan",
    );
  });
});

describe("Wave 8 Item 2 (Issue #205 Phase C): Versioned releases/<ver>+<sha12>/ Upgrade & Rollback Slot Manager", () => {
  it("renders current, previous, and staged release slots and executes dry-run upgrade and atomic rollback", () => {
    expect(DEFAULT_RELEASE_SLOTS).toHaveLength(3);

    render(
      <SettingsModal
        isOpen={true}
        onClose={() => {}}
        initialTab="products"
      />,
    );

    expect(screen.getByTestId("release-slots-rollback-card")).toBeTruthy();
    expect(
      screen.getByTestId("release-current-symlink-badge").textContent,
    ).toContain("current -> releases/0.9.4+b0888c6bc1ed");

    for (const slot of DEFAULT_RELEASE_SLOTS) {
      expect(
        screen.getByTestId(`release-slot-row-${slot.slot_id}`),
      ).toBeTruthy();
      expect(
        screen.getByTestId(`release-slot-role-${slot.slot_id}`).textContent,
      ).toBe(slot.role);
    }

    fireEvent.click(screen.getByTestId("release-dry-run-upgrade-btn"));
    expect(
      screen.getByTestId("release-slot-action-banner").textContent,
    ).toContain("Dry-run upgrade verified");

    fireEvent.click(screen.getByTestId("release-rollback-btn"));
    expect(
      screen.getByTestId("release-current-symlink-badge").textContent,
    ).toContain("current -> releases/0.9.3+7a41d09ef312");
    expect(
      screen.getByTestId("release-slot-role-0.9.3+7a41d09ef312").textContent,
    ).toBe("current");
  });
});

describe("Wave 8 & Wave 9 (Record #103): Universal Agent Permission 3-Layer Model, Compile Receipt & Channel-Honest Remote Resolution", () => {
  it("renders separated Posture, Sandbox, and Rule Preset controls, Compile Receipt, and keyed Change Classes in SpinUpAgentModal and AgentSettingsModal", () => {
    expect(ReExportedCapabilities).toEqual(UNIVERSAL_PERMISSION_CAPABILITIES);
    expect(ReExportedVendorPreview).toBe(resolveVendorPermissionPreview);
    expect(ReExportedCompile).toBe(compileUniversalPermissions);
    expect(ReExportedChangeClass).toBe(resolvePermissionChangeClass);
    expect(UNIVERSAL_PERMISSION_CAPABILITIES).toEqual([
      "fs_read_workspace",
      "fs_read_external",
      "fs_write_workspace",
      "fs_write_external",
      "fs_write_protected",
      "shell_exec",
      "network_web",
      "mcp_tool_call",
    ]);

    // 1. Verify 5-vendor compile receipts & separation of posture vs sandbox
    const codexBypassKeepSandbox = compileUniversalPermissions(
      "codex",
      "bypass",
      "vendor-default",
      DEFAULT_RULE_PRESET_MATRICES.workspace,
    );
    expect(codexBypassKeepSandbox.nativeFlagsOrConfig).toContain(
      "--ask-for-approval never --sandbox workspace-write",
    );
    expect(codexBypassKeepSandbox.refused[0]).toContain(
      "posture=bypass skips rule evaluation",
    );

    const agyReceipt = compileUniversalPermissions(
      "antigravity",
      "ask",
      "vendor-default",
      DEFAULT_RULE_PRESET_MATRICES["workspace-web"],
    );
    expect(agyReceipt.downgrades.join(" ")).toContain(
      "exact-string command(...)",
    );
    expect(agyReceipt.downgrades.join(" ")).toContain(
      "allowNonWorkspaceAccess",
    );

    const museReceipt = compileUniversalPermissions(
      "muse",
      "ask",
      "vendor-default",
      DEFAULT_RULE_PRESET_MATRICES["read-only"],
    );
    expect(museReceipt.downgrades.join(" ")).toContain(
      "Muse proxy-only sandbox network mode",
    );

    // 2. Verify keyed change classes: hot | session | pending-next-start | respawn
    expect(resolvePermissionChangeClass("claude", "pipe-rpc", "sandbox")).toBe(
      "respawn",
    );
    expect(
      resolvePermissionChangeClass(
        "claude",
        "pipe-rpc",
        "posture",
        "accept-edits",
      ),
    ).toBe("session");
    expect(
      resolvePermissionChangeClass(
        "claude",
        "pipe-rpc",
        "posture",
        "bypass",
        false,
      ),
    ).toBe("respawn");
    expect(
      resolvePermissionChangeClass("claude", "pipe-rpc", "shell_exec"),
    ).toBe("hot");
    expect(
      resolvePermissionChangeClass("codex", "pipe-rpc", "shell_exec"),
    ).toBe("hot");
    expect(
      resolvePermissionChangeClass("codex", "pipe-rpc", "fs_write_external"),
    ).toBe("pending-next-start");
    expect(
      resolvePermissionChangeClass("antigravity", "pipe-rpc", "posture"),
    ).toBe("respawn");

    render(<SpinUpAgentModal isOpen={true} onClose={() => {}} />);

    expect(screen.getByTestId("spin-up-permissions-section")).toBeTruthy();
    expect(
      (screen.getByTestId("spin-up-posture-select") as HTMLSelectElement).value,
    ).toBe("ask");
    expect(
      (screen.getByTestId("spin-up-sandbox-select") as HTMLSelectElement).value,
    ).toBe("vendor-default");
    expect(
      (screen.getByTestId("spin-up-rule-preset-select") as HTMLSelectElement)
        .value,
    ).toBe("workspace");
    expect(screen.getByTestId("spin-up-compile-receipt")).toBeTruthy();
    expect(
      screen.getByTestId("spin-up-compile-emulation").textContent,
    ).toContain("Edit(.claude/**)");

    for (const cap of UNIVERSAL_PERMISSION_CAPABILITIES) {
      expect(screen.getByTestId(`spin-up-perm-cap-${cap}`)).toBeTruthy();
      expect(screen.getByTestId(`spin-up-perm-select-${cap}`)).toBeTruthy();
    }

    fireEvent.change(screen.getByTestId("spin-up-permission-mode-select"), {
      target: { value: "workspace-write" },
    });
    expect(
      screen.getByTestId("spin-up-permission-vendor-preview").textContent,
    ).toContain("--permission-mode acceptEdits");

    cleanup();

    render(
      <AgentSettingsModal
        isOpen={true}
        onClose={() => {}}
        agentName="felix"
        agentDisplayName="Felix"
        agentRole="ui-developer"
        agentProject="FerroLoop"
        adapter="codex"
        sessionId="sess-felix"
        selectedModel="gpt-5-codex"
        selectedThinkingLevel="high"
        onChangeModelAndThinking={() => {}}
        contextEstimate={{
          usedTokens: 12000,
          maxTokens: 200000,
          percent: 6,
        }}
        executionMode="auto"
        canCompact={false}
        compacting={false}
        onCompactContext={() => {}}
        canClear={true}
        clearing={false}
        onOpenClearModal={() => {}}
        canDecommission={true}
        decommissioning={false}
        onOpenDecommissionModal={() => {}}
      />,
    );

    expect(screen.getByTestId("agent-permissions-section")).toBeTruthy();
    expect(
      screen.getByTestId("agent-change-class-posture").textContent,
    ).toBe("session");
    expect(
      screen.getByTestId("agent-change-class-sandbox").textContent,
    ).toBe("respawn");
    expect(
      screen.getByTestId("agent-perm-change-class-shell_exec").textContent,
    ).toBe("hot");
    expect(
      screen.getByTestId("agent-perm-change-class-fs_write_external")
        .textContent,
    ).toBe("pending-next-start");
    expect(screen.getByTestId("agent-compile-receipt")).toBeTruthy();
  });

  it("renders channel-honest remote resolution in TriageQueuePane (RPC verified buttons vs PTY snapshot Notification Only + Send keys — unverified)", () => {
    expect(ReExportedTurnPrompts).toEqual(DEFAULT_PENDING_PERMISSION_PROMPTS);
    expect(DEFAULT_PENDING_PERMISSION_PROMPTS).toHaveLength(3);

    render(<TriageQueuePane />);

    expect(screen.getByTestId("triage-permission-prompts-section")).toBeTruthy();

    // 1. Real RPC channel with Always Allow supported (Claude SDK MCP)
    expect(screen.getByTestId("perm-prompt-card-perm-req-01")).toBeTruthy();
    expect(
      (
        screen.getByTestId(
          "perm-prompt-always-allow-btn-perm-req-01",
        ) as HTMLButtonElement
      ).disabled,
    ).toBe(false);
    fireEvent.click(
      screen.getByTestId("perm-prompt-approve-session-btn-perm-req-01"),
    );
    expect(
      screen.getByTestId("perm-prompt-status-perm-req-01").textContent,
    ).toBe("approve_session");

    // 2. Real RPC channel where Always Allow is NOT supported (Codex item/fileChange/requestApproval)
    expect(
      (
        screen.getByTestId(
          "perm-prompt-always-allow-btn-perm-req-02",
        ) as HTMLButtonElement
      ).disabled,
    ).toBe(true);
    fireEvent.click(screen.getByTestId("perm-prompt-deny-btn-perm-req-02"));
    expect(
      screen.getByTestId("perm-prompt-status-perm-req-02").textContent,
    ).toBe("deny");

    // 3. PTY snapshot channel (pty-vt100): Notification Only + Send keys — unverified (no verified RPC buttons)
    expect(
      screen.getByTestId("perm-prompt-notification-only-badge-perm-req-03"),
    ).toBeTruthy();
    expect(
      screen.queryByTestId("perm-prompt-approve-once-btn-perm-req-03"),
    ).toBeNull();
    fireEvent.click(
      screen.getByTestId("perm-prompt-send-keys-unverified-btn-perm-req-03"),
    );
    expect(
      screen.getByTestId("perm-prompt-status-perm-req-03").textContent,
    ).toBe("send_keys_unverified");
    expect(
      screen.getByTestId("triage-permission-prompt-banner").textContent,
    ).toContain("no verified permission grant recorded");
  });
});
