import React, { useEffect, useRef, useState } from "react";
import { HubApiClient } from "../api/client";
import { roleLabel, roleOptions, useRoles } from "../hooks/useRoles";
import {
  formatModelOptionLabel,
  getVendorModelOptions,
} from "./ChatPane";
import { projectLabel } from "../hooks/useProjects";
import {
  normalizeHydrationStatus,
  normalizeHydrationWarnings,
  type EnrolledNode,
  type PermissionPosture,
  type ProjectSummary,
  type SpinUpSessionRequest,
  type SpinUpSessionResponse,
} from "../types/api";

export interface SpinUpProjectOption {
  value: string;
  label: string;
}

/** Spin-Up dropdown options for a project list: the slug is what is sent. */
export function spinUpProjectOptions(
  projects: readonly ProjectSummary[],
): SpinUpProjectOption[] {
  return projects.map((p) => {
    const value = p.slug.trim().toLowerCase();
    return { value, label: `${value} (${projectLabel(p)})` };
  });
}

export const SPIN_UP_ROLES = [
  { value: "assistant", label: "assistant (Assisting Engineer)" },
  { value: "ui-developer", label: "ui-developer (UI & Full-Stack)" },
  { value: "web-developer", label: "web-developer (Web Developer)" },
  { value: "mobile-developer", label: "mobile-developer (Mobile Developer)" },
  { value: "system-developer", label: "system-developer (System / Backend Developer)" },
  { value: "data-engineer", label: "data-engineer (Data Engineer)" },
  { value: "tester", label: "tester (QA & Tester)" },
  { value: "strategist", label: "strategist (Research & Architecture Strategist)" },
  { value: "journalist", label: "journalist (Journalist / Technical Scribe)" },
  { value: "lead", label: "lead (Project Lead)" },
  { value: "lab-manager", label: "lab-manager (Lab Manager)" },
] as const;

export const SPIN_UP_VENDORS = [
  { value: "claude", label: "claude (Claude Code CLI)" },
  { value: "antigravity", label: "antigravity (Google Antigravity)" },
  { value: "codex", label: "codex (OpenAI Codex)" },
  { value: "grok", label: "grok (xAI Grok)" },
  { value: "muse", label: "muse (Muse Code)" },
] as const;

export interface SpinUpAgentModalProps {
  isOpen: boolean;
  onClose: () => void;
  initialProject?: string | null;
  /** The shared project list for the Project dropdown. Defaults to the built-in list. */
  projects?: readonly ProjectSummary[];
  nodes?: EnrolledNode[];
  defaultNodeId?: string | null;
  onSpinUp?: (req: SpinUpSessionRequest) => Promise<SpinUpSessionResponse>;
  onSpunUp?: (res: SpinUpSessionResponse) => void;
  apiClient?: HubApiClient;
}

export function resolveSpinUpNodeId(
  defaultNodeId?: string | null,
  nodes: readonly EnrolledNode[] = [],
): string {
  const explicit = (defaultNodeId ?? "").trim();
  if (explicit) {
    return explicit;
  }
  const hubNode = nodes.find((n) => n.role === "hub") ?? nodes[0];
  return (hubNode?.node_id || hubNode?.display_name || "").trim();
}

export function resolveSpinUpProject(
  rawProject?: string | null,
  projects: readonly ProjectSummary[] = [],
): string {
  const options = spinUpProjectOptions(projects);
  if (options.length === 0) {
    return "";
  }
  const trimmed = (rawProject ?? "").trim().toLowerCase();
  if (!trimmed || trimmed === "all") {
    return options[0]?.value ?? "";
  }
  const matched =
    options.find((p) => p.value === trimmed) ??
    projects
      .map((p) => ({
        value: p.slug.trim().toLowerCase(),
        name: p.name.trim().toLowerCase(),
        label: projectLabel(p).trim().toLowerCase(),
      }))
      .find(
        (p) =>
          p.value === trimmed ||
          p.name === trimmed ||
          p.label === trimmed ||
          p.label.includes(trimmed),
      );
  return matched ? matched.value : "";
}

// Record #87: Worktree Policy, Bootstrap (.fl/worktree.toml) & Conflict-Free Port Allocation
export type WorktreePolicyMode = "create" | "reuse" | "shared";

export interface WorktreeBootstrapPreview {
  policy: WorktreePolicyMode;
  worktreePath: string;
  bootstrapConfigPath: string;
  bootstrapHooks: string[];
  allocatedPort: number;
  allocatedVitePort: number;
}

export function computeWorktreeBootstrapPreview(
  persona: string,
  project: string,
  policy: WorktreePolicyMode = "create",
): WorktreeBootstrapPreview {
  const cleanPersona = (persona || "agent").trim().toLowerCase() || "agent";
  const cleanProject = (project || "ferroloop").trim().toLowerCase() || "ferroloop";
  const key = `${cleanProject}:${cleanPersona}`;
  let hash = 0;
  for (let i = 0; i < key.length; i += 1) {
    hash = (hash * 31 + key.charCodeAt(i)) % 800;
  }
  const allocatedPort = 4100 + Math.abs(hash);
  const allocatedVitePort = allocatedPort + 1000;

  const worktreePath =
    policy === "create"
      ? `~/Projects/Artificial-Humanity/.worktrees/${cleanProject}/${cleanPersona}`
      : policy === "reuse"
        ? `~/Projects/Artificial-Humanity/.worktrees/${cleanProject}/reused-${cleanPersona}`
        : `~/Projects/Artificial-Humanity/${cleanProject}`;

  const bootstrapHooks =
    policy === "shared"
      ? ["fl state sync --shared-checkout"]
      : [
          "git submodule update --init",
          "fl state sync --worktree",
          `export PORT=${allocatedPort} VITE_PORT=${allocatedVitePort}`,
        ];

  return {
    policy,
    worktreePath,
    bootstrapConfigPath: ".fl/worktree.toml",
    bootstrapHooks,
    allocatedPort,
    allocatedVitePort,
  };
}

// Record #103: Universal Agent Permission & Auto-Accept Management
// Converged 5-Vendor 3-Layer Model: Posture + Sandbox + Rules
export type UniversalPermissionPosture = PermissionPosture;

export type UniversalSandboxMode = "vendor-default" | "off";

export type UniversalRulePreset =
  | "read-only"
  | "workspace"
  | "workspace-web"
  | "custom";

export type PermissionChangeClass =
  | "hot"
  | "session"
  | "pending-next-start"
  | "respawn";

export type UniversalPermissionMode =
  | "auto-accept"
  | "workspace-write"
  | "granular";

export type PermissionRuleAction = "allow" | "ask" | "deny";

export type ProtectedPathClass = "settings" | "git" | "tool-config";

export type UnenforcedDefaultsMode = "refuse" | "acknowledge";

export type ReceiptClass =
  | "exact"
  | "stricter_downgrade"
  | "tool_scoped_emulation"
  | "unsupported"
  | "native"
  | "downgraded"
  | "emulated"
  | "refused";

export type EvidenceClass =
  | "documented"
  | "schema_defined"
  | "runtime_proven"
  | "unsupported"
  | "live-verified"
  | "docs-verified"
  | "source-verified";

export type CanonicalRulePattern =
  | { kind: "path_glob"; glob: string }
  | { kind: "abs_path_glob"; glob: string }
  | { kind: "path_class"; class: ProtectedPathClass }
  | { kind: "argv_prefix"; argv: string[] }
  | { kind: "shell_glob"; glob: string }
  | { kind: "domain"; domain: string }
  | { kind: "mcp"; server: string; tool?: string };

export interface CanonicalPermissionRule {
  capability: UniversalPermissionCapability;
  effect: PermissionRuleAction;
  pattern?: CanonicalRulePattern;
}

/**
 * Canonical `PermissionPolicy` v1 (`crates/core/src/permissions/policy.rs`, PR #271).
 * Uses `deny_unknown_fields`: contains no extra UI-only keys.
 */
export interface CanonicalPermissionPolicy {
  schema_version: 1;
  posture: UniversalPermissionPosture;
  sandbox: UniversalSandboxMode;
  require_fs_enforcement?: boolean;
  unenforced_defaults?: UnenforcedDefaultsMode;
  rules: CanonicalPermissionRule[];
}

export interface CanonicalUnenforcedItem {
  capability: UniversalPermissionCapability;
  vendor?: string;
  kind?: PermissionRuleAction;
  effect?: PermissionRuleAction;
  reason?: string;
}

export interface CanonicalReceiptLine {
  subject?: string;
  requested?: string;
  native?: string[];
  class: ReceiptClass;
  evidence: EvidenceClass;
  note?: string;
  summary?: string;
}

export type UniversalPermissionCapability =
  | "fs_read_workspace"
  | "fs_read_external"
  | "fs_write_workspace"
  | "fs_write_external"
  | "fs_write_protected"
  | "shell_exec"
  | "network_web"
  | "mcp_tool_call";

export const UNIVERSAL_PERMISSION_CAPABILITIES: readonly UniversalPermissionCapability[] =
  [
    "fs_read_workspace",
    "fs_read_external",
    "fs_write_workspace",
    "fs_write_external",
    "fs_write_protected",
    "shell_exec",
    "network_web",
    "mcp_tool_call",
  ] as const;

export function canonicalDefaultEffect(
  capability: UniversalPermissionCapability,
): PermissionRuleAction {
  return capability === "fs_write_protected" ? "deny" : "ask";
}

const EFFECT_RANK: Record<PermissionRuleAction, number> = {
  deny: 0,
  ask: 1,
  allow: 2,
};

export interface BuildCanonicalPolicyInput {
  posture: UniversalPermissionPosture;
  sandbox: UniversalSandboxMode;
  matrix?: UniversalPermissionMatrix;
  requireFsEnforcement?: boolean;
  unenforcedDefaults?: UnenforcedDefaultsMode;
  patternRules?: CanonicalPermissionRule[];
}

export function buildCanonicalPermissionPolicy(
  postureOrInput: UniversalPermissionPosture | BuildCanonicalPolicyInput,
  sandboxArg: UniversalSandboxMode = "vendor-default",
  matrixArg: UniversalPermissionMatrix = DEFAULT_RULE_PRESET_MATRICES.workspace,
  options?: {
    requireFsEnforcement?: boolean;
    unenforcedDefaults?: UnenforcedDefaultsMode;
    patternRules?: CanonicalPermissionRule[];
  },
): CanonicalPermissionPolicy {
  const resolved: BuildCanonicalPolicyInput =
    typeof postureOrInput === "object" && postureOrInput !== null
      ? postureOrInput
      : {
          posture: postureOrInput,
          sandbox: sandboxArg,
          matrix: matrixArg,
          requireFsEnforcement: options?.requireFsEnforcement,
          unenforcedDefaults: options?.unenforcedDefaults,
          patternRules: options?.patternRules,
        };
  const {
    posture,
    sandbox,
    matrix = DEFAULT_RULE_PRESET_MATRICES.workspace,
    requireFsEnforcement = false,
    unenforcedDefaults = "refuse",
    patternRules = [],
  } = resolved;

  const isWriteCap = (cap: UniversalPermissionCapability): boolean =>
    cap === "fs_write_workspace" ||
    cap === "fs_write_external" ||
    cap === "fs_write_protected";

  const candidateRules: CanonicalPermissionRule[] = [];
  for (const cap of UNIVERSAL_PERMISSION_CAPABILITIES) {
    const effect = matrix[cap];
    if (effect === canonicalDefaultEffect(cap)) {
      continue;
    }
    if (posture === "bypass" && effect !== "allow") {
      continue;
    }
    if (posture === "auto" && effect === "ask") {
      continue;
    }
    if (posture === "accept-edits" && effect === "ask" && isWriteCap(cap)) {
      continue;
    }
    candidateRules.push({ capability: cap, effect });
  }

  for (const r of patternRules) {
    if (posture === "bypass" && r.effect !== "allow") {
      continue;
    }
    if (posture === "auto" && r.effect === "ask") {
      continue;
    }
    if (
      posture === "accept-edits" &&
      r.effect === "ask" &&
      isWriteCap(r.capability)
    ) {
      continue;
    }
    candidateRules.push(
      r.pattern
        ? { capability: r.capability, effect: r.effect, pattern: r.pattern }
        : { capability: r.capability, effect: r.effect },
    );
  }

  candidateRules.sort((a, b) => {
    const capDiff =
      UNIVERSAL_PERMISSION_CAPABILITIES.indexOf(a.capability) -
      UNIVERSAL_PERMISSION_CAPABILITIES.indexOf(b.capability);
    if (capDiff !== 0) return capDiff;
    return EFFECT_RANK[a.effect] - EFFECT_RANK[b.effect];
  });

  const effectiveRequireFs =
    posture === "bypass" || sandbox === "off" ? false : requireFsEnforcement;

  return {
    schema_version: 1,
    posture,
    sandbox,
    require_fs_enforcement: effectiveRequireFs,
    unenforced_defaults: unenforcedDefaults,
    rules: candidateRules,
  };
}

export type UniversalPermissionMatrix = Record<
  UniversalPermissionCapability,
  PermissionRuleAction
>;

export const DEFAULT_RULE_PRESET_MATRICES: Record<
  UniversalRulePreset,
  UniversalPermissionMatrix
> = {
  "read-only": {
    fs_read_workspace: "allow",
    fs_read_external: "ask",
    fs_write_workspace: "deny",
    fs_write_external: "deny",
    fs_write_protected: "deny",
    shell_exec: "deny",
    network_web: "deny",
    mcp_tool_call: "ask",
  },
  workspace: {
    fs_read_workspace: "allow",
    fs_read_external: "ask",
    fs_write_workspace: "allow",
    fs_write_external: "ask",
    fs_write_protected: "deny",
    shell_exec: "ask",
    network_web: "ask",
    mcp_tool_call: "allow",
  },
  "workspace-web": {
    fs_read_workspace: "allow",
    fs_read_external: "allow",
    fs_write_workspace: "allow",
    fs_write_external: "ask",
    fs_write_protected: "deny",
    shell_exec: "ask",
    network_web: "allow",
    mcp_tool_call: "allow",
  },
  custom: {
    fs_read_workspace: "allow",
    fs_read_external: "ask",
    fs_write_workspace: "allow",
    fs_write_external: "ask",
    fs_write_protected: "deny",
    shell_exec: "ask",
    network_web: "ask",
    mcp_tool_call: "allow",
  },
};

export const DEFAULT_UNIVERSAL_PERMISSION_MATRICES: Record<
  UniversalPermissionMode,
  UniversalPermissionMatrix
> = {
  "auto-accept": {
    fs_read_workspace: "allow",
    fs_read_external: "allow",
    fs_write_workspace: "allow",
    fs_write_external: "allow",
    fs_write_protected: "ask",
    shell_exec: "allow",
    network_web: "allow",
    mcp_tool_call: "allow",
  },
  "workspace-write": DEFAULT_RULE_PRESET_MATRICES.workspace,
  granular: DEFAULT_RULE_PRESET_MATRICES["workspace-web"],
};

export interface PermissionCompileReceipt {
  vendor: string;
  posture: UniversalPermissionPosture;
  sandbox: UniversalSandboxMode;
  policy: CanonicalPermissionPolicy;
  requiresRespawn: boolean;
  unenforced: CanonicalUnenforcedItem[];
  receiptLines: CanonicalReceiptLine[];
  nativeFlagsOrConfig: string;
  downgrades: string[];
  emulationNotices: string[];
  refused: string[];
  respawnOnLaterChange: string[];
}

export function compileUniversalPermissions(
  vendor?: string | null,
  posture: UniversalPermissionPosture = "ask",
  sandbox: UniversalSandboxMode = "vendor-default",
  matrix: UniversalPermissionMatrix = DEFAULT_RULE_PRESET_MATRICES.workspace,
  options?: {
    requireFsEnforcement?: boolean;
    unenforcedDefaults?: UnenforcedDefaultsMode;
    patternRules?: CanonicalPermissionRule[];
  },
): PermissionCompileReceipt {
  const v = (vendor ?? "claude").trim().toLowerCase();
  const policy = buildCanonicalPermissionPolicy(posture, sandbox, matrix, options);
  const downgrades: string[] = [];
  const emulationNotices: string[] = [];
  const refused: string[] = [];
  const unenforced: CanonicalUnenforcedItem[] = [];
  const receiptLines: CanonicalReceiptLine[] = [];

  const hasExplicitDeny = UNIVERSAL_PERMISSION_CAPABILITIES.some(
    (cap) => matrix[cap] === "deny",
  );
  if (posture === "bypass" && hasExplicitDeny) {
    refused.push(
      "posture=bypass skips rule evaluation and cannot enforce explicit deny rules.",
    );
  }
  if (options?.requireFsEnforcement && sandbox === "off") {
    refused.push(
      "require_fs_enforcement=true requires sandbox=vendor-default (cannot enforce without OS sandbox boundary).",
    );
  }

  if (v.includes("claude")) {
    const postureFlag =
      posture === "ask"
        ? "--permission-mode default"
        : posture === "accept-edits"
          ? "--permission-mode acceptEdits"
          : posture === "auto"
            ? "--permission-mode auto"
            : posture === "plan"
              ? "--permission-mode plan"
              : "--permission-mode bypassPermissions --allow-dangerously-skip-permissions";
    const sandboxFlag =
      sandbox === "off" ? "sandbox.enabled=false" : "sandbox.enabled=true";
    receiptLines.push(
      {
        class: "native",
        summary: `Claude CLI posture flag: ${postureFlag}`,
        evidence: "live-verified",
      },
      {
        class: "native",
        summary: `.claude/settings.local.json (${sandboxFlag})`,
        evidence: "docs-verified",
      },
    );
    if (matrix.fs_write_protected === "deny") {
      const msg =
        "fs_write_protected compiled as tool-scoped deny rules: Edit(.claude/**), Edit(.git/**), Write(.claude/**), Write(.git/**); Bash writes remain governed by shell_exec.";
      emulationNotices.push(msg);
      receiptLines.push({
        class: "emulated",
        summary: msg,
        evidence: "docs-verified",
      });
    }
    return {
      vendor: "claude",
      posture,
      sandbox,
      policy,
      requiresRespawn: false,
      unenforced,
      receiptLines,
      nativeFlagsOrConfig: `${postureFlag} · .claude/settings.local.json (${sandboxFlag})`,
      downgrades,
      emulationNotices,
      refused,
      respawnOnLaterChange: [
        "sandbox",
        ...(posture !== "bypass"
          ? ["posture -> bypass (when not offered at launch)"]
          : []),
      ],
    };
  }

  if (v.includes("codex")) {
    const postureFlag =
      posture === "bypass"
        ? "--ask-for-approval never"
        : posture === "accept-edits" || posture === "auto"
          ? "--ask-for-approval on-request"
          : "--ask-for-approval untrusted";
    const sandboxFlag =
      sandbox === "off"
        ? "--sandbox danger-full-access"
        : "--sandbox workspace-write";
    const combinedFlags =
      posture === "bypass" && sandbox === "off"
        ? "--dangerously-bypass-approvals-and-sandbox"
        : `${postureFlag} ${sandboxFlag}`;
    receiptLines.push({
      class: "native",
      summary: `Codex launch flags: ${combinedFlags}`,
      evidence: "live-verified",
    });
    if (
      matrix.fs_read_external === "deny" ||
      matrix.fs_write_external === "deny"
    ) {
      const msg =
        "codex has no path-glob deny file; external path isolation relies on OS Seatbelt/Landlock sandbox.";
      downgrades.push(msg);
      receiptLines.push({
        class: "downgraded",
        summary: msg,
        evidence: "source-verified",
      });
    }
    return {
      vendor: "codex",
      posture,
      sandbox,
      policy,
      requiresRespawn: false,
      unenforced,
      receiptLines,
      nativeFlagsOrConfig: `${combinedFlags} · ~/.codex/rules/default.rules`,
      downgrades,
      emulationNotices,
      refused,
      respawnOnLaterChange: ["sandbox", "config.toml non-shell rules"],
    };
  }

  if (v.includes("antigravity") || v.includes("agy")) {
    if (matrix.network_web === "deny" || matrix.mcp_tool_call === "deny") {
      const msg =
        "antigravity has no network_web or mcp_tool_call deny primitive; unsupported deny rule refused.";
      refused.push(msg);
    }
    const downgradeShell =
      "shell_exec patterns downgrade to exact-string command(...) entries in ~/.gemini/antigravity/settings.json (no glob/regex).";
    downgrades.push(downgradeShell);
    receiptLines.push({
      class: "downgraded",
      summary: downgradeShell,
      evidence: "docs-verified",
    });
    if (matrix.fs_read_external !== matrix.fs_write_external) {
      const downgradeFs =
        "fs_read_external and fs_write_external share a single allowNonWorkspaceAccess boolean in Antigravity.";
      downgrades.push(downgradeFs);
      receiptLines.push({
        class: "downgraded",
        summary: downgradeFs,
        evidence: "docs-verified",
      });
    }
    const postureFlag =
      posture === "bypass"
        ? "--yolo"
        : posture === "auto"
          ? "terminalAutoExecutionPolicy=auto"
          : posture === "plan"
            ? "planningMode=plan"
            : "terminalAutoExecutionPolicy=off";
    return {
      vendor: "antigravity",
      posture,
      sandbox,
      policy,
      requiresRespawn: false,
      unenforced,
      receiptLines,
      nativeFlagsOrConfig: `${postureFlag} · ~/.gemini/antigravity/settings.json`,
      downgrades,
      emulationNotices,
      refused,
      respawnOnLaterChange: [
        "posture",
        "sandbox",
        "settings.json rules (pending-next-start)",
      ],
    };
  }

  if (v.includes("grok")) {
    if (
      matrix.fs_write_external === "deny" ||
      matrix.network_web === "deny"
    ) {
      refused.push(
        "grok has no path-scoped or network deny primitive; unsupported deny rule refused.",
      );
    }
    const msg =
      "grok supports shell prefix strings in ~/.grok/user-settings.json only; no OS sandbox.";
    downgrades.push(msg);
    receiptLines.push({
      class: "downgraded",
      summary: msg,
      evidence: "source-verified",
    });
    unenforced.push({
      capability: "fs_write_protected",
      effect: matrix.fs_write_protected,
      reason: "Grok CLI has no OS sandbox or path-scoped write deny primitive.",
    });
    const postureFlag =
      posture === "bypass"
        ? "full-auto"
        : posture === "accept-edits" || posture === "auto"
          ? "auto-edit"
          : posture === "plan"
            ? "plan"
            : "default";
    return {
      vendor: "grok",
      posture,
      sandbox,
      policy,
      requiresRespawn: false,
      unenforced,
      receiptLines,
      nativeFlagsOrConfig: `mode=${postureFlag} · ~/.grok/user-settings.json`,
      downgrades,
      emulationNotices,
      refused,
      respawnOnLaterChange: [
        "sandbox",
        "user-settings.json rules (pending-next-start)",
      ],
    };
  }

  // Muse
  if (matrix.network_web === "deny") {
    const msg =
      "network_web=deny downgrades to Muse proxy-only sandbox network mode (HTTPS_PROXY=http://localhost:8877).";
    downgrades.push(msg);
    receiptLines.push({
      class: "downgraded",
      summary: msg,
      evidence: "source-verified",
    });
  }
  const musePosture =
    posture === "bypass"
      ? "yolo"
      : posture === "accept-edits" || posture === "auto"
        ? "auto_edit"
        : posture === "plan"
          ? "plan"
          : "default";
  const museSandbox = sandbox === "off" ? "--no-sandbox" : "--sandbox";
  receiptLines.push({
    class: "native",
    summary: `Muse profile=${musePosture} ${museSandbox}`,
    evidence: "source-verified",
  });
  return {
    vendor: "muse",
    posture,
    sandbox,
    policy,
    requiresRespawn: false,
    unenforced,
    receiptLines,
    nativeFlagsOrConfig: `profile=${musePosture} ${museSandbox} · ~/.muse/settings.json`,
    downgrades,
    emulationNotices,
    refused,
    respawnOnLaterChange: [
      "posture",
      "sandbox",
      "settings.json rules (pending-next-start)",
    ],
  };
}

export function resolvePermissionChangeClass(
  vendor?: string | null,
  transport?: string | null,
  target: "posture" | "sandbox" | UniversalPermissionCapability = "posture",
  nextPosture?: UniversalPermissionPosture,
  bypassOfferedAtLaunch = false,
): PermissionChangeClass {
  if (target === "sandbox") {
    return "respawn";
  }
  const v = (vendor ?? "claude").trim().toLowerCase();
  const t = (transport ?? "").trim().toLowerCase();

  if (target === "posture") {
    if (v.includes("claude")) {
      if (nextPosture === "bypass" && !bypassOfferedAtLaunch) {
        return "respawn";
      }
      return "session";
    }
    if (v.includes("grok")) {
      return "session";
    }
    if (v.includes("codex")) {
      return t.includes("app-server") || t.includes("rpc") || t.includes("pipe")
        ? "session"
        : "respawn";
    }
    return "respawn";
  }

  // Capability rule change class
  if (v.includes("claude")) {
    return "hot";
  }
  if (v.includes("codex")) {
    return target === "shell_exec" ? "hot" : "pending-next-start";
  }
  if (v.includes("muse") && t.includes("msp")) {
    return "session";
  }
  return "pending-next-start";
}

export function resolveVendorPermissionPreview(
  vendor?: string | null,
  mode: UniversalPermissionMode = "auto-accept",
): string {
  const v = (vendor ?? "claude").trim().toLowerCase();
  if (mode === "auto-accept") {
    if (v.includes("claude")) return "--dangerously-skip-permissions";
    if (v.includes("codex")) {
      return "--dangerously-bypass-approvals-and-sandbox";
    }
    if (v.includes("antigravity") || v.includes("agy")) {
      return "--yolo (auto-accept)";
    }
    return "--auto-approve";
  }
  if (mode === "workspace-write") {
    if (v.includes("claude")) {
      return "--permission-mode acceptEdits (.claude/settings.local.json)";
    }
    if (v.includes("codex")) {
      return "--ask-for-approval on-request --sandbox workspace-write";
    }
    return "workspace-write (prompt on external/shell)";
  }
  if (v.includes("claude")) {
    return ".claude/settings.local.json (8-capability granular rules)";
  }
  if (v.includes("codex")) {
    return "~/.codex/config.toml (8-capability granular rules)";
  }
  return ".fl/permissions.toml (8-capability granular matrix)";
}

export const SpinUpAgentModal: React.FC<SpinUpAgentModalProps> = ({
  isOpen,
  onClose,
  initialProject = null,
  projects = [],
  nodes = [],
  defaultNodeId = null,
  onSpinUp,
  onSpunUp,
  apiClient,
}) => {
  const catalog = useRoles(isOpen, apiClient);
  const roles = roleOptions(SPIN_UP_ROLES, catalog.roles);
  const [persona, setPersona] = useState("");
  const [project, setProject] = useState<string>(() =>
    resolveSpinUpProject(initialProject, projects),
  );
  const [projectTouched, setProjectTouched] = useState(false);
  // Latest list for the reset effect, which must not rerun when the list refreshes.
  const projectsRef = useRef(projects);
  projectsRef.current = projects;
  const defaultNodeRef = useRef({ defaultNodeId, nodes });
  defaultNodeRef.current = { defaultNodeId, nodes };
  const [role, setRole] = useState<string>("assistant");
  const [vendor, setVendor] = useState<string>("claude");
  const [model, setModel] = useState("");
  const [nodeId, setNodeId] = useState<string>(() =>
    resolveSpinUpNodeId(defaultNodeId, nodes),
  );
  const [nodeIdTouched, setNodeIdTouched] = useState(false);
  const [hosted, setHosted] = useState(true);
  const [worktreePolicy, setWorktreePolicy] =
    useState<WorktreePolicyMode>("create");
  const [worktreePolicyTouched, setWorktreePolicyTouched] = useState(false);
  const [localProviderPreset, setLocalProviderPreset] =
    useState<string>("vendor-native");
  const [posture, setPosture] = useState<UniversalPermissionPosture>("auto");
  const [sandbox, setSandbox] =
    useState<UniversalSandboxMode>("vendor-default");
  const [rulePreset, setRulePreset] =
    useState<UniversalRulePreset>("workspace");
  const [permissionMode, setPermissionMode] =
    useState<UniversalPermissionMode>("auto-accept");
  const [permissionMatrix, setPermissionMatrix] =
    useState<UniversalPermissionMatrix>(
      () => DEFAULT_RULE_PRESET_MATRICES.workspace,
    );
  const [requireFsEnforcement, setRequireFsEnforcement] = useState(false);
  const [unenforcedDefaults, setUnenforcedDefaults] =
    useState<UnenforcedDefaultsMode>("refuse");
  const [permissionsTouched, setPermissionsTouched] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastSpinUpResult, setLastSpinUpResult] =
    useState<SpinUpSessionResponse | null>(null);

  useEffect(() => {
    if (isOpen) {
      setProject(resolveSpinUpProject(initialProject, projectsRef.current));
      setProjectTouched(false);
      setNodeId(
        resolveSpinUpNodeId(
          defaultNodeRef.current.defaultNodeId,
          defaultNodeRef.current.nodes,
        ),
      );
      setNodeIdTouched(false);
      setPosture("auto");
      setWorktreePolicyTouched(false);
      setPermissionsTouched(false);
      setLastSpinUpResult(null);
    }
  }, [initialProject, isOpen]);

  const resolvedDefaultProject = resolveSpinUpProject(initialProject, projects);
  useEffect(() => {
    if (
      isOpen &&
      !projectTouched &&
      (!project.trim() ||
        !projects.some((p) => p.slug.trim().toLowerCase() === project)) &&
      resolvedDefaultProject !== project
    ) {
      setProject(resolvedDefaultProject);
    }
  }, [isOpen, projectTouched, project, projects, resolvedDefaultProject]);

  const resolvedDefaultNodeId = resolveSpinUpNodeId(defaultNodeId, nodes);
  useEffect(() => {
    if (isOpen && !nodeIdTouched && !nodeId.trim() && resolvedDefaultNodeId) {
      setNodeId(resolvedDefaultNodeId);
    }
  }, [isOpen, nodeIdTouched, nodeId, resolvedDefaultNodeId]);

  useEffect(() => {
    setError(null);
    if (!isOpen) return;
    const handleKeyDownEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !submitting) {
        onClose();
      }
    };
    window.addEventListener("keydown", handleKeyDownEscape);
    return () => window.removeEventListener("keydown", handleKeyDownEscape);
  }, [isOpen, submitting, onClose]);

  if (!isOpen) {
    return null;
  }

  const projectDropdownOptions = spinUpProjectOptions(projects);
  if (project && !projectDropdownOptions.some((o) => o.value === project)) {
    // The selected project (a deleted one) stays selectable.
    projectDropdownOptions.unshift({ value: project, label: project });
  }

  const nodeOptions = Array.from(
    new Set(
      [
        defaultNodeId?.trim(),
        ...nodes.map((n) => (n.node_id || n.display_name).trim()),
      ].filter((v): v is string => Boolean(v)),
    ),
  );

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    const trimmedPersona = persona.trim().toLowerCase();
    if (!trimmedPersona) {
      setError("Please enter a persona name (e.g. kacey).");
      return;
    }
    const trimmedProject = project.trim().toLowerCase();
    if (!trimmedProject) {
      setError("Please select a project.");
      return;
    }

    setError(null);
    setSubmitting(true);

    const worktreePreview = computeWorktreeBootstrapPreview(
      trimmedPersona,
      trimmedProject,
      worktreePolicy,
    );

    const trimmedNodeId = nodeId.trim();
    const payload: SpinUpSessionRequest = {
      persona: trimmedPersona,
      project: trimmedProject,
      role,
      vendor,
      ...(model.trim() ? { model: model.trim() } : {}),
      posture,
      ...(trimmedNodeId ? { node_id: trimmedNodeId } : {}),
      hosted,
      ...(worktreePolicyTouched
        ? {
            worktree_policy: worktreePolicy,
            allocated_port: worktreePreview.allocatedPort,
            allocated_vite_port: worktreePreview.allocatedVitePort,
          }
        : {}),
      ...(permissionsTouched
        ? {
            permission_policy: buildCanonicalPermissionPolicy(
              posture,
              sandbox,
              permissionMatrix,
              {
                requireFsEnforcement,
                unenforcedDefaults,
              },
            ),
          }
        : {}),
    };

    try {
      let res: SpinUpSessionResponse;
      if (onSpinUp) {
        res = await onSpinUp(payload);
      } else if (apiClient) {
        res = await apiClient.spinUpSession(payload);
      } else {
        const client = new HubApiClient();
        res = await client.spinUpSession(payload);
      }
      const normalizedRes: SpinUpSessionResponse = {
        ...res,
        hydration_status: normalizeHydrationStatus(res.hydration_status),
        warnings: normalizeHydrationWarnings(res.warnings),
      };
      setLastSpinUpResult(normalizedRes);
      onSpunUp?.(normalizedRes);
      setPersona("");
      setModel("");
      onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create new agent session");
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div
      className="fd-modal-backdrop"
      onClick={() => {
        if (!submitting) {
          onClose();
        }
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label="New Agent Session"
        data-testid="spin-up-agent-modal"
        className="fd-modal"
        onClick={(e) => e.stopPropagation()}
        style={{ maxWidth: "480px" }}
      >
        <div
          className="fd-modal-header"
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: "8px",
            marginBottom: "14px",
          }}
        >
          <h2 style={{ margin: 0, fontSize: "16px", fontWeight: 700, flex: 1, minWidth: 0 }}>
            New Agent Session
          </h2>
          <button
            type="button"
            className="fd-btn"
            style={{ flexShrink: 0 }}
            onClick={onClose}
            disabled={submitting}
            aria-label="Close New Agent Modal"
          >
            ✕
          </button>
        </div>

        <p style={{ margin: "0 0 14px 0", fontSize: "12px", color: "var(--text-secondary)" }}>
          Launch a zero-terminal agent runtime session, register its persona in the workspace
          roster, and dispatch its root startup hydration prompt automatically.
        </p>

        {error && (
          <div
            role="alert"
            style={{
              marginBottom: "12px",
              padding: "8px 10px",
              borderRadius: "var(--radius-md)",
              backgroundColor: "rgba(239, 68, 68, 0.14)",
              border: "1px solid rgba(239, 68, 68, 0.45)",
              color: "#FCA5A5",
              fontSize: "12px",
            }}
          >
            {error}
          </div>
        )}

        <form
          onSubmit={(e) => void handleSubmit(e)}
          style={{ display: "flex", flexDirection: "column", gap: "12px" }}
        >
          <div>
            <label
              htmlFor="fd-spin-up-persona"
              style={{
                display: "block",
                fontSize: "11px",
                fontWeight: 600,
                color: "var(--text-muted)",
                marginBottom: "4px",
                textTransform: "uppercase",
              }}
            >
              Persona Name
            </label>
            <input
              id="fd-spin-up-persona"
              type="text"
              name="fd-spin-up-persona"
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              data-1p-ignore="true"
              data-lpignore="true"
              data-form-type="other"
              data-testid="spin-up-persona-input"
              className="fd-input fd-mono"
              placeholder="e.g. kacey"
              value={persona}
              onChange={(e) => setPersona(e.target.value)}
              disabled={submitting}
              style={{ width: "100%" }}
            />
          </div>

          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: "10px" }}>
            <div>
              <label
                htmlFor="fd-spin-up-project"
                style={{
                  display: "block",
                  fontSize: "11px",
                  fontWeight: 600,
                  color: "var(--text-muted)",
                  marginBottom: "4px",
                  textTransform: "uppercase",
                }}
              >
                Project
              </label>
              <select
                id="fd-spin-up-project"
                data-testid="spin-up-project-select"
                className="fd-select fd-mono"
                value={project}
                onChange={(e) => {
                  setProject(e.target.value);
                  setProjectTouched(true);
                }}
                disabled={submitting}
                style={{ width: "100%" }}
              >
                {(!project || projectDropdownOptions.length === 0) && (
                  <option value="">Select a project…</option>
                )}
                {projectDropdownOptions.map((proj) => (
                  <option key={proj.value} value={proj.value}>
                    {proj.label}
                  </option>
                ))}
              </select>
            </div>

            <div>
              <label
                htmlFor="fd-spin-up-role"
                style={{
                  display: "block",
                  fontSize: "11px",
                  fontWeight: 600,
                  color: "var(--text-muted)",
                  marginBottom: "4px",
                  textTransform: "uppercase",
                }}
              >
                Role
              </label>
              <select
                id="fd-spin-up-role"
                data-testid="spin-up-role-select"
                className="fd-select fd-mono"
                value={role}
                onChange={(e) => setRole(e.target.value)}
                disabled={submitting}
                style={{ width: "100%" }}
              >
                {!roles.some((r) => r.value === role) && <option value={role}>{roleLabel(role, catalog.roles)}</option>}
                {roles.map((r) => (
                  <option key={r.value} value={r.value}>
                    {r.label}
                  </option>
                ))}
              </select>
              {catalog.loading && <div role="status">Loading roles…</div>}
              {catalog.error && <div role="status">Role catalog: {catalog.error}. Existing roles remain available.</div>}
            </div>
          </div>

          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: "10px" }}>
            <div>
              <label
                htmlFor="fd-spin-up-vendor"
                style={{
                  display: "block",
                  fontSize: "11px",
                  fontWeight: 600,
                  color: "var(--text-muted)",
                  marginBottom: "4px",
                  textTransform: "uppercase",
                }}
              >
                Vendor Runtime
              </label>
              <select
                id="fd-spin-up-vendor"
                data-testid="spin-up-vendor-select"
                className="fd-select fd-mono"
                value={vendor}
                onChange={(e) => {
                  const nextVendor = e.target.value;
                  setVendor(nextVendor);
                  const nextOpts = getVendorModelOptions(nextVendor);
                  if (model.trim() && nextOpts.length > 0 && !nextOpts.includes(model.trim())) {
                    setModel("");
                  }
                }}
                disabled={submitting}
                style={{ width: "100%" }}
              >
                {SPIN_UP_VENDORS.map((v) => (
                  <option key={v.value} value={v.value}>
                    {v.label}
                  </option>
                ))}
              </select>
            </div>

            <div>
              <label
                htmlFor="fd-spin-up-model"
                style={{
                  display: "block",
                  fontSize: "11px",
                  fontWeight: 600,
                  color: "var(--text-muted)",
                  marginBottom: "4px",
                  textTransform: "uppercase",
                }}
              >
                Model (Optional)
              </label>
              <input
                id="fd-spin-up-model"
                type="text"
                list="fd-spin-up-model-options"
                name="fd-spin-up-model"
                autoComplete="off"
                autoCorrect="off"
                autoCapitalize="off"
                spellCheck={false}
                data-1p-ignore="true"
                data-lpignore="true"
                data-form-type="other"
                data-testid="spin-up-model-input"
                className="fd-input fd-mono"
                placeholder={
                  getVendorModelOptions(vendor)[0]
                    ? `e.g. ${getVendorModelOptions(vendor)[0]}`
                    : "e.g. claude-opus-5-5"
                }
                value={model}
                onChange={(e) => setModel(e.target.value)}
                disabled={submitting}
                style={{ width: "100%" }}
              />
              <datalist id="fd-spin-up-model-options" data-testid="spin-up-model-options">
                {getVendorModelOptions(vendor).map((opt) => {
                  const label = formatModelOptionLabel(opt);
                  return (
                    <option key={opt} value={opt} label={label}>
                      {label}
                    </option>
                  );
                })}
              </datalist>
            </div>
          </div>

          <div>
            <label
              htmlFor="fd-spin-up-node"
              style={{
                display: "block",
                fontSize: "11px",
                fontWeight: 600,
                color: "var(--text-muted)",
                marginBottom: "4px",
                textTransform: "uppercase",
              }}
            >
              Target Node
            </label>
            <input
              id="fd-spin-up-node"
              type="text"
              list="fd-spin-up-node-options"
              name="fd-spin-up-node"
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              data-1p-ignore="true"
              data-lpignore="true"
              data-form-type="other"
              data-testid="spin-up-node-select"
              className="fd-input fd-mono"
              value={nodeId}
              onChange={(e) => {
                setNodeId(e.target.value);
                setNodeIdTouched(true);
              }}
              disabled={submitting}
              style={{ width: "100%" }}
            />
            <datalist id="fd-spin-up-node-options">
              {nodeOptions.map((n) => (
                <option key={n} value={n} />
              ))}
            </datalist>
          </div>

          {/* Record #72: Local Model & API-Key Endpoint Preset */}
          <div
            data-testid="spin-up-local-provider-section"
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "8px",
              padding: "6px 10px",
              borderRadius: "6px",
              backgroundColor: "var(--bg-elevated)",
              border: "1px solid var(--border-subtle)",
              fontSize: "11px",
              flexWrap: "wrap",
            }}
          >
            <label
              htmlFor="fd-spin-up-local-provider"
              style={{
                fontWeight: 600,
                color: "var(--text-muted)",
                textTransform: "uppercase",
              }}
            >
              Model Provider (#72)
            </label>
            <select
              id="fd-spin-up-local-provider"
              aria-label="Model Provider Endpoint"
              data-testid="spin-up-local-provider-select"
              className="fd-select fd-mono"
              value={localProviderPreset}
              disabled={submitting}
              onChange={(e) => setLocalProviderPreset(e.target.value)}
              style={{ fontSize: "10.5px" }}
            >
              <option value="vendor-native">
                vendor-native (Subscription CLI)
              </option>
              <option value="ollama-local">
                ollama-local (http://127.0.0.1:11434/v1)
              </option>
              <option value="lemonade-rocm">
                lemonade-rocm (http://127.0.0.1:8000/v1 · AMD ROCm)
              </option>
              <option value="openai-compat-vllm">
                openai-compat-vllm (vault://ferrogate/providers/local-vllm-key)
              </option>
            </select>
            <code
              className="fd-mono"
              data-testid="spin-up-local-provider-endpoint-preview"
              style={{ fontSize: "10px", color: "var(--accent-primary)" }}
            >
              {localProviderPreset === "ollama-local"
                ? "http://127.0.0.1:11434/v1 (keyless)"
                : localProviderPreset === "lemonade-rocm"
                  ? "http://127.0.0.1:8000/v1 (ROCm)"
                  : localProviderPreset === "openai-compat-vllm"
                    ? "http://127.0.0.1:8080/v1 (vault://ferrogate/providers/local-vllm-key)"
                    : "Subscription CLI default"}
            </code>
          </div>

          {/* Record #87: Worktree Policy, Bootstrap (.fl/worktree.toml) & Port Allocation */}
          {(() => {
            const preview = computeWorktreeBootstrapPreview(
              persona,
              project,
              worktreePolicy,
            );
            return (
              <div
                data-testid="spin-up-worktree-preview"
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
                  padding: "8px 10px",
                  borderRadius: "6px",
                  backgroundColor: "var(--bg-elevated)",
                  border: "1px solid var(--border-subtle)",
                  fontSize: "11.5px",
                }}
              >
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "8px",
                    flexWrap: "wrap",
                  }}
                >
                  <label
                    htmlFor="fd-spin-up-worktree-policy"
                    style={{
                      fontSize: "11px",
                      fontWeight: 600,
                      color: "var(--text-muted)",
                      textTransform: "uppercase",
                    }}
                  >
                    Worktree Policy
                  </label>
                  <span
                    className="fd-badge fd-mono"
                    data-testid="spin-up-allocated-ports-badge"
                    style={{
                      backgroundColor: "rgba(56, 189, 248, 0.14)",
                      borderColor: "rgba(56, 189, 248, 0.4)",
                      color: "var(--accent-primary)",
                      fontSize: "10.5px",
                    }}
                  >
                    PORT={preview.allocatedPort} · VITE_PORT={preview.allocatedVitePort}
                  </span>
                </div>
                <select
                  id="fd-spin-up-worktree-policy"
                  aria-label="Worktree Policy"
                  data-testid="spin-up-worktree-policy-select"
                  className="fd-select fd-mono"
                  value={worktreePolicy}
                  disabled={submitting}
                  onChange={(e) => {
                    setWorktreePolicyTouched(true);
                    setWorktreePolicy(e.target.value as WorktreePolicyMode);
                  }}
                  style={{ width: "100%" }}
                >
                  <option value="create">
                    create (Isolated branch worktree)
                  </option>
                  <option value="reuse">
                    reuse (Existing worktree checkout)
                  </option>
                  <option value="shared">
                    shared (Primary project checkout)
                  </option>
                </select>
                <div
                  className="fd-mono"
                  data-testid="spin-up-worktree-path"
                  style={{ fontSize: "10.5px", color: "var(--text-secondary)" }}
                >
                  Path: {preview.worktreePath}
                </div>
                <div
                  className="fd-mono"
                  data-testid="spin-up-bootstrap-hooks"
                  style={{ fontSize: "10.5px", color: "var(--text-muted)" }}
                >
                  {preview.bootstrapConfigPath}: {preview.bootstrapHooks.join(" && ")}
                </div>
              </div>
            );
          })()}

          {/* Record #103: Universal Agent Permission & Auto-Accept Management (3-Layer Model) */}
          {(() => {
            const compileReceipt = compileUniversalPermissions(
              vendor,
              posture,
              sandbox,
              permissionMatrix,
              {
                requireFsEnforcement,
                unenforcedDefaults,
              },
            );
            return (
              <div
                data-testid="spin-up-permissions-section"
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
                  padding: "8px 10px",
                  borderRadius: "6px",
                  backgroundColor: "var(--bg-elevated)",
                  border: "1px solid var(--border-subtle)",
                  fontSize: "11.5px",
                }}
              >
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "8px",
                    flexWrap: "wrap",
                  }}
                >
                  <label
                    htmlFor="fd-spin-up-permission-mode"
                    style={{
                      fontSize: "11px",
                      fontWeight: 600,
                      color: "var(--text-muted)",
                      textTransform: "uppercase",
                    }}
                  >
                    Permission Posture, Sandbox &amp; Rules
                  </label>
                  <code
                    className="fd-mono"
                    data-testid="spin-up-permission-vendor-preview"
                    style={{ fontSize: "10.5px", color: "var(--accent-primary)" }}
                  >
                    {resolveVendorPermissionPreview(vendor, permissionMode)}
                  </code>
                </div>

                {/* 3-Layer Controls: Posture + Sandbox + Rule Preset */}
                <div
                  style={{
                    display: "grid",
                    gridTemplateColumns: "repeat(3, minmax(0, 1fr))",
                    gap: "6px",
                  }}
                >
                  <div>
                    <label
                      htmlFor="fd-spin-up-posture"
                      style={{
                        display: "block",
                        fontSize: "10px",
                        fontWeight: 600,
                        color: "var(--text-muted)",
                        marginBottom: "2px",
                        textTransform: "uppercase",
                      }}
                    >
                      1. Posture
                    </label>
                    <select
                      id="fd-spin-up-posture"
                      aria-label="Approval Posture"
                      data-testid="spin-up-posture-select"
                      className="fd-select fd-mono"
                      value={posture}
                      disabled={submitting}
                      onChange={(e) => {
                        const nextPosture = e.target
                          .value as UniversalPermissionPosture;
                        setPermissionsTouched(true);
                        setPosture(nextPosture);
                        if (nextPosture === "bypass") {
                          setPermissionMode("auto-accept");
                          setRequireFsEnforcement(false);
                        } else if (nextPosture === "accept-edits") {
                          setPermissionMode("workspace-write");
                        }
                      }}
                      style={{ width: "100%", fontSize: "10.5px" }}
                    >
                      <option value="ask">ask</option>
                      <option value="accept-edits">accept-edits</option>
                      <option value="auto">auto (default)</option>
                      <option value="plan">plan</option>
                      <option value="bypass">bypass (skip prompts)</option>
                    </select>
                  </div>

                  <div>
                    <label
                      htmlFor="fd-spin-up-sandbox"
                      style={{
                        display: "block",
                        fontSize: "10px",
                        fontWeight: 600,
                        color: "var(--text-muted)",
                        marginBottom: "2px",
                        textTransform: "uppercase",
                      }}
                    >
                      2. Sandbox
                    </label>
                    <select
                      id="fd-spin-up-sandbox"
                      aria-label="OS Sandbox Mode"
                      data-testid="spin-up-sandbox-select"
                      className="fd-select fd-mono"
                      value={sandbox}
                      disabled={submitting}
                      onChange={(e) => {
                        const nextSandbox = e.target
                          .value as UniversalSandboxMode;
                        setPermissionsTouched(true);
                        setSandbox(nextSandbox);
                        if (nextSandbox === "off") {
                          setRequireFsEnforcement(false);
                        }
                      }}
                      style={{ width: "100%", fontSize: "10.5px" }}
                    >
                      <option value="vendor-default">vendor-default</option>
                      <option value="off">off (no OS sandbox)</option>
                    </select>
                  </div>

                  <div>
                    <label
                      htmlFor="fd-spin-up-rule-preset"
                      style={{
                        display: "block",
                        fontSize: "10px",
                        fontWeight: 600,
                        color: "var(--text-muted)",
                        marginBottom: "2px",
                        textTransform: "uppercase",
                      }}
                    >
                      3. Rule Preset
                    </label>
                    <select
                      id="fd-spin-up-rule-preset"
                      aria-label="Capability Rule Preset"
                      data-testid="spin-up-rule-preset-select"
                      className="fd-select fd-mono"
                      value={rulePreset}
                      disabled={submitting}
                      onChange={(e) => {
                        const nextPreset = e.target
                          .value as UniversalRulePreset;
                        setPermissionsTouched(true);
                        setRulePreset(nextPreset);
                        setPermissionMatrix({
                          ...DEFAULT_RULE_PRESET_MATRICES[nextPreset],
                        });
                      }}
                      style={{ width: "100%", fontSize: "10.5px" }}
                    >
                      <option value="read-only">read-only</option>
                      <option value="workspace">workspace</option>
                      <option value="workspace-web">workspace-web</option>
                      <option value="custom">custom</option>
                    </select>
                  </div>
                </div>

                {/* Canonical PermissionPolicy v1 enforcement & unenforced_defaults controls */}
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "8px",
                    flexWrap: "wrap",
                  }}
                >
                  <label
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: "6px",
                      fontSize: "10.5px",
                      color: "var(--text-secondary)",
                      cursor: "pointer",
                    }}
                  >
                    <input
                      type="checkbox"
                      data-testid="spin-up-require-fs-enforcement-checkbox"
                      checked={requireFsEnforcement}
                      disabled={
                        submitting || sandbox === "off" || posture === "bypass"
                      }
                      onChange={(e) => {
                        setPermissionsTouched(true);
                        setRequireFsEnforcement(e.target.checked);
                      }}
                    />
                    <span>
                      require_fs_enforcement (OS boundary for fs_write_protected)
                    </span>
                  </label>
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: "4px",
                    }}
                  >
                    <label
                      htmlFor="fd-spin-up-unenforced-defaults"
                      style={{
                        fontSize: "10px",
                        color: "var(--text-muted)",
                        fontWeight: 600,
                      }}
                    >
                      unenforced_defaults:
                    </label>
                    <select
                      id="fd-spin-up-unenforced-defaults"
                      aria-label="Unenforced Defaults Policy"
                      data-testid="spin-up-unenforced-defaults-select"
                      className="fd-select fd-mono"
                      value={unenforcedDefaults}
                      disabled={submitting}
                      onChange={(e) => {
                        setPermissionsTouched(true);
                        setUnenforcedDefaults(
                          e.target.value as UnenforcedDefaultsMode,
                        );
                      }}
                      style={{ fontSize: "10px", padding: "1px 4px" }}
                    >
                      <option value="refuse">refuse (default)</option>
                      <option value="acknowledge">acknowledge</option>
                    </select>
                  </div>
                </div>

                <select
                  id="fd-spin-up-permission-mode"
                  aria-label="Permission & Approval Mode"
                  data-testid="spin-up-permission-mode-select"
                  className="fd-select fd-mono"
                  value={permissionMode}
                  disabled={submitting}
                  onChange={(e) => {
                    const nextMode = e.target.value as UniversalPermissionMode;
                    setPermissionsTouched(true);
                    setPermissionMode(nextMode);
                    setPermissionMatrix({
                      ...DEFAULT_UNIVERSAL_PERMISSION_MATRICES[nextMode],
                    });
                    if (nextMode === "auto-accept") {
                      setPosture("bypass");
                      setRequireFsEnforcement(false);
                    } else if (nextMode === "workspace-write") {
                      setPosture("accept-edits");
                      setRulePreset("workspace");
                    } else {
                      setRulePreset("custom");
                    }
                  }}
                  style={{ width: "100%", fontSize: "10.5px" }}
                >
                  <option value="auto-accept">
                    auto-accept (Full autonomous bypass)
                  </option>
                  <option value="workspace-write">
                    workspace-write (Auto-allow workspace, prompt external/shell)
                  </option>
                  <option value="granular">
                    granular (Per-capability allow / ask / deny matrix)
                  </option>
                </select>

                <div
                  data-testid="spin-up-permission-matrix"
                  style={{
                    display: "grid",
                    gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))",
                    gap: "4px",
                    marginTop: "2px",
                  }}
                >
                  {UNIVERSAL_PERMISSION_CAPABILITIES.map((cap) => (
                    <div
                      key={cap}
                      data-testid={`spin-up-perm-cap-${cap}`}
                      style={{
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                        gap: "4px",
                        padding: "3px 6px",
                        borderRadius: "4px",
                        border: "1px solid var(--border-subtle)",
                        fontSize: "10.5px",
                      }}
                    >
                      <code className="fd-mono">{cap}</code>
                      <select
                        aria-label={`Permission for ${cap}`}
                        data-testid={`spin-up-perm-select-${cap}`}
                        className="fd-select fd-mono"
                        style={{ fontSize: "10px", padding: "1px 4px" }}
                        value={permissionMatrix[cap]}
                        onChange={(e) => {
                          const nextAction = e.target
                            .value as PermissionRuleAction;
                          setPermissionsTouched(true);
                          setPermissionMode("granular");
                          setRulePreset("custom");
                          setPermissionMatrix((prev) => ({
                            ...prev,
                            [cap]: nextAction,
                          }));
                        }}
                      >
                        <option value="allow">allow</option>
                        <option value="ask">ask</option>
                        <option value="deny">deny</option>
                      </select>
                    </div>
                  ))}
                </div>

                {/* Compile Receipt & Downgrade Preview */}
                <div
                  data-testid="spin-up-compile-receipt"
                  style={{
                    padding: "6px 8px",
                    borderRadius: "4px",
                    backgroundColor: "var(--bg-surface)",
                    border: "1px solid var(--border-subtle)",
                    display: "flex",
                    flexDirection: "column",
                    gap: "4px",
                    fontSize: "10.5px",
                  }}
                >
                  <div
                    className="fd-mono"
                    data-testid="spin-up-compile-native-flags"
                    style={{ color: "var(--accent-primary)" }}
                  >
                    Compiled ({compileReceipt.vendor}):{" "}
                    {compileReceipt.nativeFlagsOrConfig}
                  </div>
                  <div
                    className="fd-mono"
                    data-testid="spin-up-canonical-policy-preview"
                    style={{ color: "var(--text-secondary)", fontSize: "10px" }}
                  >
                    Policy v{compileReceipt.policy.schema_version}: posture=
                    {compileReceipt.policy.posture} · sandbox=
                    {compileReceipt.policy.sandbox} · rules=
                    {compileReceipt.policy.rules.length} · unenforced_defaults=
                    {compileReceipt.policy.unenforced_defaults ?? "refuse"}
                  </div>
                  {compileReceipt.downgrades.length > 0 && (
                    <div
                      data-testid="spin-up-compile-downgrades"
                      style={{ color: "#FBBF24" }}
                    >
                      Downgrade: {compileReceipt.downgrades.join(" ")}
                    </div>
                  )}
                  {compileReceipt.emulationNotices.length > 0 && (
                    <div
                      data-testid="spin-up-compile-emulation"
                      style={{ color: "var(--text-secondary)" }}
                    >
                      Emulated: {compileReceipt.emulationNotices.join(" ")}
                    </div>
                  )}
                  {compileReceipt.refused.length > 0 && (
                    <div
                      data-testid="spin-up-compile-refused"
                      style={{ color: "#F87171", fontWeight: 600 }}
                    >
                      Refused: {compileReceipt.refused.join(" ")}
                    </div>
                  )}
                  <div
                    className="fd-mono"
                    data-testid="spin-up-compile-respawn-notice"
                    style={{ color: "var(--text-muted)", fontSize: "10px" }}
                  >
                    Requires Respawn if Changed Later:{" "}
                    {compileReceipt.respawnOnLaterChange.join(", ")}
                  </div>
                </div>
              </div>
            );
          })()}

          <label
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: "8px",
              fontSize: "12px",
              color: "var(--text-secondary)",
              cursor: "pointer",
            }}
          >
            <input
              type="checkbox"
              data-testid="spin-up-hosted-checkbox"
              checked={hosted}
              onChange={(e) => setHosted(e.target.checked)}
              disabled={submitting}
            />
            <span>Supervised (Subscription Hosting)</span>
          </label>

          {lastSpinUpResult && (() => {
            const hydrationStatus = normalizeHydrationStatus(
              lastSpinUpResult.hydration_status,
            );
            const warnings = normalizeHydrationWarnings(
              lastSpinUpResult.warnings,
            );
            const isFailed = hydrationStatus === "failed";
            const isConfirmed = hydrationStatus === "confirmed";
            return (
              <div
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
                }}
              >
                <div
                  role={isFailed ? "status" : undefined}
                  data-testid="spin-up-hydration-status"
                  data-hydration-status={hydrationStatus}
                  data-hydration-dispatched={String(
                    Boolean(lastSpinUpResult.hydration_dispatched),
                  )}
                  className="fd-mono"
                  style={{
                    padding: "6px 8px",
                    borderRadius: "4px",
                    fontSize: "11px",
                    border: isFailed
                      ? "1px solid rgba(245, 158, 11, 0.45)"
                      : isConfirmed
                        ? "1px solid rgba(16, 185, 129, 0.4)"
                        : "1px solid var(--border-subtle)",
                    backgroundColor: isFailed
                      ? "rgba(245, 158, 11, 0.14)"
                      : isConfirmed
                        ? "rgba(16, 185, 129, 0.12)"
                        : "var(--bg-elevated)",
                    color: isFailed
                      ? "#FBBF24"
                      : isConfirmed
                        ? "#34D399"
                        : "var(--text-secondary)",
                  }}
                >
                  Hydration: <strong>{hydrationStatus}</strong> (dispatched:{" "}
                  {lastSpinUpResult.hydration_dispatched ? "true" : "false"})
                </div>
                {warnings.length > 0 && (
                  <ul
                    role="status"
                    data-testid="spin-up-warnings"
                    className="fd-mono"
                    style={{
                      margin: 0,
                      padding: "6px 8px 6px 22px",
                      borderRadius: "4px",
                      fontSize: "11px",
                      border: "1px solid rgba(245, 158, 11, 0.45)",
                      backgroundColor: "rgba(245, 158, 11, 0.14)",
                      color: "#FBBF24",
                      display: "flex",
                      flexDirection: "column",
                      gap: "2px",
                    }}
                  >
                    {warnings.map((warning, idx) => (
                      <li key={idx} data-testid="spin-up-warning-item">
                        {warning}
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            );
          })()}

          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "flex-end",
              gap: "8px",
              marginTop: "4px",
            }}
          >
            <button
              type="button"
              className="fd-btn"
              data-testid="spin-up-cancel-btn"
              disabled={submitting}
              onClick={onClose}
            >
              Cancel
            </button>
            <button
              type="submit"
              className="fd-btn fd-btn-primary"
              data-testid="spin-up-submit-btn"
              disabled={submitting}
            >
              {submitting ? "Creating Agent..." : "New Agent"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
};
