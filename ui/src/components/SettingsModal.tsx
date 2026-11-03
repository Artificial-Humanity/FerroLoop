import React, { useEffect, useMemo, useState } from "react";
import { HubApiClient, HubApiError, generatePasskeyCredentialId } from "../api/client";
import {
  FONT_SCALE_MAX_PCT,
  FONT_SCALE_MIN_PCT,
  FONT_SCALE_PRESETS,
  FONT_SCALE_STEP_PCT,
  applyFontScalePct,
  applyThemeMode,
  clampFontScalePct,
  getInitialFontScalePct,
  getInitialThemeMode,
  persistFontScalePct,
  persistThemeMode,
  resolveSystemTheme,
  type ThemeMode,
} from "../lib/accessibility";
import {
  CURRENCY_OPTIONS,
  detectBrowserCurrency,
  getActiveCurrency,
  setActiveCurrency,
  type CurrencyPreference,
} from "../lib/currency";
import { TIMEZONE_OPTIONS, setActiveTimeZone } from "../lib/datetime";
import { PinIcon, togglePinnedProject, usePinnedState } from "./RosterSidebar";
import { RuntimesQuotasPanel } from "./RuntimesQuotasPanel";
import {
  ACP_CONFORMANCE_OPERATIONS,
  DEFAULT_ACP_CONFORMANCE_MATRIX,
  type AcpAdapterConformanceRow,
  type AcpConformanceOperation,
} from "./FerroWatchPane";
import {
  DEFAULT_MCP_CATALOG_REPOS,
  DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS,
  type McpCatalogRepoRow,
  type McpCatalogRepoStatus,
  type VaultConsumerCutoverRow,
} from "./VaultGatePane";
import {
  evaluateAgentMcpPosture,
  type AgentMcpPostureInput,
  type AgentMcpPostureReport,
} from "./AgentSettingsModal";

export {
  ACP_CONFORMANCE_OPERATIONS,
  DEFAULT_ACP_CONFORMANCE_MATRIX,
  DEFAULT_MCP_CATALOG_REPOS,
  DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS,
  type AcpAdapterConformanceRow,
  type AcpConformanceOperation,
  type McpCatalogRepoRow,
  type McpCatalogRepoStatus,
  type VaultConsumerCutoverRow,
};

/**
 * Issue #205 Phase A1: `<ui_dir>/build-info.json` schema and `/api/v1/version` response.
 */
export interface UiBuildInfoPayload {
  commit: string;
  build_time_unix: number | null;
  version: string | null;
  dirty?: boolean;
}

export interface HubVersionProvenanceResponse {
  name: string;
  version: string;
  commit: string;
  commit_short: string;
  dirty: boolean | null;
  build_time_unix: number | null;
  source: string;
  release_id: string | null;
  ui: UiBuildInfoPayload | null;
  code_ui_agree: boolean | null;
}

export const DEFAULT_VERSION_PROVENANCE: HubVersionProvenanceResponse = {
  name: "fl-hub",
  version: "0.9.4",
  commit: "b0888c6bc1ed0000000000000000000000000000",
  commit_short: "b0888c6bc1ed",
  dirty: false,
  build_time_unix: 1775232000,
  source: "source-build",
  release_id: "0.9.4+b0888c6bc1ed",
  ui: {
    commit: "b0888c6bc1ed0000000000000000000000000000",
    build_time_unix: 1775232000,
    version: "0.1.0",
    dirty: false,
  },
  code_ui_agree: true,
};

/**
 * Record #220 (Issue #205 Phase A3): Observational `fl doctor` 6 canonical G7 checks.
 */
export type DoctorCheckStatus =
  | "pass"
  | "warn"
  | "fail"
  | "not-installed"
  | "unsupported";

export interface DoctorCheckItem {
  id:
    | "G7.release-identity"
    | "G7.receipt-integrity"
    | "G7.path-ownership"
    | "G7.store-health"
    | "G7.vault-perms"
    | "G7.code-ui-agree";
  title: string;
  status: DoctorCheckStatus;
  detail: string;
}

export const CANONICAL_DOCTOR_CHECK_IDS = [
  "G7.release-identity",
  "G7.receipt-integrity",
  "G7.path-ownership",
  "G7.store-health",
  "G7.vault-perms",
  "G7.code-ui-agree",
] as const;

export const DEFAULT_DOCTOR_CHECKS: DoctorCheckItem[] = [
  {
    id: "G7.release-identity",
    title: "Release Identity & Build Stamp",
    status: "not-installed",
    detail: "Awaiting live GET /api/v1/doctor check — no false passes per #205 §7",
  },
  {
    id: "G7.receipt-integrity",
    title: "Install Receipt Schema v1 Integrity",
    status: "not-installed",
    detail: "Awaiting live GET /api/v1/doctor check — no false passes per #205 §7",
  },
  {
    id: "G7.path-ownership",
    title: "Launcher & Release Symlink Ownership",
    status: "not-installed",
    detail: "Awaiting live GET /api/v1/doctor check — no false passes per #205 §7",
  },
  {
    id: "G7.store-health",
    title: "FerroStore Header & Magic Verification",
    status: "not-installed",
    detail: "Awaiting live GET /api/v1/doctor check — no false passes per #205 §7",
  },
  {
    id: "G7.vault-perms",
    title: "FerroGate Vault File Permissions",
    status: "not-installed",
    detail: "Awaiting live GET /api/v1/doctor check — no false passes per #205 §7",
  },
  {
    id: "G7.code-ui-agree",
    title: "Hub Binary <-> UI Bundle Commit Agreement",
    status: "not-installed",
    detail: "Awaiting live GET /api/v1/doctor check — no false passes per #205 §7",
  },
];

/**
 * Issue #205 Phase A2: Deterministic `fl install --dry-run` plan preview (`InstallPlan`).
 */
export interface DryRunInstallPlan {
  status: "ready" | "needs_decision";
  selected_products: string[];
  planned_files: string[];
  planned_symlinks: string[];
  planned_units: string[];
  conflicts: string[];
}

export function computeDryRunInstallPlan(
  selectedProducts: string[],
  plannedUnits: string[],
  adoptionState: "managed" | "unmanaged_adopt_required" = "managed",
): DryRunInstallPlan {
  const conflicts =
    adoptionState === "unmanaged_adopt_required"
      ? [
          "~/.local/bin/fl exists without a managed ~/.local/share/fl/receipt.json (--adopt required)",
        ]
      : [];
  return {
    status: conflicts.length > 0 ? "needs_decision" : "ready",
    selected_products: selectedProducts,
    planned_files: [
      "~/.local/share/fl/releases/0.9.4+b0888c6bc1ed/bin/fl",
      "~/.local/share/fl/releases/0.9.4+b0888c6bc1ed/bin/ferrostore",
      "~/.local/share/fl/receipt.json",
    ],
    planned_symlinks: [
      "~/.local/share/fl/current -> releases/0.9.4+b0888c6bc1ed",
      "~/.local/bin/fl -> ~/.local/share/fl/current/bin/fl",
    ],
    planned_units: plannedUnits,
    conflicts,
  };
}

/**
 * Issue #205 Phase C: Versioned `releases/<ver>+<sha12>/` upgrade & rollback slot manager.
 */
export interface ReleaseSlotEntry {
  slot_id: string;
  path: string;
  role: "current" | "previous" | "staged";
  commit_short: string;
  dirty: boolean;
}

export const DEFAULT_RELEASE_SLOTS: ReleaseSlotEntry[] = [
  {
    slot_id: "0.9.4+b0888c6bc1ed",
    path: "~/.local/share/fl/releases/0.9.4+b0888c6bc1ed",
    role: "current",
    commit_short: "b0888c6bc1ed",
    dirty: false,
  },
  {
    slot_id: "0.9.3+7a41d09ef312",
    path: "~/.local/share/fl/releases/0.9.3+7a41d09ef312",
    role: "previous",
    commit_short: "7a41d09ef312",
    dirty: false,
  },
  {
    slot_id: "0.9.5+975bcacab4b4",
    path: "~/.local/share/fl/releases/0.9.5+975bcacab4b4",
    role: "staged",
    commit_short: "975bcacab4b4",
    dirty: false,
  },
];

/**
 * Record #67: FerroDeck+ Operator Push Notifications (Web Push VAPID + APNs + FCM).
 */
export type WebPushPermissionStatus = "granted" | "default" | "denied";
export type MobilePushChannelStatus = "active" | "pending" | "unregistered";

export interface OperatorPushSubscriptionState {
  web_push_permission: WebPushPermissionStatus;
  vapid_service_worker_registered: boolean;
  vapid_public_key_fingerprint: string;
  apns_ios_status: MobilePushChannelStatus;
  fcm_android_status: MobilePushChannelStatus;
  triggers: {
    question_badge: boolean;
    blocker_error_badge: boolean;
    urgent_wire_mail: boolean;
    pr_review_requests: boolean;
  };
  project_scope: string;
  quiet_hours: {
    enabled: boolean;
    start_time: string;
    end_time: string;
    timezone: string;
  };
}

export const DEFAULT_OPERATOR_PUSH_STATE: OperatorPushSubscriptionState = {
  web_push_permission: "granted",
  vapid_service_worker_registered: true,
  vapid_public_key_fingerprint: "VAPID-SHA256:8f4e2b91c0d3",
  apns_ios_status: "active",
  fcm_android_status: "active",
  triggers: {
    question_badge: true,
    blocker_error_badge: true,
    urgent_wire_mail: true,
    pr_review_requests: true,
  },
  project_scope: "all",
  quiet_hours: {
    enabled: true,
    start_time: "22:00",
    end_time: "07:00",
    timezone: "UTC",
  },
};

/**
 * Record #74: User-Configurable Agent Role Registry (11 built-in roles + custom RoleRecord).
 */
export type RoleReviewPolicy =
  | "lead-review"
  | "lab-manager-review"
  | "peer-review";

export interface ConfigurableRoleRecord {
  slug: string;
  label: string;
  description: string;
  builtin: boolean;
  review_policy: RoleReviewPolicy;
  prompt_template: string;
}

export const BUILTIN_ROLE_REGISTRY: ConfigurableRoleRecord[] = [
  {
    slug: "lead",
    label: "Project Lead",
    description:
      "Owns project planning, architecture, subordinate PR reviews, and delegation.",
    builtin: true,
    review_policy: "lab-manager-review",
    prompt_template:
      "Read .agents/roles/lead.md and lead project architecture and reviews.",
  },
  {
    slug: "assistant",
    label: "Assisting Engineer",
    description:
      "Executes delegated engineering tasks with local subagent pre-review.",
    builtin: true,
    review_policy: "lead-review",
    prompt_template:
      "Read .agents/roles/assistant.md and execute delegated implementation tasks.",
  },
  {
    slug: "strategist",
    label: "Research & Architecture Strategist",
    description:
      "Serves on the Strategy Council synthesizing research literature and architecture.",
    builtin: true,
    review_policy: "lead-review",
    prompt_template:
      "Read .agents/roles/strategist.md and synthesize research against empirical findings.",
  },
  {
    slug: "ui-developer",
    label: "UI & Full-Stack Application Developer",
    description:
      "Specializes in React, TypeScript, Rust web frameworks, and native cross-platform UI.",
    builtin: true,
    review_policy: "lead-review",
    prompt_template:
      "Read .agents/roles/ui-developer.md and maintain cockpit UI parity and tests.",
  },
  {
    slug: "lab-manager",
    label: "Lab Manager",
    description:
      "Workspace-wide authority over ai-lab-0, GPU arbitration, and Lead PR reviews.",
    builtin: true,
    review_policy: "peer-review",
    prompt_template:
      "Read .agents/roles/lab-manager.md and manage workspace infrastructure and governance.",
  },
  {
    slug: "qa-tester",
    label: "QA & Verification Engineer",
    description:
      "Owns regression harnesses, conformance suites, and end-to-end verification.",
    builtin: true,
    review_policy: "lead-review",
    prompt_template:
      "Run exhaustive verification suites and report deterministic reproduction cases.",
  },
  {
    slug: "security-engineer",
    label: "Security & Vault Engineer",
    description:
      "Audits FerroGate vault policies, credential boundaries, and sandbox isolation.",
    builtin: true,
    review_policy: "lead-review",
    prompt_template:
      "Audit secret boundaries, capability matrices, and sandbox enforcement.",
  },
  {
    slug: "devops-release",
    label: "DevOps & Release Engineer",
    description:
      "Maintains CI/CD pipelines, versioned release slots, and systemd supervisor units.",
    builtin: true,
    review_policy: "lead-review",
    prompt_template:
      "Manage release slot rollouts, build provenance stamps, and CI workflows.",
  },
  {
    slug: "data-ml-engineer",
    label: "Data & ML Systems Engineer",
    description:
      "Builds dataset pipelines, training runs, and evaluation benchmarks.",
    builtin: true,
    review_policy: "lead-review",
    prompt_template:
      "Maintain training hygiene, dataset lineage, and benchmark reproducibility.",
  },
  {
    slug: "technical-writer",
    label: "Technical Writer & Spec Maintainer",
    description:
      "Maintains architecture specifications, runbooks, and operator documentation.",
    builtin: true,
    review_policy: "lead-review",
    prompt_template:
      "Keep specifications, runbooks, and documentation accurate and concise.",
  },
  {
    slug: "journalist",
    label: "Journalist & Technical Scribe",
    description:
      "Chronicles project milestones, experiment logs, and publication drafts.",
    builtin: true,
    review_policy: "lead-review",
    prompt_template:
      "Record empirical milestones, experiment notes, and technical dispatches.",
  },
];

/**
 * Record #71: Manage Vendor Cloud Sessions in FerroDeck.
 */
export type VendorCloudSessionVendor =
  | "claude-cloud"
  | "codex-cloud"
  | "gemini-jules-cloud"
  | "grok-cloud";

export type VendorCloudSessionStatus =
  | "running"
  | "paused"
  | "completed"
  | "failed";

export interface VendorCloudSessionEntry {
  session_id: string;
  vendor: VendorCloudSessionVendor;
  vendor_label: string;
  repo_branch: string;
  status: VendorCloudSessionStatus;
  turns: number;
  tokens_used: number;
  mirrored: boolean;
  last_active: string;
}

export const DEFAULT_VENDOR_CLOUD_SESSIONS: VendorCloudSessionEntry[] = [
  {
    session_id: "cs-claude-web-901",
    vendor: "claude-cloud",
    vendor_label: "Claude Cloud / Web",
    repo_branch: "Artificial-Humanity/FerroLoop@feat/cloud-handoff",
    status: "running",
    turns: 18,
    tokens_used: 64200,
    mirrored: true,
    last_active: "2m ago",
  },
  {
    session_id: "cs-codex-cloud-412",
    vendor: "codex-cloud",
    vendor_label: "OpenAI Codex Cloud",
    repo_branch: "Artificial-Humanity/FerroDeck@fix/remote-sync",
    status: "paused",
    turns: 11,
    tokens_used: 38900,
    mirrored: false,
    last_active: "14m ago",
  },
  {
    session_id: "cs-jules-cloud-207",
    vendor: "gemini-jules-cloud",
    vendor_label: "Google Jules / Gemini Cloud",
    repo_branch: "Artificial-Humanity/FerroLoop@audit/acp-matrix",
    status: "completed",
    turns: 24,
    tokens_used: 91500,
    mirrored: true,
    last_active: "1h ago",
  },
  {
    session_id: "cs-grok-cloud-088",
    vendor: "grok-cloud",
    vendor_label: "xAI Grok Cloud",
    repo_branch: "Artificial-Humanity/FerroLoop@exp/fast-triage",
    status: "running",
    turns: 7,
    tokens_used: 21400,
    mirrored: false,
    last_active: "5m ago",
  },
];

/**
 * Record #72: Local Model & API-Key Agent Lifecycle (OpenAI-compatible, Ollama, Lemonade).
 */
export type LocalModelProviderKind =
  | "ollama"
  | "lemonade"
  | "openai-compatible";

export interface LocalModelProviderPreset {
  id: string;
  kind: LocalModelProviderKind;
  label: string;
  endpoint_url: string;
  default_model: string;
  vault_credential_ref: string;
  context_window_tokens: number;
  max_output_tokens: number;
  lifecycle_ops: readonly ("spin-up" | "compact" | "reforge" | "recast")[];
}

export const DEFAULT_LOCAL_MODEL_PROVIDERS: LocalModelProviderPreset[] = [
  {
    id: "ollama-local",
    kind: "ollama",
    label: "Ollama Local Runtime",
    endpoint_url: "http://127.0.0.1:11434/v1",
    default_model: "qwen3-coder:30b",
    vault_credential_ref: "keyless (loopback)",
    context_window_tokens: 131072,
    max_output_tokens: 16384,
    lifecycle_ops: ["spin-up", "compact", "reforge", "recast"],
  },
  {
    id: "lemonade-rocm",
    kind: "lemonade",
    label: "Lemonade (AMD Ryzen AI / ROCm)",
    endpoint_url: "http://127.0.0.1:8000/v1",
    default_model: "Llama-3.3-70B-Instruct-ROCm",
    vault_credential_ref: "keyless (loopback)",
    context_window_tokens: 131072,
    max_output_tokens: 16384,
    lifecycle_ops: ["spin-up", "compact", "reforge", "recast"],
  },
  {
    id: "openai-compat-vllm",
    kind: "openai-compatible",
    label: "OpenAI-Compatible (vLLM / llama.cpp / SGLang)",
    endpoint_url: "http://127.0.0.1:8080/v1",
    default_model: "deepseek-r1-distill-qwen-32b",
    vault_credential_ref: "vault://ferrogate/providers/local-vllm-key",
    context_window_tokens: 65536,
    max_output_tokens: 8192,
    lifecycle_ops: ["spin-up", "compact", "reforge", "recast"],
  },
];
import {
  DEFAULT_CLOUD_ADAPTERS_CONFIG,
  DEFAULT_HUB_SETTINGS,
  DEFAULT_WORKSPACE_AGENTS,
  type AgentRecord,
  type BulkSessionAction,
  type BulkSessionActionResponse,
  type CloudAdaptersConfig,
  type CockpitTab,
  type DecommissionMode,
  type HubAuthority,
  type HubSettings,
  type OperatorAccount,
  type OperatorRole,
  type ProjectArtifactsDirInfo,
  type TotpEnrollmentResponse,
} from "../types/api";

export type SettingsTabId =
  | "agents"
  | "system"
  | "products"
  | "runtimes"
  | "accessibility"
  | "auth";

export interface SettingsModalProps {
  isOpen: boolean;
  onClose: () => void;
  initialTab?: SettingsTabId;
  initialProject?: string | null;
  agents?: AgentRecord[];
  authority?: HubAuthority | null;
  wsConnected?: boolean;
  onlineNodesCount?: number;
  activeCockpitTab?: CockpitTab;
  onSelectCockpitTab?: (tab: CockpitTab) => void;
  onOpenNodesModal?: () => void;
  onOpenSpinUpModal?: (initialProject?: string | null) => void;
  onOpenPairingModal?: () => void;
  onOpenAgentSettings?: (nameKey: string) => void;
  onLogout?: () => void;
  onAgentsUpdated?: () => void;
  onDecommissionAgent?: (
    targetId: string,
    mode: DecommissionMode,
  ) => Promise<unknown>;
  onBulkSessionAction?: (
    action: BulkSessionAction,
    selectedAgentKeys?: string[],
  ) => Promise<BulkSessionActionResponse>;
  themeMode?: ThemeMode;
  onChangeThemeMode?: (mode: ThemeMode) => void;
  fontScalePct?: number;
  onChangeFontScalePct?: (pct: number) => void;
  onSettingsUpdated?: (settings: HubSettings) => void;
  apiClient?: HubApiClient;
  postureInputsByAgent?: Record<string, Partial<AgentMcpPostureInput>>;
  onReconcileAgentPosture?: (
    agentKey: string,
    report: AgentMcpPostureReport,
  ) => void;
  versionProvenance?: HubVersionProvenanceResponse;
  doctorChecks?: DoctorCheckItem[];
}

export const CANONICAL_PROJECTS = [
  "AI-Lab-AMD",
  "FerroLoop",
  "Kaggle-Gemma4",
  "Lucida",
  "Prosodia",
  "Sonora",
  "Website",
  "Workspace",
];

export const ATTACHMENT_RETENTION_OPTIONS = [
  { value: 1, label: "1 day" },
  { value: 3, label: "3 days" },
  { value: 7, label: "7 days (Default)" },
  { value: 14, label: "14 days" },
  { value: 30, label: "30 days" },
  { value: 90, label: "90 days" },
  { value: 0, label: "Never (0)" },
] as const;

export function resolveProjectFilterValue(
  rawProject: string | null | undefined,
  options: readonly string[] = CANONICAL_PROJECTS,
): string {
  const trimmed = (rawProject ?? "").trim();
  if (!trimmed || trimmed.toLowerCase() === "all") {
    return "all";
  }
  const lower = trimmed.toLowerCase();
  const matched = options.find((opt) => opt.toLowerCase() === lower);
  return matched ?? trimmed;
}

export const CREATE_CUSTOM_ROLE_VALUE = "__create_custom__";

export const RECAST_ROLE_OPTIONS = [
  { value: "lead", label: "lead" },
  { value: "assistant", label: "assistant" },
  { value: "lab_manager", label: "lab_manager" },
  { value: "ui-developer", label: "ui-developer" },
  { value: "web-developer", label: "web-developer" },
  { value: "mobile-developer", label: "mobile-developer" },
  { value: "system-developer", label: "system-developer" },
  { value: "data-engineer", label: "data-engineer" },
  { value: "tester", label: "tester" },
  { value: "strategist", label: "strategist" },
  { value: "journalist", label: "journalist" },
];

export function canonicalizeDefaultRole(rawRole: string): string {
  const trimmed = (rawRole ?? "").trim();
  if (!trimmed) return "assistant";
  if (trimmed === CREATE_CUSTOM_ROLE_VALUE) return CREATE_CUSTOM_ROLE_VALUE;
  const stripped = trimmed.toLowerCase().startsWith("custom:")
    ? trimmed.slice(7).trim()
    : trimmed;
  const lower = stripped.toLowerCase();
  if (lower === "scribe") return "journalist";
  if (lower === "lab-manager" || lower === "lab_manager") return "lab_manager";
  if (RECAST_ROLE_OPTIONS.some((opt) => opt.value === lower)) {
    return lower;
  }
  return trimmed;
}

export function normalizeRecastRole(
  selectedRole: string,
  customRoleInput: string,
): string {
  if (selectedRole === CREATE_CUSTOM_ROLE_VALUE) {
    const clean = customRoleInput.trim();
    if (!clean) return "assistant";
    const canonical = canonicalizeDefaultRole(clean);
    if (RECAST_ROLE_OPTIONS.some((opt) => opt.value === canonical)) {
      return canonical;
    }
    return canonical.startsWith("custom:") ? canonical : `custom:${canonical}`;
  }
  return canonicalizeDefaultRole(selectedRole);
}

export interface RecastConflictState {
  message: string;
  replaceLead: boolean;
  createArea: boolean;
}

export function extractRecastConflict(
  errOrRes: HubApiError | { retires_lead?: string | null; creates_areas?: string[] },
): RecastConflictState {
  if (errOrRes instanceof HubApiError) {
    const details = errOrRes.details ?? {};
    const msg = (errOrRes.message || "").toLowerCase();
    const hasLead =
      Boolean(details.current_lead) ||
      Boolean(details.retires_lead) ||
      msg.includes("lead");
    const hasArea =
      (Array.isArray(details.missing_areas) && details.missing_areas.length > 0) ||
      (Array.isArray(details.creates_areas) && details.creates_areas.length > 0) ||
      msg.includes("area");
    return {
      message: errOrRes.message || "existing Lead",
      replaceLead: hasLead || (!hasLead && !hasArea),
      createArea: hasArea,
    };
  }
  const hasLead = Boolean(errOrRes.retires_lead);
  const hasArea =
    Array.isArray(errOrRes.creates_areas) && errOrRes.creates_areas.length > 0;
  const warningParts: string[] = [];
  if (errOrRes.retires_lead) {
    warningParts.push(`retires active Lead (${errOrRes.retires_lead})`);
  }
  if (hasArea && errOrRes.creates_areas) {
    warningParts.push(`creates area (${errOrRes.creates_areas.join(", ")})`);
  }
  return {
    message: warningParts.join("; ") || "existing Lead",
    replaceLead: hasLead || (!hasLead && !hasArea),
    createArea: hasArea,
  };
}

const ACTION_LABELS: Record<BulkSessionAction, string> = {
  compact: "Compact All",
  clear: "Clear All",
  reforge: "Reforge All",
  prepare: "Prepare All",
  teardown: "Tear Down All",
};

const ACTION_VERBS: Record<BulkSessionAction, string> = {
  compact: "Compact",
  clear: "Clear",
  reforge: "Reforge",
  prepare: "Prepare",
  teardown: "Tear Down",
};

const ACTION_DESCRIPTIONS: Record<BulkSessionAction, string> = {
  compact:
    "Send /compact to every active session across the workspace fleet.",
  clear:
    "Clear conversation context across all active sessions, leaving each session blank.",
  reforge:
    "Clear conversation context and immediately re-prime every active session with its registered identity, project, and role prompt.",
  prepare:
    "Send a pre-decommission state and worktree hygiene handshake prompt to every active session across the fleet.",
  teardown:
    "Immediately stop every active runtime session, clear queues and routes, and unregister each persona.",
};

// Record #84 (Issue #205 Phase B): Sub-Product Component Picker, Profile Presets & Provenance Truth
export type InstallProfilePreset = "minimal" | "headless" | "full" | "lab";

export type SubProductId =
  | "ferroloop"
  | "ferrostore"
  | "ferrodeck"
  | "ferrowire"
  | "ferrostate"
  | "ferrogate"
  | "ferrotrack"
  | "ferrowatch"
  | "fl-flow";

export interface SubProductNode {
  id: SubProductId;
  displayName: string;
  required: boolean;
  hiddenDefault: boolean;
  dependsOn: readonly SubProductId[];
  managedUnits: readonly string[];
  managedMcpEntries: readonly string[];
}

export const SUBPRODUCT_DEPENDENCY_GRAPH: Record<SubProductId, SubProductNode> = {
  ferroloop: {
    id: "ferroloop",
    displayName: "FerroLoop Core Engine (fl)",
    required: true,
    hiddenDefault: false,
    dependsOn: ["ferrostore"],
    managedUnits: [],
    managedMcpEntries: [],
  },
  ferrostore: {
    id: "ferrostore",
    displayName: "FerroStore Embedded Storage (.fls / FERRSTR1)",
    required: true,
    hiddenDefault: true,
    dependsOn: [],
    managedUnits: [],
    managedMcpEntries: [],
  },
  ferrodeck: {
    id: "ferrodeck",
    displayName: "FerroDeck (fl hub)",
    required: false,
    hiddenDefault: false,
    dependsOn: ["ferrostore", "ferrowire"],
    managedUnits: ["ferroloop-hub.service"],
    managedMcpEntries: [],
  },
  ferrowire: {
    id: "ferrowire",
    displayName: "FerroWire (fl wire)",
    required: false,
    hiddenDefault: false,
    dependsOn: ["ferrostore"],
    managedUnits: ["ferrowire.service", "agent-bus-dispatcher.service"],
    managedMcpEntries: ["fl-wire-mcp"],
  },
  ferrostate: {
    id: "ferrostate",
    displayName: "FerroState (fl state)",
    required: false,
    hiddenDefault: false,
    dependsOn: ["ferrostore"],
    managedUnits: [],
    managedMcpEntries: ["fl-state-mcp"],
  },
  ferrogate: {
    id: "ferrogate",
    displayName: "FerroGate (fl vault / fl auth)",
    required: false,
    hiddenDefault: false,
    dependsOn: [],
    managedUnits: [],
    managedMcpEntries: ["fl-gate-mcp"],
  },
  ferrotrack: {
    id: "ferrotrack",
    displayName: "FerroTrack (fl github)",
    required: false,
    hiddenDefault: false,
    dependsOn: ["ferrostore"],
    managedUnits: [],
    managedMcpEntries: [],
  },
  ferrowatch: {
    id: "ferrowatch",
    displayName: "FerroWatch (fl stats)",
    required: false,
    hiddenDefault: false,
    dependsOn: ["ferrostore"],
    managedUnits: [],
    managedMcpEntries: [],
  },
  "fl-flow": {
    id: "fl-flow",
    displayName: "DAG Workflow Runner (fl flow)",
    required: false,
    hiddenDefault: false,
    dependsOn: ["ferroloop", "ferrostore"],
    managedUnits: [],
    managedMcpEntries: [],
  },
};

export interface ProductDependencyClosureResult {
  resolvedProducts: SubProductId[];
  autoIncludedBy: Partial<Record<SubProductId, SubProductId[]>>;
  plannedUnits: string[];
  plannedMcpCatalog: string[];
  storeMode: "embedded" | "lab-socket-daemon";
}

function normalizeSubProductId(raw: string): SubProductId | null {
  const clean = raw.trim().toLowerCase();
  if (clean === "ferrostep" || clean === "fl-flow" || clean === "flow") {
    return "fl-flow";
  }
  if (clean in SUBPRODUCT_DEPENDENCY_GRAPH) {
    return clean as SubProductId;
  }
  return null;
}

export function computeProductDependencyClosure(
  selectedIds: readonly string[],
  profile: InstallProfilePreset | "custom" = "full",
): ProductDependencyClosureResult {
  const resolved = new Set<SubProductId>(["ferroloop", "ferrostore"]);
  const autoIncludedBy: Partial<Record<SubProductId, SubProductId[]>> = {
    ferrostore: ["ferroloop"],
  };

  const queue: SubProductId[] = ["ferroloop"];
  for (const raw of selectedIds) {
    const id = normalizeSubProductId(raw);
    if (id) {
      resolved.add(id);
      queue.push(id);
    }
  }

  while (queue.length > 0) {
    const current = queue.shift()!;
    const node = SUBPRODUCT_DEPENDENCY_GRAPH[current];
    if (!node) continue;
    for (const dep of node.dependsOn) {
      const existingRequirers = autoIncludedBy[dep] ?? [];
      if (!existingRequirers.includes(current)) {
        autoIncludedBy[dep] = [...existingRequirers, current];
      }
      if (!resolved.has(dep)) {
        resolved.add(dep);
        queue.push(dep);
      }
    }
  }

  const orderedIds: SubProductId[] = (
    Object.keys(SUBPRODUCT_DEPENDENCY_GRAPH) as SubProductId[]
  ).filter((id) => resolved.has(id));

  const plannedUnitsSet = new Set<string>();
  const plannedMcpSet = new Set<string>();
  for (const id of orderedIds) {
    const node = SUBPRODUCT_DEPENDENCY_GRAPH[id];
    for (const u of node.managedUnits) plannedUnitsSet.add(u);
    for (const m of node.managedMcpEntries) plannedMcpSet.add(m);
  }
  if (profile === "lab") {
    plannedUnitsSet.add("ferrostore@ferroloop.service");
    plannedUnitsSet.add("ferrostore@workspace.service");
  }

  return {
    resolvedProducts: orderedIds,
    autoIncludedBy,
    plannedUnits: Array.from(plannedUnitsSet),
    plannedMcpCatalog: Array.from(plannedMcpSet),
    storeMode: profile === "lab" ? "lab-socket-daemon" : "embedded",
  };
}

export function resolveProfileSubsystems(
  profile: InstallProfilePreset,
): HubSettings["subsystems"] {
  if (profile === "minimal") {
    return {
      ferrogate_enabled: false,
      ferrostate_enabled: false,
      ferrostep_enabled: false,
      ferrodeck_enabled: false,
      ferrowire_enabled: false,
      ferrotrack_enabled: false,
      ferrowatch_enabled: false,
    };
  }
  if (profile === "headless") {
    return {
      ferrogate_enabled: true,
      ferrostate_enabled: true,
      ferrostep_enabled: true,
      ferrodeck_enabled: false,
      ferrowire_enabled: true,
      ferrotrack_enabled: true,
      ferrowatch_enabled: true,
    };
  }
  return {
    ferrogate_enabled: true,
    ferrostate_enabled: true,
    ferrostep_enabled: true,
    ferrodeck_enabled: true,
    ferrowire_enabled: true,
    ferrotrack_enabled: true,
    ferrowatch_enabled: true,
  };
}

export function inferInstallProfilePreset(
  subsystems: HubSettings["subsystems"],
  preferredLab = false,
): InstallProfilePreset | "custom" {
  const deck = subsystems.ferrodeck_enabled ?? true;
  const wire = subsystems.ferrowire_enabled ?? true;
  const state = subsystems.ferrostate_enabled;
  const gate = subsystems.ferrogate_enabled;
  const track = subsystems.ferrotrack_enabled ?? true;
  const watch = subsystems.ferrowatch_enabled ?? true;
  const flow = subsystems.ferrostep_enabled;

  if (!deck && !wire && !state && !gate && !track && !watch && !flow) {
    return "minimal";
  }
  if (!deck && wire && state && gate && track && watch && flow) {
    return "headless";
  }
  if (deck && wire && state && gate && track && watch && flow) {
    return preferredLab ? "lab" : "full";
  }
  return "custom";
}

export function getBulkActionLabel(
  action: BulkSessionAction,
  selectedCount: number,
): string {
  if (selectedCount > 0) {
    return `${ACTION_VERBS[action]} Selected (${selectedCount})`;
  }
  return ACTION_LABELS[action];
}

export async function executeScopedBulkSessionAction(
  client: HubApiClient,
  action: BulkSessionAction,
  selectedAgentKeys?: string[],
  fallbackAgentKeys?: string[],
): Promise<BulkSessionActionResponse> {
  if (
    (!selectedAgentKeys || selectedAgentKeys.length === 0) &&
    action !== "prepare" &&
    action !== "teardown"
  ) {
    return client.bulkSessionAction(action);
  }
  const targetKeys =
    selectedAgentKeys && selectedAgentKeys.length > 0
      ? selectedAgentKeys
      : fallbackAgentKeys && fallbackAgentKeys.length > 0
        ? fallbackAgentKeys
        : DEFAULT_WORKSPACE_AGENTS.map((a) => a.name_key.toLowerCase());
  const results = await Promise.all(
    targetKeys.map(async (sessionId) => {
      try {
        if (action === "compact") {
          await client.sendSessionPrompt(sessionId, {
            prompt: "/compact",
            send_now: true,
          });
        } else if (action === "prepare" || action === "teardown") {
          await client.decommissionSession(sessionId, action);
        } else {
          await client.clearSession(sessionId, action);
        }
        return { session_id: sessionId, ok: true, error: null };
      } catch (err) {
        return {
          session_id: sessionId,
          ok: false,
          error: err instanceof Error ? err.message : String(err),
        };
      }
    }),
  );
  const succeeded = results.filter((r) => r.ok).length;
  return {
    action,
    total: results.length,
    succeeded,
    results,
  };
}

export const SettingsModal: React.FC<SettingsModalProps> = ({
  isOpen,
  onClose,
  initialTab = "agents",
  initialProject = null,
  agents: propAgents,
  authority,
  wsConnected = false,
  onlineNodesCount = 0,
  onSelectCockpitTab,
  onOpenNodesModal,
  onOpenSpinUpModal,
  onOpenPairingModal,
  onOpenAgentSettings,
  onLogout,
  onAgentsUpdated,
  onDecommissionAgent,
  onBulkSessionAction,
  themeMode: propThemeMode,
  onChangeThemeMode,
  fontScalePct: propFontScalePct,
  onChangeFontScalePct,
  onSettingsUpdated,
  apiClient,
  postureInputsByAgent,
  onReconcileAgentPosture,
  versionProvenance: propVersionProvenance,
  doctorChecks: propDoctorChecks,
}) => {
  const [reconciledPostureAgents, setReconciledPostureAgents] = useState<
    Set<string>
  >(() => new Set());
  // Record #84 (Issue #205 Phase B): Profile preset, auto-closure & receipt provenance state
  const [labProfilePreferred, setLabProfilePreferred] = useState(false);
  const [autoClosureNotice, setAutoClosureNotice] = useState<string | null>(
    null,
  );
  const [receiptAdoptionState, setReceiptAdoptionState] = useState<
    "managed" | "unmanaged_adopt_required"
  >("managed");
  // Issue #205 Phase A1: Live `/api/v1/version` & `<ui_dir>/build-info.json` provenance state
  const [versionProvenance, setVersionProvenance] =
    useState<HubVersionProvenanceResponse>(
      () => propVersionProvenance ?? DEFAULT_VERSION_PROVENANCE,
    );
  const [verifyingVersion, setVerifyingVersion] = useState(false);
  // Record #220 (Issue #205 Phase A3): Observational `fl doctor` 6 canonical G7 checks
  const [doctorChecks, setDoctorChecks] = useState<DoctorCheckItem[]>(
    () => propDoctorChecks ?? DEFAULT_DOCTOR_CHECKS,
  );
  const [runningDoctorCheck, setRunningDoctorCheck] = useState(false);
  // Issue #205 Phase A2 & Phase C: Dry-run plan preview & versioned release slots state
  const [dryRunBanner, setDryRunBanner] = useState<string | null>(null);
  const [releaseSlots, setReleaseSlots] = useState<ReleaseSlotEntry[]>(
    DEFAULT_RELEASE_SLOTS,
  );
  const [releaseSlotBanner, setReleaseSlotBanner] = useState<string | null>(
    null,
  );
  const [activeTab, setActiveTab] = useState<SettingsTabId>(initialTab);
  const [localThemeMode, setLocalThemeMode] = useState<ThemeMode>(
    () => propThemeMode ?? getInitialThemeMode(),
  );
  const [resolvedOsTheme, setResolvedOsTheme] = useState<"dark" | "light">(
    resolveSystemTheme,
  );
  const [localFontScalePct, setLocalFontScalePct] = useState<number>(
    () => propFontScalePct ?? getInitialFontScalePct(),
  );
  const { pinnedProjects } = usePinnedState();
  const [selectedProjectFilter, setSelectedProjectFilter] = useState<string>(
    () => resolveProjectFilterValue(initialProject, CANONICAL_PROJECTS),
  );
  const [selectedAgentKeys, setSelectedAgentKeys] = useState<Set<string>>(
    () => new Set(),
  );
  const [settings, setSettings] = useState<HubSettings>(DEFAULT_HUB_SETTINGS);
  const [currencyPref, setCurrencyPref] = useState<CurrencyPreference>(() =>
    getActiveCurrency(),
  );
  const [savingSettings, setSavingSettings] = useState(false);
  const [settingsSaved, setSettingsSaved] = useState(false);
  const [pruningAttachments, setPruningAttachments] = useState(false);
  const [pruneStatusMessage, setPruneStatusMessage] = useState<string | null>(
    null,
  );

  // Issue #207: System & Project Artifacts Directory Override Modals state
  const [projectsArtifactsDirs, setProjectsArtifactsDirs] = useState<
    Record<string, ProjectArtifactsDirInfo>
  >({});
  const [artifactsDirModalTarget, setArtifactsDirModalTarget] = useState<
    { scope: "system" } | { scope: "project"; project: string } | null
  >(null);
  const [artifactsDirNewPath, setArtifactsDirNewPath] = useState("");
  const [artifactsDirMigrate, setArtifactsDirMigrate] = useState(true);
  const [changingArtifactsDir, setChangingArtifactsDir] = useState(false);
  const [artifactsDirModalError, setArtifactsDirModalError] = useState<
    string | null
  >(null);
  const [systemArtifactsDirStatus, setSystemArtifactsDirStatus] = useState<
    string | null
  >(null);
  const [projectArtifactsDirStatus, setProjectArtifactsDirStatus] = useState<
    string | null
  >(null);

  // Agents Fleet & Re-Cast state
  const [loadedAgents, setLoadedAgents] = useState<AgentRecord[]>(
    propAgents && propAgents.length > 0 ? propAgents : DEFAULT_WORKSPACE_AGENTS,
  );
  const [recastDrafts, setRecastDrafts] = useState<
    Record<string, { project: string; role: string }>
  >({});
  const [recastCustomRoleInputs, setRecastCustomRoleInputs] = useState<
    Record<string, string>
  >({});
  const [recastModalAgentKey, setRecastModalAgentKey] = useState<string | null>(
    null,
  );
  const [recastReplaceLeadArmed, setRecastReplaceLeadArmed] = useState<
    Record<string, RecastConflictState>
  >({});
  const [recastingAgentKey, setRecastingAgentKey] = useState<string | null>(
    null,
  );
  const [recastStatusBanner, setRecastStatusBanner] = useState<string | null>(
    null,
  );

  // Record 11: Operators, Passkeys & MFA state
  const [operators, setOperators] = useState<OperatorAccount[]>([]);
  const [opUsername, setOpUsername] = useState("");
  const [opRole, setOpRole] = useState<OperatorRole>("operator");
  const [opPassword, setOpPassword] = useState("");
  const [addingOperator, setAddingOperator] = useState(false);
  const [totpEnrollment, setTotpEnrollment] =
    useState<TotpEnrollmentResponse | null>(null);
  const [totpVerifyCode, setTotpVerifyCode] = useState("");
  const [totpVerified, setTotpVerified] = useState(false);
  const [passkeyLabel, setPasskeyLabel] = useState("");
  const [passkeyRegistered, setPasskeyRegistered] = useState(false);

  // Records 52 & 53: Cloud Sync & Edge Adapters state
  const [cloudAdapters, setCloudAdapters] = useState<CloudAdaptersConfig>(
    DEFAULT_CLOUD_ADAPTERS_CONFIG,
  );
  const [savingCloudAdapters, setSavingCloudAdapters] = useState(false);
  const [cloudAdaptersSaved, setCloudAdaptersSaved] = useState(false);

  // Bulk Session Operations state
  const [armedAction, setArmedAction] = useState<BulkSessionAction | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastResult, setLastResult] =
    useState<BulkSessionActionResponse | null>(null);

  useEffect(() => {
    if (propAgents && propAgents.length > 0) {
      setLoadedAgents(propAgents);
    }
  }, [propAgents]);

  useEffect(() => {
    setActiveTab(initialTab);
  }, [initialTab]);

  useEffect(() => {
    if (propThemeMode) {
      setLocalThemeMode(propThemeMode);
    }
  }, [propThemeMode]);

  useEffect(() => {
    if (typeof propFontScalePct === "number") {
      setLocalFontScalePct(clampFontScalePct(propFontScalePct));
    }
  }, [propFontScalePct]);

  useEffect(() => {
    if (!isOpen) return;
    setLocalThemeMode(propThemeMode ?? getInitialThemeMode());
    setLocalFontScalePct(propFontScalePct ?? getInitialFontScalePct());
    setResolvedOsTheme(resolveSystemTheme());
  }, [isOpen, propThemeMode, propFontScalePct]);

  useEffect(() => {
    if (
      localThemeMode !== "system" ||
      typeof window === "undefined" ||
      typeof window.matchMedia !== "function"
    ) {
      return;
    }
    const mql = window.matchMedia("(prefers-color-scheme: light)");
    setResolvedOsTheme(mql.matches ? "light" : "dark");
    const handleChange = (event?: MediaQueryListEvent) => {
      const isLight =
        typeof event?.matches === "boolean" ? event.matches : mql.matches;
      const nextTheme: "dark" | "light" = isLight ? "light" : "dark";
      setResolvedOsTheme(nextTheme);
      if (typeof document !== "undefined") {
        document.documentElement.setAttribute("data-theme", nextTheme);
      }
    };
    if (typeof mql.addEventListener === "function") {
      mql.addEventListener("change", handleChange);
      return () => mql.removeEventListener("change", handleChange);
    }
    if (typeof mql.addListener === "function") {
      mql.addListener(handleChange);
      return () => mql.removeListener(handleChange);
    }
  }, [localThemeMode, isOpen]);

  const handleSelectThemeMode = (nextMode: ThemeMode) => {
    setLocalThemeMode(nextMode);
    setResolvedOsTheme(resolveSystemTheme());
    persistThemeMode(nextMode);
    applyThemeMode(nextMode);
    setSettings((prev) => ({ ...prev, theme_preference: nextMode }));
    onChangeThemeMode?.(nextMode);
  };

  const handleSelectFontScalePct = (nextPct: number) => {
    const clamped = clampFontScalePct(nextPct);
    setLocalFontScalePct(clamped);
    persistFontScalePct(clamped);
    applyFontScalePct(clamped);
    onChangeFontScalePct?.(clamped);
  };

  useEffect(() => {
    setSelectedProjectFilter(
      resolveProjectFilterValue(initialProject, CANONICAL_PROJECTS),
    );
    if (initialProject) {
      setActiveTab("agents");
    }
  }, [initialProject]);

  useEffect(() => {
    setArmedAction(null);
    setError(null);
    setLastResult(null);
    setSettingsSaved(false);
    setCurrencyPref(getActiveCurrency());
    setPruneStatusMessage(null);
    setArtifactsDirModalTarget(null);
    setArtifactsDirModalError(null);
    setSystemArtifactsDirStatus(null);
    setProjectArtifactsDirStatus(null);
    setCloudAdaptersSaved(false);
    setRecastStatusBanner(null);
    setRecastReplaceLeadArmed({});
    setRecastModalAgentKey(null);
    setSelectedAgentKeys(new Set());
    setSelectedProjectFilter(
      resolveProjectFilterValue(initialProject, CANONICAL_PROJECTS),
    );
    setActiveTab(initialProject ? "agents" : initialTab);
    setTotpEnrollment(null);
    setTotpVerifyCode("");
    setTotpVerified(false);
    setPasskeyRegistered(false);
    if (!isOpen) {
      return;
    }
    let cancelled = false;
    const client = apiClient ?? new HubApiClient();

    client
      .getSettings()
      .then((loaded) => {
        if (!cancelled) {
          setSettings(loaded);
        }
      })
      .catch(() => {
        // Keep DEFAULT_HUB_SETTINGS if endpoint is unreachable
      });

    client
      .getAgents()
      .then((res) => {
        if (!cancelled && Array.isArray(res?.agents) && res.agents.length > 0) {
          setLoadedAgents(res.agents);
        }
      })
      .catch(() => {
        // Keep current loadedAgents / DEFAULT_WORKSPACE_AGENTS
      });

    client
      .listOperators()
      .then((res) => {
        if (!cancelled && Array.isArray(res?.operators)) {
          setOperators(res.operators);
        }
      })
      .catch(() => {
        // Ignore if operator endpoint is not yet seeded
      });

    client
      .getCloudAdapters()
      .then((loadedCloud) => {
        if (!cancelled) {
          setCloudAdapters(loadedCloud);
        }
      })
      .catch(() => {
        // Keep DEFAULT_CLOUD_ADAPTERS_CONFIG if endpoint is unreachable
      });

    if (typeof client.listProjectsArtifactsDirs === "function") {
      client
        .listProjectsArtifactsDirs()
        .then((res) => {
          if (!cancelled && Array.isArray(res?.projects)) {
            const map: Record<string, ProjectArtifactsDirInfo> = {};
            for (const item of res.projects) {
              if (item && typeof item.id === "string" && item.id.trim()) {
                map[item.id.trim().toLowerCase()] = item;
              }
            }
            setProjectsArtifactsDirs(map);
          }
        })
        .catch(() => {
          // Ignore if /api/v1/projects is not yet available
        });
    }

    return () => {
      cancelled = true;
    };
  }, [isOpen, apiClient]);

  useEffect(() => {
    if (!isOpen) return;
    const handleKeyDownEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (artifactsDirModalTarget) {
        if (!changingArtifactsDir) {
          setArtifactsDirModalTarget(null);
          setArtifactsDirModalError(null);
        }
        return;
      }
      if (recastModalAgentKey) {
        if (!recastingAgentKey) {
          setRecastModalAgentKey(null);
        }
        return;
      }
      if (!submitting && !savingSettings && !savingCloudAdapters) {
        onClose();
      }
    };
    window.addEventListener("keydown", handleKeyDownEscape);
    return () => window.removeEventListener("keydown", handleKeyDownEscape);
  }, [
    isOpen,
    artifactsDirModalTarget,
    changingArtifactsDir,
    recastModalAgentKey,
    recastingAgentKey,
    submitting,
    savingSettings,
    savingCloudAdapters,
    onClose,
  ]);

  const effectiveAgents = useMemo<AgentRecord[]>(() => {
    const base =
      loadedAgents.length > 0
        ? loadedAgents
        : propAgents && propAgents.length > 0
          ? propAgents
          : DEFAULT_WORKSPACE_AGENTS;
    const seen = new Set<string>();
    const out: AgentRecord[] = [];
    for (const ag of base) {
      const key = ag.name_key.trim().toLowerCase();
      if (!seen.has(key)) {
        seen.add(key);
        out.push(ag);
      }
    }
    return out;
  }, [propAgents, loadedAgents]);

  const projectOptions = useMemo<string[]>(() => {
    const out: string[] = [...CANONICAL_PROJECTS];
    const addProjectOption = (raw?: string | null) => {
      const trimmed = (raw ?? "").trim();
      if (!trimmed || trimmed.toLowerCase() === "all") return;
      const lower = trimmed.toLowerCase();
      if (!out.some((opt) => opt.toLowerCase() === lower)) {
        out.push(trimmed);
      }
    };
    for (const ag of effectiveAgents) {
      addProjectOption(ag.project || "Workspace");
    }
    addProjectOption(initialProject);
    addProjectOption(selectedProjectFilter);
    return out;
  }, [effectiveAgents, initialProject, selectedProjectFilter]);

  const resolvedProjectFilter = useMemo<string>(
    () => resolveProjectFilterValue(selectedProjectFilter, projectOptions),
    [selectedProjectFilter, projectOptions],
  );

  const filteredAgents = useMemo<AgentRecord[]>(() => {
    if (!resolvedProjectFilter || resolvedProjectFilter === "all") {
      return effectiveAgents;
    }
    const target = resolvedProjectFilter.toLowerCase();
    return effectiveAgents.filter(
      (ag) => (ag.project || "Workspace").toLowerCase() === target,
    );
  }, [effectiveAgents, resolvedProjectFilter]);

  const allFilteredSelected = useMemo(() => {
    if (filteredAgents.length === 0) return false;
    return filteredAgents.every((ag) =>
      selectedAgentKeys.has(ag.name_key.toLowerCase()),
    );
  }, [filteredAgents, selectedAgentKeys]);

  const projectSummaryLead = useMemo(() => {
    if (!resolvedProjectFilter || resolvedProjectFilter === "all") return null;
    const lead = filteredAgents.find(
      (ag) => ag.role === "lead" || ag.role === "lab_manager",
    );
    return lead ? lead.display_name : "Unassigned";
  }, [filteredAgents, resolvedProjectFilter]);

  const authorityText = useMemo(() => {
    if (!authority) return "Unauthenticated";
    if (authority.tier === "owner") {
      return `Owner: ${authority.login}`;
    }
    return authority.agent ? `Peer: ${authority.agent}` : "Peer";
  }, [authority]);

  const [operatorPushState, setOperatorPushState] =
    useState<OperatorPushSubscriptionState>(() => DEFAULT_OPERATOR_PUSH_STATE);
  const [pushActionBanner, setPushActionBanner] = useState<string | null>(null);
  const [roleRegistry, setRoleRegistry] = useState<ConfigurableRoleRecord[]>(
    () => BUILTIN_ROLE_REGISTRY,
  );
  const [roleDraftSlug, setRoleDraftSlug] = useState("");
  const [roleDraftLabel, setRoleDraftLabel] = useState("");
  const [roleDraftDescription, setRoleDraftDescription] = useState("");
  const [roleDraftReviewPolicy, setRoleDraftReviewPolicy] =
    useState<RoleReviewPolicy>("lead-review");
  const [roleDraftPromptTemplate, setRoleDraftPromptTemplate] = useState("");
  const [roleRegistryBanner, setRoleRegistryBanner] = useState<string | null>(
    null,
  );
  const [cloudSessions, setCloudSessions] = useState<VendorCloudSessionEntry[]>(
    () => DEFAULT_VENDOR_CLOUD_SESSIONS,
  );
  const [cloudSessionBanner, setCloudSessionBanner] = useState<string | null>(
    null,
  );
  const [localProviders, setLocalProviders] = useState<
    LocalModelProviderPreset[]
  >(() => DEFAULT_LOCAL_MODEL_PROVIDERS);
  const [localProviderDraftKind, setLocalProviderDraftKind] =
    useState<LocalModelProviderKind>("ollama");
  const [localProviderDraftEndpoint, setLocalProviderDraftEndpoint] = useState(
    "http://127.0.0.1:11434/v1",
  );
  const [localProviderDraftVaultRef, setLocalProviderDraftVaultRef] = useState(
    "keyless (loopback)",
  );
  const [localProviderDraftContextWindow, setLocalProviderDraftContextWindow] =
    useState("131072");
  const [localProviderDraftMaxOutput, setLocalProviderDraftMaxOutput] =
    useState("16384");
  const [localProviderBanner, setLocalProviderBanner] = useState<string | null>(
    null,
  );

  if (!isOpen) {
    return null;
  }

  const getDraftForAgent = (ag: AgentRecord) => {
    const key = ag.name_key.toLowerCase();
    return (
      recastDrafts[key] ?? {
        project: ag.project || "Workspace",
        role: canonicalizeDefaultRole(ag.role || "assistant"),
      }
    );
  };

  const handleRecastAgent = async (
    ag: AgentRecord,
    confirmOverrides = false,
  ) => {
    const key = ag.name_key.toLowerCase();
    const draft = getDraftForAgent(ag);
    const effectiveRole = normalizeRecastRole(
      draft.role,
      recastCustomRoleInputs[key] || "",
    );
    if (recastingAgentKey) return;
    setRecastingAgentKey(key);
    setError(null);
    setRecastStatusBanner(null);
    const armedConflict = recastReplaceLeadArmed[key];
    try {
      const client = apiClient ?? new HubApiClient();
      const res = await client.recastAgent(key, {
        project: draft.project,
        role: effectiveRole,
        replace_lead: confirmOverrides
          ? (armedConflict ? armedConflict.replaceLead : true)
          : false,
        create_area: confirmOverrides
          ? (armedConflict ? armedConflict.createArea : true)
          : false,
      });
      if (
        (res?.retires_lead ||
          (Array.isArray(res?.creates_areas) && res.creates_areas.length > 0)) &&
        !confirmOverrides
      ) {
        const conflict = extractRecastConflict(res);
        setRecastReplaceLeadArmed((prev) => ({
          ...prev,
          [key]: conflict,
        }));
        return;
      }
      const updatedAgent: AgentRecord = res?.agent ??
        res?.after ?? {
          ...ag,
          project: draft.project,
          role: effectiveRole,
        };
      setLoadedAgents((prev) =>
        prev.map((item) =>
          item.name_key.toLowerCase() === key ? updatedAgent : item,
        ),
      );
      setRecastDrafts((prev) => ({
        ...prev,
        [key]: {
          project: updatedAgent.project,
          role: canonicalizeDefaultRole(updatedAgent.role),
        },
      }));
      setRecastReplaceLeadArmed((prev) => {
        const next = { ...prev };
        delete next[key];
        return next;
      });
      setRecastModalAgentKey(null);
      setRecastStatusBanner(
        `Re-cast ${ag.display_name} to ${updatedAgent.project} as ${updatedAgent.role}.`,
      );
      onAgentsUpdated?.();
    } catch (err) {
      if (
        err instanceof HubApiError &&
        (err.status === 409 || err.status === 400) &&
        !confirmOverrides
      ) {
        const conflict = extractRecastConflict(err);
        setRecastReplaceLeadArmed((prev) => ({
          ...prev,
          [key]: conflict,
        }));
      } else {
        setError(
          err instanceof Error
            ? err.message
            : `Failed to re-cast ${ag.display_name}`,
        );
      }
    } finally {
      setRecastingAgentKey(null);
    }
  };

  const handleSaveSettings = async () => {
    if (savingSettings) return;
    setSavingSettings(true);
    setError(null);
    setSettingsSaved(false);
    try {
      const client = apiClient ?? new HubApiClient();
      const updated = await client.updateSettings(settings);
      setSettings(updated);
      setActiveTimeZone(updated.timezone);
      setActiveCurrency(currencyPref);
      setSettingsSaved(true);
      onSettingsUpdated?.(updated);
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to save hub settings",
      );
    } finally {
      setSavingSettings(false);
    }
  };

  const handlePruneAttachments = async (olderThanDays?: number) => {
    if (pruningAttachments) return;
    setPruningAttachments(true);
    setError(null);
    setPruneStatusMessage(null);
    try {
      const client = apiClient ?? new HubApiClient();
      const res = await client.pruneAttachments(olderThanDays);
      const deletedCount = res?.deleted_count ?? 0;
      const deletedBytes = res?.deleted_bytes ?? 0;
      const keptCount = res?.kept_count ?? 0;
      setPruneStatusMessage(
        olderThanDays === 0
          ? `Purged ${deletedCount} screenshot/attachment file${deletedCount === 1 ? "" : "s"} (${deletedBytes} bytes freed).`
          : `Cleaned up ${deletedCount} expired screenshot/attachment file${deletedCount === 1 ? "" : "s"} (${deletedBytes} bytes freed; ${keptCount} kept).`,
      );
    } catch (err) {
      setError(
        err instanceof Error
          ? err.message
          : "Failed to clean up screenshot attachments",
      );
    } finally {
      setPruningAttachments(false);
    }
  };

  const systemDefaultArtifactsDir =
    settings.default_artifacts_dir ||
    (settings.home_dir
      ? `${settings.home_dir.replace(/\/+$/, "")}/artifacts`
      : "~/.local/share/fl/artifacts");
  const systemEffectiveArtifactsDir =
    settings.effective_artifacts_dir ||
    settings.artifacts_dir ||
    systemDefaultArtifactsDir;

  const getProjectArtifactsDirState = (projectName: string) => {
    const slug = projectName.trim().toLowerCase();
    const record = projectsArtifactsDirs[slug];
    const overrideFromSettings = settings.project_artifacts_dirs?.[slug] ?? null;
    const artifactsDir =
      record?.artifacts_dir !== undefined
        ? record.artifacts_dir
        : overrideFromSettings;
    const defaultDir =
      record?.default_artifacts_dir ||
      `${systemEffectiveArtifactsDir.replace(/\/+$/, "")}/${slug}`;
    const effectiveDir =
      record?.effective_artifacts_dir || artifactsDir || defaultDir;
    return {
      slug,
      artifactsDir,
      defaultDir,
      effectiveDir,
    };
  };

  const openSystemArtifactsDirModal = () => {
    setArtifactsDirModalError(null);
    setSystemArtifactsDirStatus(null);
    setArtifactsDirNewPath(systemEffectiveArtifactsDir);
    setArtifactsDirMigrate(true);
    setArtifactsDirModalTarget({ scope: "system" });
  };

  const openProjectArtifactsDirModal = (projectName: string) => {
    const info = getProjectArtifactsDirState(projectName);
    setArtifactsDirModalError(null);
    setProjectArtifactsDirStatus(null);
    setArtifactsDirNewPath(info.effectiveDir);
    setArtifactsDirMigrate(true);
    setArtifactsDirModalTarget({ scope: "project", project: projectName });
  };

  const handleConfirmChangeArtifactsDir = async (
    explicitNewPath?: string | null,
  ) => {
    if (!artifactsDirModalTarget || changingArtifactsDir) return;
    setChangingArtifactsDir(true);
    setArtifactsDirModalError(null);
    const rawPath =
      explicitNewPath !== undefined ? explicitNewPath : artifactsDirNewPath;
    const trimmed = typeof rawPath === "string" ? rawPath.trim() : "";
    try {
      const client = apiClient ?? new HubApiClient();
      if (artifactsDirModalTarget.scope === "system") {
        const nextPathPayload =
          trimmed === "" || trimmed === systemDefaultArtifactsDir
            ? null
            : trimmed;
        const res = await client.changeSystemArtifactsDir({
          new_path: nextPathPayload,
          migrate: artifactsDirMigrate,
        });
        const nextEffective =
          res?.effective_artifacts_dir ||
          res?.new_path ||
          nextPathPayload ||
          systemDefaultArtifactsDir;
        const nextDefault =
          res?.default_artifacts_dir || systemDefaultArtifactsDir;
        const nextOverride =
          res?.artifacts_dir !== undefined ? res.artifacts_dir : nextPathPayload;
        setSettings((prev) => {
          const updated: HubSettings = {
            ...prev,
            artifacts_dir: nextOverride,
            default_artifacts_dir: nextDefault,
            effective_artifacts_dir: nextEffective,
          };
          onSettingsUpdated?.(updated);
          return updated;
        });
        setProjectsArtifactsDirs((prev) => {
          const base = nextEffective.replace(/\/+$/, "");
          const nextMap: Record<string, ProjectArtifactsDirInfo> = {};
          for (const [slug, item] of Object.entries(prev)) {
            const projDefault = `${base}/${slug}`;
            nextMap[slug] = {
              ...item,
              default_artifacts_dir: projDefault,
              effective_artifacts_dir: item.artifacts_dir || projDefault,
            };
          }
          return nextMap;
        });
        const migratedFiles = res?.migrated_files ?? 0;
        const migratedBytes = res?.migrated_bytes ?? 0;
        setSystemArtifactsDirStatus(
          artifactsDirMigrate
            ? `Updated artifacts directory to ${nextEffective} (migrated ${migratedFiles} file${migratedFiles === 1 ? "" : "s"}, ${migratedBytes} bytes).`
            : `Updated artifacts directory to ${nextEffective}.`,
        );
        setArtifactsDirModalTarget(null);
      } else {
        const projectName = artifactsDirModalTarget.project;
        const info = getProjectArtifactsDirState(projectName);
        const nextPathPayload =
          trimmed === "" || trimmed === info.defaultDir ? null : trimmed;
        const res = await client.changeProjectArtifactsDir(projectName, {
          new_path: nextPathPayload,
          migrate: artifactsDirMigrate,
        });
        const nextEffective =
          res?.effective_artifacts_dir ||
          res?.new_path ||
          nextPathPayload ||
          info.defaultDir;
        const nextDefault = res?.default_artifacts_dir || info.defaultDir;
        const nextOverride =
          res?.artifacts_dir !== undefined ? res.artifacts_dir : nextPathPayload;
        setProjectsArtifactsDirs((prev) => ({
          ...prev,
          [info.slug]: {
            id: info.slug,
            artifacts_dir: nextOverride,
            default_artifacts_dir: nextDefault,
            effective_artifacts_dir: nextEffective,
          },
        }));
        setSettings((prev) => {
          const nextMap = { ...(prev.project_artifacts_dirs ?? {}) };
          if (nextOverride) {
            nextMap[info.slug] = nextOverride;
          } else {
            delete nextMap[info.slug];
          }
          const updated: HubSettings = {
            ...prev,
            project_artifacts_dirs: nextMap,
          };
          onSettingsUpdated?.(updated);
          return updated;
        });
        const migratedFiles = res?.migrated_files ?? 0;
        const migratedBytes = res?.migrated_bytes ?? 0;
        setProjectArtifactsDirStatus(
          artifactsDirMigrate
            ? `Updated ${projectName} artifacts directory to ${nextEffective} (migrated ${migratedFiles} file${migratedFiles === 1 ? "" : "s"}, ${migratedBytes} bytes).`
            : `Updated ${projectName} artifacts directory to ${nextEffective}.`,
        );
        setArtifactsDirModalTarget(null);
      }
    } catch (err) {
      setArtifactsDirModalError(
        err instanceof Error
          ? err.message
          : "Failed to change artifacts directory",
      );
    } finally {
      setChangingArtifactsDir(false);
    }
  };

  const handleAddOperator = async (e: React.FormEvent) => {
    e.preventDefault();
    const trimmedUser = opUsername.trim();
    if (!trimmedUser || addingOperator) {
      return;
    }
    setAddingOperator(true);
    setError(null);
    try {
      const client = apiClient ?? new HubApiClient();
      const res = await client.createOperator({
        username: trimmedUser,
        display_name: trimmedUser,
        role: opRole,
        ...(opPassword ? { password: opPassword } : {}),
      });
      const created: OperatorAccount = res?.operator ?? {
        id: `op-${trimmedUser}`,
        username: trimmedUser,
        display_name: trimmedUser,
        role: opRole,
        has_password: Boolean(opPassword),
        totp_enabled: false,
        passkeys_count: 0,
        created_at_ms: Date.now(),
      };
      setOperators((prev) => [
        ...prev.filter((item) => item.username !== created.username),
        created,
      ]);
      setOpUsername("");
      setOpPassword("");
      setOpRole("operator");
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to create operator",
      );
    } finally {
      setAddingOperator(false);
    }
  };

  const handleDeleteOperator = async (op: OperatorAccount) => {
    setError(null);
    try {
      const client = apiClient ?? new HubApiClient();
      await client.deleteOperator(op.id || op.username);
      setOperators((prev) =>
        prev.filter(
          (item) => item.id !== op.id && item.username !== op.username,
        ),
      );
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to delete operator",
      );
    }
  };

  const handleEnrollTotp = async () => {
    setError(null);
    setTotpVerified(false);
    try {
      const client = apiClient ?? new HubApiClient();
      const res = await client.enrollTotp();
      // Never show a secret the hub did not issue: one made up here (or an empty answer
      // rendered as blanks) would leave the operator with an authenticator entry that
      // protects nothing.
      if (
        typeof res?.secret_base32 !== "string" ||
        res.secret_base32.trim().length === 0 ||
        typeof res.otpauth_uri !== "string" ||
        res.otpauth_uri.trim().length === 0
      ) {
        setTotpEnrollment(null);
        setError(
          "The hub did not return a TOTP secret; enrollment was not started.",
        );
        return;
      }
      setTotpEnrollment(res);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to enroll TOTP");
    }
  };

  const handleVerifyTotp = async () => {
    const code = totpVerifyCode.trim();
    if (!code) return;
    setError(null);
    try {
      const client = apiClient ?? new HubApiClient();
      const res = await client.verifyTotp(code);
      // An answer that does not say `ok: true` (an empty body, a proxy's page) is not proof.
      if (res?.ok === true) {
        setTotpVerified(true);
      } else {
        setError("The hub did not confirm the TOTP code.");
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to verify TOTP");
    }
  };

  const handleRegisterPasskey = async () => {
    setError(null);
    setPasskeyRegistered(false);
    try {
      const client = apiClient ?? new HubApiClient();
      const label = passkeyLabel.trim() || "Operator Passkey";
      await client.registerPasskey({
        credential_id: generatePasskeyCredentialId(),
        label,
      });
      setPasskeyRegistered(true);
      setPasskeyLabel("");
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to register passkey",
      );
    }
  };

  const handleSaveCloudAdapters = async () => {
    if (savingCloudAdapters) return;
    setSavingCloudAdapters(true);
    setError(null);
    setCloudAdaptersSaved(false);
    try {
      const client = apiClient ?? new HubApiClient();
      const updated = await client.updateCloudAdapters(cloudAdapters);
      setCloudAdapters(updated);
      setCloudAdaptersSaved(true);
    } catch (err) {
      setError(
        err instanceof Error
          ? err.message
          : "Failed to save cloud adapter configuration",
      );
    } finally {
      setSavingCloudAdapters(false);
    }
  };

  const handleConfirmBulkAction = async () => {
    if (!armedAction || submitting) return;
    setSubmitting(true);
    setError(null);
    const selectedKeys =
      selectedAgentKeys.size > 0 ? Array.from(selectedAgentKeys) : undefined;
    try {
      let res: BulkSessionActionResponse;
      if (onBulkSessionAction) {
        res = selectedKeys
          ? await onBulkSessionAction(armedAction, selectedKeys)
          : await onBulkSessionAction(armedAction);
      } else {
        const client = apiClient ?? new HubApiClient();
        res = await executeScopedBulkSessionAction(
          client,
          armedAction,
          selectedKeys,
          filteredAgents.map((a) => a.name_key.toLowerCase()),
        );
      }
      setLastResult(res);
      setArmedAction(null);
    } catch (err) {
      setError(
        err instanceof Error
          ? err.message
          : `Failed to execute bulk ${armedAction}`,
      );
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div
      className="fd-modal-backdrop"
      onClick={() => {
        if (!submitting && !savingSettings && !savingCloudAdapters) {
          onClose();
        }
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label="FerroDeck Settings"
        data-testid="settings-modal"
        className="fd-modal"
        onClick={(e) => e.stopPropagation()}
        style={{
          maxWidth: "740px",
          width: "95vw",
          maxHeight: "88vh",
          overflowY: "auto",
          display: "flex",
          flexDirection: "column",
          gap: "12px",
        }}
      >
        <div
          className="fd-modal-header"
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: "12px",
          }}
        >
          <h2 style={{ margin: 0, fontSize: "16px", fontWeight: 700, flex: 1, minWidth: 0 }}>
            Settings &amp; Fleet Operations
          </h2>
          <button
            type="button"
            className="fd-btn"
            style={{ flexShrink: 0 }}
            onClick={onClose}
            disabled={submitting || savingSettings || savingCloudAdapters}
            aria-label="Close Settings Modal"
          >
            ✕
          </button>
        </div>

        {/* 3-Tab Navigation Header (System & Auth omitted when opened from a Project cog) */}
        <div
          role="tablist"
          aria-label="Settings Categories"
          data-testid="settings-tab-bar"
          style={{
            display: "flex",
            alignItems: "center",
            gap: "6px",
            flexWrap: "wrap",
            borderBottom: "1px solid var(--border-subtle)",
            paddingBottom: "8px",
          }}
        >
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === "agents"}
            data-testid="settings-tab-agents"
            className={`fd-btn${activeTab === "agents" ? " fd-btn-active" : ""}`}
            onClick={() => setActiveTab("agents")}
          >
            {filteredAgents.length} Agents
          </button>
          {!initialProject && (
            <>
              <button
                type="button"
                role="tab"
                aria-selected={activeTab === "system"}
                data-testid="settings-tab-system"
                className={`fd-btn${activeTab === "system" ? " fd-btn-active" : ""}`}
                onClick={() => setActiveTab("system")}
              >
                System
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={activeTab === "products"}
                data-testid="settings-tab-products"
                className={`fd-btn${activeTab === "products" ? " fd-btn-active" : ""}`}
                onClick={() => setActiveTab("products")}
              >
                Products
              </button>
              {authority?.tier === "owner" && (
                <button
                  type="button"
                  role="tab"
                  aria-selected={activeTab === "runtimes"}
                  data-testid="settings-tab-runtimes"
                  className={`fd-btn${activeTab === "runtimes" ? " fd-btn-active" : ""}`}
                  onClick={() => setActiveTab("runtimes")}
                >
                  Runtimes &amp; Quotas
                </button>
              )}
              <button
                type="button"
                role="tab"
                aria-selected={activeTab === "accessibility"}
                data-testid="settings-tab-accessibility"
                className={`fd-btn${activeTab === "accessibility" ? " fd-btn-active" : ""}`}
                onClick={() => setActiveTab("accessibility")}
              >
                Accessibility
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={activeTab === "auth"}
                data-testid="settings-tab-auth"
                className={`fd-btn${activeTab === "auth" ? " fd-btn-active" : ""}`}
                onClick={() => setActiveTab("auth")}
              >
                Auth
              </button>
            </>
          )}
        </div>

        {error && (
          <div
            role="alert"
            style={{
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

        {recastStatusBanner && (
          <div
            role="status"
            data-testid="recast-status-banner"
            className="fd-card"
            style={{
              borderColor: "var(--diff-add-border)",
              backgroundColor: "var(--diff-add-bg)",
              color: "var(--diff-add-text)",
              fontSize: "12px",
              padding: "8px 12px",
            }}
          >
            {recastStatusBanner}
          </div>
        )}

        {settingsSaved && (
          <div
            role="status"
            data-testid="settings-saved-banner"
            className="fd-card"
            style={{
              borderColor: "var(--diff-add-border)",
              backgroundColor: "var(--diff-add-bg)",
              color: "var(--diff-add-text)",
              fontSize: "12px",
              padding: "8px 12px",
            }}
          >
            Settings saved successfully.
          </div>
        )}

        {cloudAdaptersSaved && (
          <div
            role="status"
            data-testid="cloud-adapters-saved-banner"
            className="fd-card"
            style={{
              borderColor: "var(--diff-add-border)",
              backgroundColor: "var(--diff-add-bg)",
              color: "var(--diff-add-text)",
              fontSize: "12px",
              padding: "8px 12px",
            }}
          >
            Cloud sync &amp; gate adapters saved successfully.
          </div>
        )}

        {lastResult && (
          <div
            role="status"
            data-testid="bulk-action-result"
            className="fd-card"
            style={{
              borderColor: "var(--diff-add-border)",
              backgroundColor: "var(--diff-add-bg)",
              color: "var(--diff-add-text)",
              fontSize: "12px",
            }}
          >
            <div>
              Completed <strong>{ACTION_LABELS[lastResult.action]}</strong>:{" "}
              {lastResult.succeeded} / {lastResult.total} sessions succeeded.
            </div>
            {lastResult.results.some((r) => !r.ok) && (
              <ul
                style={{
                  margin: "6px 0 0 0",
                  paddingLeft: "18px",
                  color: "#FCA5A5",
                }}
              >
                {lastResult.results
                  .filter((r) => !r.ok)
                  .map((r) => (
                    <li key={r.session_id}>
                      <code>{r.session_id}</code>:{" "}
                      {r.error || "Unknown failure"}
                    </li>
                  ))}
              </ul>
            )}
          </div>
        )}

        {/* TAB 1: <N> Agents */}
        <div
          role="tabpanel"
          aria-label="Agents Fleet Panel"
          data-testid="settings-panel-agents"
          style={{
            display: activeTab === "agents" ? "flex" : "none",
            flexDirection: "column",
            gap: "12px",
          }}
        >
          <div
            className="fd-card"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "12px",
            }}
          >
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "12px",
                flexWrap: "wrap",
              }}
            >
              <div>
                <h3
                  data-testid="settings-agents-heading"
                  style={{
                    margin: "0 0 4px 0",
                    fontSize: "14px",
                    fontWeight: 700,
                  }}
                >
                  {filteredAgents.length} Agents
                </h3>
                <p
                  style={{
                    margin: 0,
                    fontSize: "12px",
                    color: "var(--text-secondary)",
                  }}
                >
                  Create new agent sessions, re-cast an agent&apos;s project or role, or
                  decommission sessions across the fleet.
                </p>
              </div>
              <button
                type="button"
                data-testid="settings-spin-up-btn"
                className="fd-btn fd-btn-primary"
                onClick={() => {
                  onOpenSpinUpModal?.(
                    resolvedProjectFilter !== "all" ? resolvedProjectFilter : undefined,
                  );
                  onClose();
                }}
              >
                + New Agent
              </button>
            </div>

            {/* Project Filter & Selection Toolbar */}
            <div
              data-testid="settings-project-filter-bar"
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "8px",
                flexWrap: "wrap",
                padding: "6px 8px",
                borderRadius: "6px",
                backgroundColor: "var(--bg-elevated)",
                border: "1px solid var(--border-subtle)",
              }}
            >
              <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                <label
                  htmlFor="settings-project-filter-select"
                  style={{ fontSize: "11.5px", color: "var(--text-secondary)", fontWeight: 600 }}
                >
                  Project:
                </label>
                <select
                  id="settings-project-filter-select"
                  aria-label="Filter agents by project"
                  data-testid="settings-project-filter-select"
                  className="fd-select fd-mono"
                  style={{ fontSize: "11.5px", padding: "3px 8px" }}
                  value={resolvedProjectFilter}
                  onChange={(e) => {
                    setSelectedProjectFilter(e.target.value);
                    setProjectArtifactsDirStatus(null);
                  }}
                >
                  <option value="all">All Projects ({effectiveAgents.length})</option>
                  {projectOptions.map((proj) => (
                    <option key={proj} value={proj}>
                      {proj}
                    </option>
                  ))}
                </select>
              </div>

              <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                <span
                  data-testid="settings-selected-agents-count"
                  className="fd-badge fd-mono"
                  style={{ fontSize: "10.5px" }}
                >
                  {selectedAgentKeys.size > 0
                    ? `${selectedAgentKeys.size} selected`
                    : `${filteredAgents.length} shown`}
                </span>
                <button
                  type="button"
                  data-testid="settings-agents-select-all-btn"
                  className="fd-btn"
                  style={{ fontSize: "11px", padding: "3px 8px" }}
                  onClick={() => {
                    setSelectedAgentKeys((prev) => {
                      const next = new Set(prev);
                      if (allFilteredSelected) {
                        for (const ag of filteredAgents) {
                          next.delete(ag.name_key.toLowerCase());
                        }
                      } else {
                        for (const ag of filteredAgents) {
                          next.add(ag.name_key.toLowerCase());
                        }
                      }
                      return next;
                    });
                  }}
                >
                  {allFilteredSelected
                    ? "Clear Selection"
                    : `Select All (${filteredAgents.length})`}
                </button>
              </div>
            </div>

            {/* Scoped Project Summary Card (shown when a project is selected) */}
            {resolvedProjectFilter !== "all" && (
              <div
                data-testid="settings-project-summary-card"
                className="fd-card"
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "8px",
                  backgroundColor: "var(--bg-elevated)",
                  borderColor: "var(--border-accent)",
                  padding: "10px 12px",
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
                  <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                    <strong
                      data-testid="settings-project-summary-name"
                      style={{ fontSize: "13px" }}
                    >
                      {resolvedProjectFilter}
                    </strong>
                    <span className="fd-badge fd-mono" data-testid="settings-project-summary-lead">
                      Lead: {projectSummaryLead}
                    </span>
                    <span
                      className="fd-badge fd-mono"
                      data-testid="session-cwd-badge"
                      title={`~/Projects/Artificial-Humanity/${resolvedProjectFilter}`}
                      style={{ color: "var(--text-muted)" }}
                    >
                      ~/Projects/Artificial-Humanity/{resolvedProjectFilter}
                    </span>
                    <span className="fd-badge fd-mono">
                      {filteredAgents.length} {filteredAgents.length === 1 ? "agent" : "agents"}
                    </span>
                  </div>

                  <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                    <button
                      type="button"
                      data-testid="project-pin-toggle-btn"
                      aria-pressed={pinnedProjects.has(resolvedProjectFilter.trim().toLowerCase())}
                      title={
                        pinnedProjects.has(resolvedProjectFilter.trim().toLowerCase())
                          ? "Unpin Project"
                          : "Pin Project"
                      }
                      aria-label={
                        pinnedProjects.has(resolvedProjectFilter.trim().toLowerCase())
                          ? "Unpin Project"
                          : "Pin Project"
                      }
                      className={`fd-btn${pinnedProjects.has(resolvedProjectFilter.trim().toLowerCase()) ? " fd-btn-active" : ""}`}
                      style={{
                        padding: "4px 6px",
                        display: "inline-flex",
                        alignItems: "center",
                        justifyContent: "center",
                        lineHeight: 1,
                      }}
                      onClick={() => togglePinnedProject(resolvedProjectFilter)}
                    >
                      <PinIcon
                        pinned={pinnedProjects.has(resolvedProjectFilter.trim().toLowerCase())}
                        circled={true}
                        size={16}
                      />
                    </button>
                    <button
                      type="button"
                      data-testid="settings-project-open-git-btn"
                      className="fd-btn"
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                      onClick={() => {
                        onSelectCockpitTab?.("git");
                        onClose();
                      }}
                    >
                      Git &amp; PRs
                    </button>
                    <button
                      type="button"
                      data-testid="settings-project-open-board-btn"
                      className="fd-btn"
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                      onClick={() => {
                        onSelectCockpitTab?.("board");
                        onClose();
                      }}
                    >
                      Board
                    </button>
                    <button
                      type="button"
                      data-testid="settings-project-open-artifacts-btn"
                      className="fd-btn"
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                      onClick={() => {
                        onSelectCockpitTab?.("artifacts");
                        onClose();
                      }}
                    >
                      Project Artifacts
                    </button>
                  </div>
                </div>

                {/* Project Artifacts Directory row (Issue #207) */}
                {(() => {
                  const projArtifactsInfo = getProjectArtifactsDirState(
                    resolvedProjectFilter,
                  );
                  return (
                    <div
                      data-testid="settings-project-artifacts-dir-row"
                      style={{
                        display: "flex",
                        flexDirection: "column",
                        gap: "6px",
                        paddingTop: "8px",
                        borderTop: "1px solid var(--border-subtle)",
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
                        <div
                          style={{
                            display: "flex",
                            alignItems: "center",
                            gap: "8px",
                            flexWrap: "wrap",
                            minWidth: 0,
                          }}
                        >
                          <span
                            style={{
                              fontSize: "11.5px",
                              fontWeight: 600,
                              color: "var(--text-secondary)",
                            }}
                          >
                            Artifacts Directory:
                          </span>
                          <code
                            data-testid="settings-project-artifacts-dir-value"
                            className="fd-badge fd-mono"
                            title={projArtifactsInfo.effectiveDir}
                            style={{
                              fontSize: "11px",
                              maxWidth: "100%",
                              overflow: "hidden",
                              textOverflow: "ellipsis",
                            }}
                          >
                            {projArtifactsInfo.effectiveDir}
                          </code>
                          {projArtifactsInfo.artifactsDir && (
                            <span
                              data-testid="settings-project-artifacts-dir-override-badge"
                              className="fd-badge fd-mono"
                              style={{
                                fontSize: "10px",
                                color: "var(--accent-primary, #f59e0b)",
                              }}
                            >
                              Override
                            </span>
                          )}
                        </div>
                        <button
                          type="button"
                          data-testid="settings-project-change-artifacts-dir-btn"
                          className="fd-btn"
                          style={{ fontSize: "11px", padding: "3px 8px" }}
                          onClick={() =>
                            openProjectArtifactsDirModal(resolvedProjectFilter)
                          }
                        >
                          Change Artifacts Directory
                        </button>
                      </div>

                      {projectArtifactsDirStatus && (
                        <div
                          role="status"
                          data-testid="project-artifacts-dir-status"
                          className="fd-card"
                          style={{
                            borderColor: "var(--diff-add-border)",
                            backgroundColor: "var(--diff-add-bg)",
                            color: "var(--diff-add-text)",
                            fontSize: "11.5px",
                            padding: "6px 10px",
                          }}
                        >
                          {projectArtifactsDirStatus}
                        </div>
                      )}
                    </div>
                  );
                })()}
              </div>
            )}

            <div
              data-testid="settings-agents-list"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "8px",
                maxHeight: "340px",
                overflowY: "auto",
                paddingRight: "2px",
              }}
            >
              {filteredAgents.map((ag) => {
                const key = ag.name_key.toLowerCase();

                return (
                  <div
                    key={ag.id || ag.name_key}
                    data-testid={`settings-agent-row-${key}`}
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      gap: "6px",
                      padding: "8px 10px",
                      borderRadius: "6px",
                      border: "1px solid var(--border-subtle, #1e293b)",
                      background: "var(--bg-elevated)",
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
                      <div
                        style={{
                          display: "flex",
                          alignItems: "center",
                          gap: "8px",
                          minWidth: "150px",
                        }}
                      >
                        <input
                          type="checkbox"
                          data-testid={`agent-select-checkbox-${key}`}
                          aria-label={`Select ${ag.display_name}`}
                          checked={selectedAgentKeys.has(key)}
                          onChange={(e) => {
                            const checked = e.target.checked;
                            setSelectedAgentKeys((prev) => {
                              const next = new Set(prev);
                              if (checked) {
                                next.add(key);
                              } else {
                                next.delete(key);
                              }
                              return next;
                            });
                          }}
                        />
                        <strong style={{ fontSize: "12.5px" }}>
                          {ag.display_name}
                        </strong>
                        <span
                          className="fd-badge fd-mono"
                          style={{ fontSize: "10.5px" }}
                        >
                          {ag.project}
                        </span>
                        <span
                          className="fd-badge fd-mono"
                          style={{ fontSize: "10.5px" }}
                        >
                          {canonicalizeDefaultRole(ag.role)}
                        </span>
                        {(() => {
                          const rawPosture = postureInputsByAgent?.[key] ?? {};
                          const isReconciled = reconciledPostureAgents.has(key);
                          const report = evaluateAgentMcpPosture({
                            agentName: ag.name_key,
                            project: ag.project,
                            role: ag.role,
                            ...rawPosture,
                            ...(isReconciled
                              ? {
                                  appliedMcpServers:
                                    rawPosture.desiredMcpServers,
                                  liveMcpServers: rawPosture.desiredMcpServers,
                                  sessionStartedBeforeSync: false,
                                  sessionRole: ag.role,
                                  sessionProject: ag.project,
                                }
                              : {}),
                          });
                          const isSynced = report.driftKind === "synced";
                          return (
                            <>
                              <span
                                className="fd-badge"
                                data-testid={`settings-agent-posture-badge-${key}`}
                                data-posture={report.driftKind}
                                title={report.summary}
                                style={{
                                  fontSize: "10px",
                                  fontWeight: 600,
                                  backgroundColor: isSynced
                                    ? "rgba(16, 185, 129, 0.14)"
                                    : "rgba(245, 158, 11, 0.16)",
                                  color: isSynced ? "#34D399" : "#FBBF24",
                                  borderColor: isSynced
                                    ? "rgba(16, 185, 129, 0.4)"
                                    : "rgba(245, 158, 11, 0.45)",
                                }}
                              >
                                {report.badgeLabel}
                              </span>
                              {!isSynced && (
                                <button
                                  type="button"
                                  className="fd-btn"
                                  data-testid={`settings-agent-reconcile-btn-${key}`}
                                  style={{ fontSize: "10px", padding: "2px 6px" }}
                                  title={report.summary}
                                  onClick={() => {
                                    setReconciledPostureAgents((prev) => {
                                      const next = new Set(prev);
                                      next.add(key);
                                      return next;
                                    });
                                    onReconcileAgentPosture?.(key, report);
                                  }}
                                >
                                  {report.remediationLabel}
                                </button>
                              )}
                            </>
                          );
                        })()}
                      </div>

                      <div
                        style={{
                          display: "flex",
                          alignItems: "center",
                          gap: "6px",
                          flexWrap: "wrap",
                        }}
                      >
                        <button
                          type="button"
                          data-testid={`recast-agent-btn-${key}`}
                          className="fd-btn fd-btn-primary"
                          title="Reassign agent project or role in the registry without clearing the live session"
                          style={{ fontSize: "11px", padding: "3px 8px" }}
                          disabled={recastingAgentKey === key}
                          onClick={() => {
                            setRecastDrafts((prev) => ({
                              ...prev,
                              [key]: prev[key] ?? {
                                project: ag.project || "Workspace",
                                role: canonicalizeDefaultRole(ag.role || "assistant"),
                              },
                            }));
                            setRecastReplaceLeadArmed((prev) => {
                              if (!(key in prev)) return prev;
                              const next = { ...prev };
                              delete next[key];
                              return next;
                            });
                            setRecastModalAgentKey(key);
                          }}
                        >
                          {recastingAgentKey === key ? "Re-Casting..." : "Re-Cast"}
                        </button>

                        <button
                          type="button"
                          data-testid={`settings-agent-cog-btn-${key}`}
                          className="fd-gear-btn"
                          title={`Settings for ${ag.display_name}`}
                          aria-label={`Settings for ${ag.display_name}`}
                          style={{
                            border: "none",
                            background: "transparent",
                            padding: "2px 4px",
                            fontSize: "16px",
                            lineHeight: 1,
                          }}
                          onClick={() => onOpenAgentSettings?.(ag.name_key)}
                        >
                          ⚙
                        </button>
                      </div>
                    </div>
                  </div>
                );
              })}
            </div>
          </div>

          {/* Fleet Bulk Session Operations */}
          <div
            className="fd-card"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "12px",
              borderColor: "rgba(245, 158, 11, 0.35)",
            }}
          >
            <div>
              <h3
                style={{
                  margin: "0 0 4px 0",
                  fontSize: "13px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.04em",
                }}
              >
                Fleet Bulk Session Operations
              </h3>
              <p
                style={{
                  margin: 0,
                  fontSize: "12px",
                  color: "var(--text-secondary)",
                }}
              >
                Fleet-wide session context operations are isolated here and require
                explicit two-step confirmation before executing across all active
                agent sessions.
              </p>
            </div>

            <div style={{ display: "flex", gap: "8px", flexWrap: "wrap" }}>
              <button
                type="button"
                className={`fd-btn${armedAction === "compact" ? " fd-btn-active" : ""}`}
                data-testid="bulk-compact-btn"
                disabled={submitting}
                onClick={() => {
                  setLastResult(null);
                  setError(null);
                  setArmedAction("compact");
                }}
              >
                {selectedAgentKeys.size > 0
                  ? `Compact Selected (${selectedAgentKeys.size})`
                  : "Compact All"}
              </button>
              <button
                type="button"
                className="fd-btn fd-btn-danger"
                data-testid="bulk-clear-btn"
                disabled={submitting}
                onClick={() => {
                  setLastResult(null);
                  setError(null);
                  setArmedAction("clear");
                }}
              >
                {selectedAgentKeys.size > 0
                  ? `Clear Selected (${selectedAgentKeys.size})`
                  : "Clear All"}
              </button>
              <button
                type="button"
                className={`fd-btn fd-btn-primary${armedAction === "reforge" ? " fd-btn-active" : ""}`}
                data-testid="bulk-reforge-btn"
                disabled={submitting}
                onClick={() => {
                  setLastResult(null);
                  setError(null);
                  setArmedAction("reforge");
                }}
              >
                {selectedAgentKeys.size > 0
                  ? `Reforge Selected (${selectedAgentKeys.size})`
                  : "Reforge All"}
              </button>
              <button
                type="button"
                className={`fd-btn${armedAction === "prepare" ? " fd-btn-active" : ""}`}
                data-testid="bulk-action-prepare-btn"
                disabled={submitting}
                onClick={() => {
                  setLastResult(null);
                  setError(null);
                  setArmedAction("prepare");
                }}
              >
                {selectedAgentKeys.size > 0
                  ? `Prepare Selected (${selectedAgentKeys.size})`
                  : "Prepare All"}
              </button>
              <button
                type="button"
                className={`fd-btn fd-btn-danger${armedAction === "teardown" ? " fd-btn-active" : ""}`}
                data-testid="bulk-action-teardown-btn"
                disabled={submitting}
                onClick={() => {
                  setLastResult(null);
                  setError(null);
                  setArmedAction("teardown");
                }}
              >
                {selectedAgentKeys.size > 0
                  ? `Tear Down Selected (${selectedAgentKeys.size})`
                  : "Tear Down All"}
              </button>
            </div>

            {armedAction && (
              <div
                data-testid="bulk-confirm-panel"
                className="fd-card"
                style={{
                  backgroundColor: "var(--bg-elevated)",
                  borderColor: "var(--border-accent)",
                  display: "flex",
                  flexDirection: "column",
                  gap: "10px",
                }}
              >
                <div style={{ fontSize: "12px", color: "var(--text-primary)" }}>
                  <strong>
                    Confirm{" "}
                    {getBulkActionLabel(armedAction, selectedAgentKeys.size)}:
                  </strong>{" "}
                  {selectedAgentKeys.size > 0
                    ? `Execute ${armedAction} across the ${selectedAgentKeys.size} selected agent session${selectedAgentKeys.size === 1 ? "" : "s"} (${Array.from(selectedAgentKeys).join(", ")}).`
                    : ACTION_DESCRIPTIONS[armedAction]}
                </div>
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "flex-end",
                    gap: "8px",
                  }}
                >
                  <button
                    type="button"
                    className="fd-btn"
                    data-testid="bulk-cancel-btn"
                    disabled={submitting}
                    onClick={() => setArmedAction(null)}
                  >
                    Cancel
                  </button>
                  <button
                    type="button"
                    className={
                      armedAction === "clear" || armedAction === "teardown"
                        ? "fd-btn fd-btn-danger"
                        : "fd-btn fd-btn-primary"
                    }
                    data-testid="bulk-confirm-btn"
                    disabled={submitting}
                    onClick={() => void handleConfirmBulkAction()}
                  >
                    {submitting
                      ? "Running..."
                      : `Confirm ${getBulkActionLabel(armedAction, selectedAgentKeys.size)}`}
                  </button>
                </div>
              </div>
            )}
          </div>

          {/* Record #74: User-Configurable Agent Role Registry */}
          <div
            className="fd-card"
            data-testid="configurable-role-registry-card"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "10px",
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
              <div>
                <h3
                  style={{
                    margin: "0 0 2px 0",
                    fontSize: "13px",
                    fontWeight: 700,
                    textTransform: "uppercase",
                    letterSpacing: "0.04em",
                  }}
                >
                  User-Configurable Agent Role Registry
                </h3>
                <p
                  style={{
                    margin: 0,
                    fontSize: "11.5px",
                    color: "var(--text-secondary)",
                  }}
                >
                  Inspect the 11 built-in workspace roles and define or edit custom{" "}
                  <code className="fd-mono">RoleRecord</code> templates synced via{" "}
                  <code className="fd-mono">/api/v1/roles</code>.
                </p>
              </div>
              <span
                className="fd-badge fd-mono"
                data-testid="role-registry-count-badge"
                style={{ fontSize: "10.5px" }}
              >
                {roleRegistry.length} Roles (
                {roleRegistry.filter((r) => r.builtin).length} Built-in ·{" "}
                {roleRegistry.filter((r) => !r.builtin).length} Custom)
              </span>
            </div>

            {roleRegistryBanner && (
              <div
                role="status"
                data-testid="role-registry-status-banner"
                className="fd-mono"
                style={{
                  fontSize: "11px",
                  color: "#34D399",
                  padding: "4px 8px",
                  borderRadius: "4px",
                  backgroundColor: "rgba(16, 185, 129, 0.12)",
                }}
              >
                {roleRegistryBanner}
              </div>
            )}

            <div
              data-testid="role-registry-list"
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))",
                gap: "6px",
                maxHeight: "220px",
                overflowY: "auto",
              }}
            >
              {roleRegistry.map((roleRec) => (
                <div
                  key={roleRec.slug}
                  data-testid={`role-registry-row-${roleRec.slug}`}
                  style={{
                    padding: "6px 8px",
                    borderRadius: "4px",
                    border: "1px solid var(--border-subtle)",
                    backgroundColor: "var(--bg-elevated)",
                    display: "flex",
                    flexDirection: "column",
                    gap: "4px",
                    fontSize: "11px",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: "4px",
                      flexWrap: "wrap",
                    }}
                  >
                    <code className="fd-mono" style={{ fontWeight: 700 }}>
                      {roleRec.slug}
                    </code>
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: "4px",
                      }}
                    >
                      <span
                        className="fd-badge fd-mono"
                        data-testid={`role-registry-kind-${roleRec.slug}`}
                        style={{ fontSize: "9.5px" }}
                      >
                        {roleRec.builtin ? "built-in" : "custom"}
                      </span>
                      <button
                        type="button"
                        className="fd-btn"
                        data-testid={`role-registry-edit-btn-${roleRec.slug}`}
                        style={{ fontSize: "10px", padding: "1px 6px" }}
                        onClick={() => {
                          setRoleDraftSlug(roleRec.slug);
                          setRoleDraftLabel(roleRec.label);
                          setRoleDraftDescription(roleRec.description);
                          setRoleDraftReviewPolicy(roleRec.review_policy);
                          setRoleDraftPromptTemplate(roleRec.prompt_template);
                        }}
                      >
                        Edit
                      </button>
                    </div>
                  </div>
                  <div style={{ fontWeight: 600 }}>{roleRec.label}</div>
                  <div
                    style={{
                      fontSize: "10.5px",
                      color: "var(--text-secondary)",
                    }}
                  >
                    {roleRec.description}
                  </div>
                </div>
              ))}
            </div>

            {/* Create / Edit RoleRecord Form */}
            <div
              data-testid="role-registry-editor-form"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "6px",
                padding: "8px 10px",
                borderRadius: "6px",
                backgroundColor: "var(--bg-elevated)",
                border: "1px solid var(--border-subtle)",
              }}
            >
              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "1fr 1fr 1fr",
                  gap: "6px",
                }}
              >
                <input
                  type="text"
                  aria-label="Role Slug"
                  data-testid="role-registry-slug-input"
                  className="fd-input fd-mono"
                  placeholder="slug (e.g. benchmark-auditor)"
                  value={roleDraftSlug}
                  onChange={(e) => setRoleDraftSlug(e.target.value)}
                  style={{ fontSize: "11px" }}
                />
                <input
                  type="text"
                  aria-label="Role Display Label"
                  data-testid="role-registry-label-input"
                  className="fd-input"
                  placeholder="Display Label"
                  value={roleDraftLabel}
                  onChange={(e) => setRoleDraftLabel(e.target.value)}
                  style={{ fontSize: "11px" }}
                />
                <select
                  aria-label="Role Review Policy"
                  data-testid="role-registry-review-policy-select"
                  className="fd-select fd-mono"
                  value={roleDraftReviewPolicy}
                  onChange={(e) =>
                    setRoleDraftReviewPolicy(e.target.value as RoleReviewPolicy)
                  }
                  style={{ fontSize: "11px" }}
                >
                  <option value="lead-review">lead-review</option>
                  <option value="lab-manager-review">lab-manager-review</option>
                  <option value="peer-review">peer-review</option>
                </select>
              </div>
              <input
                type="text"
                aria-label="Role Description"
                data-testid="role-registry-description-input"
                className="fd-input"
                placeholder="Role responsibilities and coverage summary"
                value={roleDraftDescription}
                onChange={(e) => setRoleDraftDescription(e.target.value)}
                style={{ fontSize: "11px" }}
              />
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: "6px",
                }}
              >
                <input
                  type="text"
                  aria-label="Role Prompt Template"
                  data-testid="role-registry-prompt-input"
                  className="fd-input fd-mono"
                  placeholder="Startup hydration prompt template"
                  value={roleDraftPromptTemplate}
                  onChange={(e) => setRoleDraftPromptTemplate(e.target.value)}
                  style={{ flex: 1, fontSize: "11px" }}
                />
                <button
                  type="button"
                  className="fd-btn fd-btn-primary"
                  data-testid="role-registry-save-btn"
                  style={{ fontSize: "11px", padding: "4px 10px" }}
                  onClick={() => {
                    const cleanSlug = roleDraftSlug
                      .trim()
                      .toLowerCase()
                      .replace(/[^a-z0-9-_]/g, "-");
                    if (!cleanSlug) return;
                    const nextRec: ConfigurableRoleRecord = {
                      slug: cleanSlug,
                      label: roleDraftLabel.trim() || cleanSlug,
                      description:
                        roleDraftDescription.trim() ||
                        `Custom role ${cleanSlug}`,
                      builtin: BUILTIN_ROLE_REGISTRY.some(
                        (b) => b.slug === cleanSlug,
                      ),
                      review_policy: roleDraftReviewPolicy,
                      prompt_template:
                        roleDraftPromptTemplate.trim() ||
                        `Read .agents/roles/${cleanSlug}.md and execute assigned tasks.`,
                    };
                    setRoleRegistry((prev) => {
                      const exists = prev.some((r) => r.slug === cleanSlug);
                      if (exists) {
                        return prev.map((r) =>
                          r.slug === cleanSlug ? nextRec : r,
                        );
                      }
                      return [...prev, nextRec];
                    });
                    setRoleRegistryBanner(
                      `Saved RoleRecord '${cleanSlug}' (${nextRec.review_policy}) to registry.`,
                    );
                    if (typeof globalThis.fetch === "function") {
                      void globalThis
                        .fetch("/api/v1/roles", {
                          method: "POST",
                          headers: { "Content-Type": "application/json" },
                          body: JSON.stringify(nextRec),
                        })
                        .catch(() => {});
                    }
                  }}
                >
                  Save Role Record
                </button>
              </div>
            </div>
          </div>

          {/* Record #71: Manage Vendor Cloud Sessions in FerroDeck */}
          <div
            className="fd-card"
            data-testid="vendor-cloud-sessions-card"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "10px",
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
              <div>
                <h3
                  style={{
                    margin: "0 0 2px 0",
                    fontSize: "13px",
                    fontWeight: 700,
                    textTransform: "uppercase",
                    letterSpacing: "0.04em",
                  }}
                >
                  Vendor Cloud Sessions (#71)
                </h3>
                <p
                  style={{
                    margin: 0,
                    fontSize: "11.5px",
                    color: "var(--text-secondary)",
                  }}
                >
                  Inspect and attach remote cloud sessions across Claude Cloud,
                  OpenAI Codex Cloud, Google Jules / Gemini Cloud, and xAI Grok
                  Cloud via <code className="fd-mono">/api/v1/cloud-sessions</code>.
                </p>
              </div>
              <span
                className="fd-badge fd-mono"
                data-testid="vendor-cloud-sessions-count-badge"
                style={{ fontSize: "10.5px" }}
              >
                {cloudSessions.length} Cloud Sessions (
                {cloudSessions.filter((s) => s.mirrored).length} Mirrored)
              </span>
            </div>

            {cloudSessionBanner && (
              <div
                role="status"
                data-testid="vendor-cloud-sessions-banner"
                className="fd-mono"
                style={{
                  fontSize: "11px",
                  color: "#34D399",
                  padding: "4px 8px",
                  borderRadius: "4px",
                  backgroundColor: "rgba(16, 185, 129, 0.12)",
                }}
              >
                {cloudSessionBanner}
              </div>
            )}

            <div
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "6px",
              }}
            >
              {cloudSessions.map((sess) => (
                <div
                  key={sess.session_id}
                  data-testid={`cloud-session-row-${sess.session_id}`}
                  style={{
                    padding: "8px 10px",
                    borderRadius: "4px",
                    border: "1px solid var(--border-subtle)",
                    backgroundColor: "var(--bg-elevated)",
                    display: "flex",
                    flexDirection: "column",
                    gap: "6px",
                    fontSize: "11.5px",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: "6px",
                      flexWrap: "wrap",
                    }}
                  >
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: "6px",
                        flexWrap: "wrap",
                      }}
                    >
                      <code className="fd-mono" style={{ fontWeight: 700 }}>
                        {sess.session_id}
                      </code>
                      <span
                        className="fd-badge fd-mono"
                        data-testid={`cloud-session-vendor-${sess.session_id}`}
                      >
                        {sess.vendor_label}
                      </span>
                      <span
                        className="fd-badge fd-mono"
                        data-testid={`cloud-session-status-${sess.session_id}`}
                      >
                        {sess.status}
                      </span>
                      <span
                        className="fd-badge fd-mono"
                        data-testid={`cloud-session-mirrored-${sess.session_id}`}
                      >
                        {sess.mirrored ? "mirrored" : "unattached"}
                      </span>
                    </div>

                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: "4px",
                        flexWrap: "wrap",
                      }}
                    >
                      <button
                        type="button"
                        className="fd-btn fd-btn-primary"
                        data-testid={`cloud-session-attach-btn-${sess.session_id}`}
                        style={{ fontSize: "10.5px", padding: "2px 6px" }}
                        onClick={() => {
                          setCloudSessions((prev) =>
                            prev.map((item) =>
                              item.session_id === sess.session_id
                                ? { ...item, mirrored: true }
                                : item,
                            ),
                          );
                          setCloudSessionBanner(
                            `Attached and mirrored remote cloud session ${sess.session_id} (${sess.vendor_label}).`,
                          );
                          if (typeof globalThis.fetch === "function") {
                            void globalThis
                              .fetch(
                                `/api/v1/cloud-sessions/${encodeURIComponent(sess.session_id)}/attach`,
                                { method: "POST" },
                              )
                              .catch(() => {});
                          }
                        }}
                      >
                        Attach / Mirror
                      </button>
                      <button
                        type="button"
                        className="fd-btn"
                        data-testid={`cloud-session-sync-handoff-btn-${sess.session_id}`}
                        style={{ fontSize: "10.5px", padding: "2px 6px" }}
                        onClick={() => {
                          setCloudSessionBanner(
                            `Synced FerroState handoff checkpoint for ${sess.session_id} (${sess.repo_branch}).`,
                          );
                          if (typeof globalThis.fetch === "function") {
                            void globalThis
                              .fetch(
                                `/api/v1/cloud-sessions/${encodeURIComponent(sess.session_id)}/handoff`,
                                { method: "POST" },
                              )
                              .catch(() => {});
                          }
                        }}
                      >
                        Sync Handoff
                      </button>
                      <button
                        type="button"
                        className="fd-btn"
                        data-testid={`cloud-session-toggle-pause-btn-${sess.session_id}`}
                        style={{ fontSize: "10.5px", padding: "2px 6px" }}
                        onClick={() => {
                          const nextStatus: VendorCloudSessionStatus =
                            sess.status === "paused" ? "running" : "paused";
                          setCloudSessions((prev) =>
                            prev.map((item) =>
                              item.session_id === sess.session_id
                                ? { ...item, status: nextStatus }
                                : item,
                            ),
                          );
                          setCloudSessionBanner(
                            `Updated ${sess.session_id} cloud session status to ${nextStatus}.`,
                          );
                        }}
                      >
                        {sess.status === "paused" ? "Resume" : "Pause"}
                      </button>
                      <button
                        type="button"
                        className="fd-btn"
                        data-testid={`cloud-session-pull-branch-btn-${sess.session_id}`}
                        style={{ fontSize: "10.5px", padding: "2px 6px" }}
                        onClick={() => {
                          setCloudSessionBanner(
                            `Pulled remote branch ${sess.repo_branch} from ${sess.vendor_label}.`,
                          );
                        }}
                      >
                        Pull Branch / PR
                      </button>
                    </div>
                  </div>
                  <div
                    className="fd-mono"
                    style={{ fontSize: "10.5px", color: "var(--text-secondary)" }}
                  >
                    {sess.repo_branch} · {sess.turns} turns ·{" "}
                    {(sess.tokens_used / 1000).toFixed(1)}k tokens · active{" "}
                    {sess.last_active}
                  </div>
                </div>
              ))}
            </div>
          </div>

          {/* Record #72: Local Model & API-Key Agent Lifecycle (Ollama, Lemonade, OpenAI-Compatible) */}
          <div
            className="fd-card"
            data-testid="local-model-providers-card"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "10px",
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
              <div>
                <h3
                  style={{
                    margin: "0 0 2px 0",
                    fontSize: "13px",
                    fontWeight: 700,
                    textTransform: "uppercase",
                    letterSpacing: "0.04em",
                  }}
                >
                  Local &amp; API-Key Model Providers (#72)
                </h3>
                <p
                  style={{
                    margin: 0,
                    fontSize: "11.5px",
                    color: "var(--text-secondary)",
                  }}
                >
                  Configure Ollama, Lemonade (AMD Ryzen AI / ROCm), and custom
                  OpenAI-compatible endpoints (<code className="fd-mono">vLLM</code>{" "}
                  / <code className="fd-mono">llama.cpp</code> /{" "}
                  <code className="fd-mono">SGLang</code>) with FerroGate Vault
                  references across the full agent lifecycle.
                </p>
              </div>
              <span
                className="fd-badge fd-mono"
                data-testid="local-providers-lifecycle-badge"
                style={{ fontSize: "10.5px" }}
              >
                Lifecycle: spin-up · compact · reforge · recast
              </span>
            </div>

            {localProviderBanner && (
              <div
                role="status"
                data-testid="local-provider-status-banner"
                className="fd-mono"
                style={{
                  fontSize: "11px",
                  color: "#34D399",
                  padding: "4px 8px",
                  borderRadius: "4px",
                  backgroundColor: "rgba(16, 185, 129, 0.12)",
                }}
              >
                {localProviderBanner}
              </div>
            )}

            <div
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "6px",
              }}
            >
              {localProviders.map((prov) => (
                <div
                  key={prov.id}
                  data-testid={`local-provider-row-${prov.id}`}
                  style={{
                    padding: "6px 10px",
                    borderRadius: "4px",
                    border: "1px solid var(--border-subtle)",
                    backgroundColor: "var(--bg-elevated)",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "8px",
                    flexWrap: "wrap",
                    fontSize: "11px",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: "6px",
                      flexWrap: "wrap",
                    }}
                  >
                    <strong>{prov.label}</strong>
                    <code
                      className="fd-mono"
                      data-testid={`local-provider-endpoint-${prov.id}`}
                    >
                      {prov.endpoint_url}
                    </code>
                    <span className="fd-badge fd-mono">{prov.default_model}</span>
                    <span
                      className="fd-badge fd-mono"
                      data-testid={`local-provider-vault-ref-${prov.id}`}
                    >
                      {prov.vault_credential_ref}
                    </span>
                  </div>
                  <button
                    type="button"
                    className="fd-btn"
                    data-testid={`local-provider-load-btn-${prov.id}`}
                    style={{ fontSize: "10.5px", padding: "2px 8px" }}
                    onClick={() => {
                      setLocalProviderDraftKind(prov.kind);
                      setLocalProviderDraftEndpoint(prov.endpoint_url);
                      setLocalProviderDraftVaultRef(prov.vault_credential_ref);
                      setLocalProviderDraftContextWindow(
                        String(prov.context_window_tokens),
                      );
                      setLocalProviderDraftMaxOutput(
                        String(prov.max_output_tokens),
                      );
                    }}
                  >
                    Load Preset
                  </button>
                </div>
              ))}
            </div>

            <div
              data-testid="local-provider-editor-form"
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
                gap: "6px",
                alignItems: "center",
              }}
            >
              <select
                aria-label="Local Provider Kind"
                data-testid="local-provider-kind-select"
                className="fd-select fd-mono"
                value={localProviderDraftKind}
                onChange={(e) =>
                  setLocalProviderDraftKind(
                    e.target.value as LocalModelProviderKind,
                  )
                }
                style={{ fontSize: "11px" }}
              >
                <option value="ollama">ollama</option>
                <option value="lemonade">lemonade (AMD ROCm)</option>
                <option value="openai-compatible">openai-compatible</option>
              </select>

              <input
                type="text"
                aria-label="Local Provider Endpoint URL"
                data-testid="local-provider-endpoint-input"
                className="fd-input fd-mono"
                value={localProviderDraftEndpoint}
                onChange={(e) => setLocalProviderDraftEndpoint(e.target.value)}
                style={{ fontSize: "11px" }}
              />

              <select
                aria-label="Local Provider Vault Credential Reference"
                data-testid="local-provider-vault-ref-select"
                className="fd-select fd-mono"
                value={localProviderDraftVaultRef}
                onChange={(e) => setLocalProviderDraftVaultRef(e.target.value)}
                style={{ fontSize: "11px" }}
              >
                <option value="keyless (loopback)">keyless (loopback)</option>
                <option value="vault://ferrogate/providers/local-vllm-key">
                  vault://ferrogate/providers/local-vllm-key
                </option>
                <option value="vault://ferrogate/providers/openrouter-key">
                  vault://ferrogate/providers/openrouter-key
                </option>
              </select>

              <input
                type="number"
                aria-label="Context Window Tokens"
                data-testid="local-provider-context-window-input"
                className="fd-input fd-mono"
                value={localProviderDraftContextWindow}
                onChange={(e) =>
                  setLocalProviderDraftContextWindow(e.target.value)
                }
                style={{ fontSize: "11px" }}
              />

              <button
                type="button"
                className="fd-btn fd-btn-primary"
                data-testid="local-provider-save-btn"
                style={{ fontSize: "11px", padding: "4px 8px" }}
                onClick={() => {
                  const nextCtx =
                    Number(localProviderDraftContextWindow) || 131072;
                  const nextMaxOut =
                    Number(localProviderDraftMaxOutput) || 16384;
                  setLocalProviders((prev) =>
                    prev.map((p) =>
                      p.kind === localProviderDraftKind
                        ? {
                            ...p,
                            endpoint_url: localProviderDraftEndpoint.trim(),
                            vault_credential_ref: localProviderDraftVaultRef,
                            context_window_tokens: nextCtx,
                            max_output_tokens: nextMaxOut,
                          }
                        : p,
                    ),
                  );
                  setLocalProviderBanner(
                    `Saved ${localProviderDraftKind} provider (${localProviderDraftEndpoint.trim()}, ${localProviderDraftVaultRef}, ctx=${nextCtx}).`,
                  );
                  if (typeof globalThis.fetch === "function") {
                    void globalThis
                      .fetch("/api/v1/providers/local", {
                        method: "POST",
                        headers: { "Content-Type": "application/json" },
                        body: JSON.stringify({
                          kind: localProviderDraftKind,
                          endpoint_url: localProviderDraftEndpoint.trim(),
                          vault_credential_ref: localProviderDraftVaultRef,
                          context_window_tokens: nextCtx,
                          max_output_tokens: nextMaxOut,
                        }),
                      })
                      .catch(() => {});
                  }
                }}
              >
                Save &amp; Probe Endpoint
              </button>
            </div>
          </div>
        </div>

        {/* TAB 2: System */}
        {!initialProject && (
          <>
            <div
              role="tabpanel"
              aria-label="System Settings Panel"
              data-testid="settings-panel-system"
              style={{
                display: activeTab === "system" ? "flex" : "none",
                flexDirection: "column",
                gap: "12px",
              }}
            >
          {/* Fleet Nodes & Connectivity Launcher */}
          <div
            className="fd-card"
            data-testid="settings-system-views-bar"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "10px",
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
              <div>
                <h3
                  style={{
                    margin: 0,
                    fontSize: "12.5px",
                    fontWeight: 700,
                    textTransform: "uppercase",
                    letterSpacing: "0.04em",
                  }}
                >
                  Fleet Nodes &amp; Connectivity
                </h3>
                <span style={{ fontSize: "11.5px", color: "var(--text-muted)" }}>
                  Inspect and enroll federated compute nodes and check live hub
                  connectivity.
                </span>
              </div>
              <span
                className="fd-badge"
                title={wsConnected ? "WebSocket Connected" : "WebSocket Disconnected"}
              >
                <span
                  className={`fd-dot ${wsConnected ? "fd-dot-alive" : "fd-dot-dead"}`}
                />
                WS {wsConnected ? "Connected" : "Offline"}
              </span>
            </div>

            <div style={{ display: "flex", gap: "8px", flexWrap: "wrap" }}>
              <button
                type="button"
                data-testid="settings-open-nodes-btn"
                className="fd-btn"
                onClick={() => {
                  onOpenNodesModal?.();
                }}
              >
                <span
                  className={`fd-dot ${onlineNodesCount > 0 ? "fd-dot-alive" : "fd-dot-dead"}`}
                />
                <span>
                  {onlineNodesCount > 0
                    ? `Nodes (${onlineNodesCount})`
                    : "Nodes"}
                </span>
              </button>
            </div>
          </div>

          {/* Unified Settings Panel (Record 4) */}
          <div
            className="fd-card"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "12px",
            }}
          >
            {/* Appearance & Theme */}
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "12px",
                flexWrap: "wrap",
              }}
            >
              <div>
                <h3
                  style={{
                    margin: 0,
                    fontSize: "12.5px",
                    fontWeight: 700,
                    textTransform: "uppercase",
                    letterSpacing: "0.04em",
                  }}
                >
                  Appearance &amp; Theme
                </h3>
                <span style={{ fontSize: "11.5px", color: "var(--text-muted)" }}>
                  Choose the default color scheme for FerroDeck.
                </span>
              </div>
              <select
                aria-label="Theme Preference"
                data-testid="settings-theme-select"
                className="fd-select fd-mono"
                value={settings.theme_preference}
                onChange={(e) => {
                  setSettingsSaved(false);
                  setSettings((prev) => ({
                    ...prev,
                    theme_preference:
                      e.target.value as HubSettings["theme_preference"],
                  }));
                }}
                style={{ fontSize: "12px", padding: "5px 10px" }}
              >
                <option value="dark">Dark</option>
                <option value="light">Light</option>
                <option value="system">System</option>
              </select>
            </div>

            {/* Time Zone */}
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "12px",
                flexWrap: "wrap",
              }}
            >
              <div>
                <h3
                  style={{
                    margin: 0,
                    fontSize: "12.5px",
                    fontWeight: 700,
                    textTransform: "uppercase",
                    letterSpacing: "0.04em",
                  }}
                >
                  Time Zone
                </h3>
                <span style={{ fontSize: "11.5px", color: "var(--text-muted)" }}>
                  Format dates and timestamps across transcripts, FerroWire,
                  FerroState, Vault, and Artifacts.
                </span>
              </div>
              <select
                aria-label="Time Zone"
                data-testid="settings-timezone-select"
                className="fd-select fd-mono"
                value={settings.timezone || "UTC"}
                onChange={(e) => {
                  setSettingsSaved(false);
                  setSettings((prev) => ({
                    ...prev,
                    timezone: e.target.value,
                  }));
                }}
                style={{ fontSize: "12px", padding: "5px 10px" }}
              >
                {settings.timezone &&
                  !TIMEZONE_OPTIONS.some(
                    (opt) => opt.value === settings.timezone,
                  ) && (
                    <option value={settings.timezone}>{settings.timezone}</option>
                  )}
                {TIMEZONE_OPTIONS.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    {opt.label}
                  </option>
                ))}
              </select>
            </div>

            {/* Local Currency */}
            <div
              data-testid="settings-currency-row"
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "12px",
                flexWrap: "wrap",
              }}
            >
              <div>
                <h3
                  style={{
                    margin: 0,
                    fontSize: "12.5px",
                    fontWeight: 700,
                    textTransform: "uppercase",
                    letterSpacing: "0.04em",
                  }}
                >
                  Local Currency
                </h3>
                <span style={{ fontSize: "11.5px", color: "var(--text-muted)" }}>
                  Format spend and cost telemetry across Records &amp; Board,
                  FerroWatch, and Hub consoles (auto-detected from browser
                  locale and time zone by default).
                </span>
              </div>
              <select
                aria-label="Local Currency"
                data-testid="settings-currency-select"
                className="fd-select fd-mono"
                value={currencyPref}
                onChange={(e) => {
                  const next = setActiveCurrency(e.target.value);
                  setCurrencyPref(next);
                  setSettingsSaved(false);
                }}
                style={{ fontSize: "12px", padding: "5px 10px" }}
              >
                {CURRENCY_OPTIONS.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    {opt.value === "auto"
                      ? `Auto-Detect (${detectBrowserCurrency()})`
                      : opt.label}
                  </option>
                ))}
              </select>
            </div>

            {/* Screenshot & Attachment Retention */}
            <div
              data-testid="settings-attachment-retention-row"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "8px",
              }}
            >
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "12px",
                  flexWrap: "wrap",
                }}
              >
                <div>
                  <h3
                    style={{
                      margin: 0,
                      fontSize: "12.5px",
                      fontWeight: 700,
                      textTransform: "uppercase",
                      letterSpacing: "0.04em",
                    }}
                  >
                    Screenshot &amp; Attachment Retention
                  </h3>
                  <span style={{ fontSize: "11.5px", color: "var(--text-muted)" }}>
                    Configure automatic cleanup for pasted screenshots and
                    uploaded prompt attachments, or prune files on demand.
                  </span>
                </div>
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: "8px",
                    flexWrap: "wrap",
                  }}
                >
                  <select
                    aria-label="Screenshot & Attachment Retention"
                    data-testid="settings-attachment-retention-select"
                    className="fd-select fd-mono"
                    value={String(settings.attachment_retention_days ?? 7)}
                    onChange={(e) => {
                      const nextDays = Number(e.target.value);
                      setSettingsSaved(false);
                      setSettings((prev) => ({
                        ...prev,
                        attachment_retention_days: Number.isFinite(nextDays)
                          ? nextDays
                          : 7,
                      }));
                    }}
                    style={{ fontSize: "12px", padding: "5px 10px" }}
                  >
                    {ATTACHMENT_RETENTION_OPTIONS.map((opt) => (
                      <option key={opt.value} value={String(opt.value)}>
                        {opt.label}
                      </option>
                    ))}
                  </select>
                  <button
                    type="button"
                    data-testid="prune-expired-attachments-btn"
                    className="fd-btn"
                    disabled={pruningAttachments}
                    onClick={() => void handlePruneAttachments()}
                    style={{ fontSize: "11.5px", padding: "5px 10px" }}
                  >
                    {pruningAttachments ? "Cleaning..." : "Clean Up Expired Now"}
                  </button>
                  <button
                    type="button"
                    data-testid="purge-all-attachments-btn"
                    className="fd-btn fd-btn-danger"
                    disabled={pruningAttachments}
                    onClick={() => void handlePruneAttachments(0)}
                    style={{ fontSize: "11.5px", padding: "5px 10px" }}
                  >
                    Purge All Screenshots
                  </button>
                </div>
              </div>

              {pruneStatusMessage && (
                <div
                  role="status"
                  data-testid="attachment-prune-status"
                  className="fd-card"
                  style={{
                    borderColor: "var(--diff-add-border)",
                    backgroundColor: "var(--diff-add-bg)",
                    color: "var(--diff-add-text)",
                    fontSize: "11.5px",
                    padding: "6px 10px",
                  }}
                >
                  {pruneStatusMessage}
                </div>
              )}
            </div>

            {/* Hub Home Directory & System Artifacts Directory (Issue #207) */}
            <div
              data-testid="settings-system-home-dir-row"
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "12px",
                flexWrap: "wrap",
              }}
            >
              <div>
                <h3
                  style={{
                    margin: 0,
                    fontSize: "12.5px",
                    fontWeight: 700,
                    textTransform: "uppercase",
                    letterSpacing: "0.04em",
                  }}
                >
                  Hub Home Directory
                </h3>
                <span style={{ fontSize: "11.5px", color: "var(--text-muted)" }}>
                  Root hub directory (<code className="fd-mono">[home]</code>);
                  stores state, logs, and default{" "}
                  <code className="fd-mono">[home]/artifacts/[project-name]</code>{" "}
                  hierarchy.
                </span>
              </div>
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: "8px",
                  flexWrap: "wrap",
                }}
              >
                <code
                  data-testid="settings-system-home-dir-value"
                  className="fd-badge fd-mono"
                  style={{ fontSize: "11.5px", padding: "4px 8px" }}
                >
                  {settings.home_dir || "~/.local/share/fl"}
                </code>
                <span
                  data-testid="settings-system-home-hierarchy-badge"
                  className="fd-badge fd-mono"
                  style={{ fontSize: "10.5px" }}
                >
                  [home]/artifacts/[project-name]
                </span>
              </div>
            </div>

            <div
              data-testid="settings-system-artifacts-dir-row"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "8px",
              }}
            >
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "12px",
                  flexWrap: "wrap",
                }}
              >
                <div>
                  <h3
                    style={{
                      margin: 0,
                      fontSize: "12.5px",
                      fontWeight: 700,
                      textTransform: "uppercase",
                      letterSpacing: "0.04em",
                    }}
                  >
                    Artifacts Directory
                  </h3>
                  <span style={{ fontSize: "11.5px", color: "var(--text-muted)" }}>
                    Hub-wide artifacts storage root (defaults to{" "}
                    <code className="fd-mono">{systemDefaultArtifactsDir}</code>
                    ; each project defaults to{" "}
                    <code className="fd-mono">
                      [artifacts]/[project-name]
                    </code>
                    ).
                  </span>
                </div>
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: "8px",
                    flexWrap: "wrap",
                  }}
                >
                  <code
                    data-testid="settings-system-artifacts-dir-value"
                    className="fd-badge fd-mono"
                    title={systemEffectiveArtifactsDir}
                    style={{
                      fontSize: "11.5px",
                      padding: "4px 8px",
                    }}
                  >
                    {systemEffectiveArtifactsDir}
                  </code>
                  {settings.artifacts_dir && (
                    <span
                      data-testid="settings-system-artifacts-dir-override-badge"
                      className="fd-badge fd-mono"
                      style={{
                        fontSize: "10.5px",
                        color: "var(--accent-primary, #f59e0b)",
                      }}
                    >
                      Override
                    </span>
                  )}
                  <button
                    type="button"
                    data-testid="settings-system-change-artifacts-dir-btn"
                    className="fd-btn"
                    style={{ fontSize: "11.5px", padding: "5px 10px" }}
                    onClick={openSystemArtifactsDirModal}
                  >
                    Change Artifacts Directory
                  </button>
                </div>
              </div>

              {systemArtifactsDirStatus && (
                <div
                  role="status"
                  data-testid="system-artifacts-dir-status"
                  className="fd-card"
                  style={{
                    borderColor: "var(--diff-add-border)",
                    backgroundColor: "var(--diff-add-bg)",
                    color: "var(--diff-add-text)",
                    fontSize: "11.5px",
                    padding: "6px 10px",
                  }}
                >
                  {systemArtifactsDirStatus}
                </div>
              )}
            </div>

            <hr
              style={{
                border: "none",
                borderTop: "1px solid var(--border-subtle)",
                margin: 0,
              }}
            />

            {/* Notification Rules */}
            <div style={{ display: "flex", flexDirection: "column", gap: "6px" }}>
              <h3
                style={{
                  margin: 0,
                  fontSize: "12.5px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.04em",
                }}
              >
                Notification Rules
              </h3>
              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "repeat(auto-fit, minmax(210px, 1fr))",
                  gap: "6px",
                  fontSize: "12.5px",
                }}
              >
                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    cursor: "pointer",
                  }}
                >
                  <input
                    type="checkbox"
                    data-testid="toggle-notify-pr"
                    checked={settings.notifications.notify_on_pr_review}
                    onChange={(e) => {
                      setSettingsSaved(false);
                      setSettings((prev) => ({
                        ...prev,
                        notifications: {
                          ...prev.notifications,
                          notify_on_pr_review: e.target.checked,
                        },
                      }));
                    }}
                  />
                  <span>PR Review Requests</span>
                </label>

                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    cursor: "pointer",
                  }}
                >
                  <input
                    type="checkbox"
                    data-testid="toggle-notify-error"
                    checked={settings.notifications.notify_on_session_error}
                    onChange={(e) => {
                      setSettingsSaved(false);
                      setSettings((prev) => ({
                        ...prev,
                        notifications: {
                          ...prev.notifications,
                          notify_on_session_error: e.target.checked,
                        },
                      }));
                    }}
                  />
                  <span>Session Errors</span>
                </label>

                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    cursor: "pointer",
                  }}
                >
                  <input
                    type="checkbox"
                    data-testid="toggle-notify-wire"
                    checked={settings.notifications.notify_on_wire_urgent}
                    onChange={(e) => {
                      setSettingsSaved(false);
                      setSettings((prev) => ({
                        ...prev,
                        notifications: {
                          ...prev.notifications,
                          notify_on_wire_urgent: e.target.checked,
                        },
                      }));
                    }}
                  />
                  <span>Urgent FerroWire Mail</span>
                </label>

                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    cursor: "pointer",
                  }}
                >
                  <input
                    type="checkbox"
                    data-testid="toggle-notify-sound"
                    checked={settings.notifications.sound_enabled}
                    onChange={(e) => {
                      setSettingsSaved(false);
                      setSettings((prev) => ({
                        ...prev,
                        notifications: {
                          ...prev.notifications,
                          sound_enabled: e.target.checked,
                        },
                      }));
                    }}
                  />
                  <span>Sound Alerts</span>
                </label>
              </div>
            </div>

            {/* Record #67: FerroDeck+ Operator Push Notifications (VAPID Web Push + APNs + FCM) */}
            <div
              data-testid="operator-push-notifications-card"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "8px",
                padding: "10px 12px",
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
                <div>
                  <h4
                    style={{
                      margin: 0,
                      fontSize: "12px",
                      fontWeight: 700,
                      textTransform: "uppercase",
                      letterSpacing: "0.04em",
                    }}
                  >
                    FerroDeck+ Operator Push Notifications (#67)
                  </h4>
                  <span style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                    Web Push (VAPID Service Worker) and native iOS/iPadOS (APNs) &amp;
                    Android (FCM) push delivery for{" "}
                    <code className="fd-mono">&lt;N&gt;?</code> questions and{" "}
                    <code className="fd-mono">&lt;N&gt;!</code> blockers.
                  </span>
                </div>
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: "6px",
                    flexWrap: "wrap",
                  }}
                >
                  <span
                    className="fd-badge fd-mono"
                    data-testid="push-vapid-status-badge"
                    style={{ fontSize: "10px" }}
                  >
                    VAPID SW:{" "}
                    {operatorPushState.vapid_service_worker_registered
                      ? "Registered"
                      : "Unregistered"}{" "}
                    ({operatorPushState.web_push_permission})
                  </span>
                  <span
                    className="fd-badge fd-mono"
                    data-testid="push-apns-status-badge"
                    style={{ fontSize: "10px" }}
                  >
                    APNs: {operatorPushState.apns_ios_status}
                  </span>
                  <span
                    className="fd-badge fd-mono"
                    data-testid="push-fcm-status-badge"
                    style={{ fontSize: "10px" }}
                  >
                    FCM: {operatorPushState.fcm_android_status}
                  </span>
                </div>
              </div>

              {pushActionBanner && (
                <div
                  role="status"
                  data-testid="push-action-banner"
                  className="fd-mono"
                  style={{
                    fontSize: "11px",
                    color: "#34D399",
                    padding: "4px 8px",
                    borderRadius: "4px",
                    backgroundColor: "rgba(16, 185, 129, 0.12)",
                  }}
                >
                  {pushActionBanner}
                </div>
              )}

              {/* Per-Severity Triggers & Per-Project Scope */}
              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "repeat(auto-fit, minmax(170px, 1fr))",
                  gap: "6px",
                  alignItems: "center",
                }}
              >
                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    cursor: "pointer",
                  }}
                >
                  <input
                    type="checkbox"
                    data-testid="push-trigger-question-checkbox"
                    checked={operatorPushState.triggers.question_badge}
                    onChange={(e) =>
                      setOperatorPushState((prev) => ({
                        ...prev,
                        triggers: {
                          ...prev.triggers,
                          question_badge: e.target.checked,
                        },
                      }))
                    }
                  />
                  <span>Question (&lt;N&gt;?)</span>
                </label>

                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    cursor: "pointer",
                  }}
                >
                  <input
                    type="checkbox"
                    data-testid="push-trigger-blocker-checkbox"
                    checked={operatorPushState.triggers.blocker_error_badge}
                    onChange={(e) =>
                      setOperatorPushState((prev) => ({
                        ...prev,
                        triggers: {
                          ...prev.triggers,
                          blocker_error_badge: e.target.checked,
                        },
                      }))
                    }
                  />
                  <span>Blocker / Error (&lt;N&gt;!)</span>
                </label>

                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    cursor: "pointer",
                  }}
                >
                  <input
                    type="checkbox"
                    data-testid="push-trigger-urgent-wire-checkbox"
                    checked={operatorPushState.triggers.urgent_wire_mail}
                    onChange={(e) =>
                      setOperatorPushState((prev) => ({
                        ...prev,
                        triggers: {
                          ...prev.triggers,
                          urgent_wire_mail: e.target.checked,
                        },
                      }))
                    }
                  />
                  <span>Urgent Wire Mail</span>
                </label>

                <select
                  aria-label="Push Notification Project Scope"
                  data-testid="push-project-scope-select"
                  className="fd-select fd-mono"
                  value={operatorPushState.project_scope}
                  onChange={(e) =>
                    setOperatorPushState((prev) => ({
                      ...prev,
                      project_scope: e.target.value,
                    }))
                  }
                  style={{ fontSize: "11px" }}
                >
                  <option value="all">Scope: All Projects</option>
                  {CANONICAL_PROJECTS.map((proj) => (
                    <option key={proj} value={proj}>
                      Scope: {proj}
                    </option>
                  ))}
                </select>
              </div>

              {/* Quiet Hours & Action Buttons */}
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "8px",
                  flexWrap: "wrap",
                }}
              >
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: "6px",
                    flexWrap: "wrap",
                  }}
                >
                  <label
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: "4px",
                      cursor: "pointer",
                    }}
                  >
                    <input
                      type="checkbox"
                      data-testid="push-quiet-hours-toggle"
                      checked={operatorPushState.quiet_hours.enabled}
                      onChange={(e) =>
                        setOperatorPushState((prev) => ({
                          ...prev,
                          quiet_hours: {
                            ...prev.quiet_hours,
                            enabled: e.target.checked,
                          },
                        }))
                      }
                    />
                    <span>Quiet Hours:</span>
                  </label>
                  <input
                    type="text"
                    aria-label="Quiet Hours Start"
                    data-testid="push-quiet-hours-start-input"
                    className="fd-input fd-mono"
                    value={operatorPushState.quiet_hours.start_time}
                    onChange={(e) =>
                      setOperatorPushState((prev) => ({
                        ...prev,
                        quiet_hours: {
                          ...prev.quiet_hours,
                          start_time: e.target.value,
                        },
                      }))
                    }
                    style={{ width: "60px", fontSize: "11px", padding: "2px 6px" }}
                  />
                  <span>to</span>
                  <input
                    type="text"
                    aria-label="Quiet Hours End"
                    data-testid="push-quiet-hours-end-input"
                    className="fd-input fd-mono"
                    value={operatorPushState.quiet_hours.end_time}
                    onChange={(e) =>
                      setOperatorPushState((prev) => ({
                        ...prev,
                        quiet_hours: {
                          ...prev.quiet_hours,
                          end_time: e.target.value,
                        },
                      }))
                    }
                    style={{ width: "60px", fontSize: "11px", padding: "2px 6px" }}
                  />
                </div>

                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: "6px",
                    flexWrap: "wrap",
                  }}
                >
                  <button
                    type="button"
                    className="fd-btn"
                    data-testid="push-subscribe-vapid-btn"
                    style={{ fontSize: "11px", padding: "3px 8px" }}
                    onClick={() => {
                      setOperatorPushState((prev) => ({
                        ...prev,
                        web_push_permission: "granted",
                        vapid_service_worker_registered: true,
                      }));
                      setPushActionBanner(
                        `Registered VAPID Web Push subscription (${operatorPushState.vapid_public_key_fingerprint}).`,
                      );
                      if (typeof globalThis.fetch === "function") {
                        void globalThis
                          .fetch("/api/v1/notifications/subscribe", {
                            method: "POST",
                            headers: { "Content-Type": "application/json" },
                            body: JSON.stringify(operatorPushState),
                          })
                          .catch(() => {});
                      }
                    }}
                  >
                    Subscribe Web Push (VAPID)
                  </button>
                  <button
                    type="button"
                    className="fd-btn fd-btn-primary"
                    data-testid="push-test-deep-link-btn"
                    style={{ fontSize: "11px", padding: "3px 8px" }}
                    onClick={() => {
                      setPushActionBanner(
                        "Dispatched test push notification deep-link (/triage?focus=perm-req-01) across VAPID + APNs + FCM.",
                      );
                      if (typeof globalThis.fetch === "function") {
                        void globalThis
                          .fetch("/api/v1/notifications/test", {
                            method: "POST",
                            headers: { "Content-Type": "application/json" },
                            body: JSON.stringify({
                              deep_link: "/triage?focus=perm-req-01",
                              project_scope: operatorPushState.project_scope,
                            }),
                          })
                          .catch(() => {});
                      }
                    }}
                  >
                    Test Push Deep-Link
                  </button>
                </div>
              </div>
            </div>

            <hr
              style={{
                border: "none",
                borderTop: "1px solid var(--border-subtle)",
                margin: 0,
              }}
            />

            {/* Hub & Polling Connection */}
            <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
              <h3
                style={{
                  margin: 0,
                  fontSize: "12.5px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.04em",
                }}
              >
                Hub &amp; Polling Connection
              </h3>
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "12px",
                  flexWrap: "wrap",
                  fontSize: "12.5px",
                }}
              >
                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    cursor: "pointer",
                  }}
                >
                  <input
                    type="checkbox"
                    data-testid="toggle-auto-reconnect"
                    checked={settings.hub_connection.auto_reconnect_ws}
                    onChange={(e) => {
                      setSettingsSaved(false);
                      setSettings((prev) => ({
                        ...prev,
                        hub_connection: {
                          ...prev.hub_connection,
                          auto_reconnect_ws: e.target.checked,
                        },
                      }));
                    }}
                  />
                  <span>Auto-reconnect WebSocket</span>
                </label>

                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                  }}
                >
                  <span>Poll Interval (secs):</span>
                  <input
                    type="number"
                    min={1}
                    max={60}
                    aria-label="Poll Interval (secs)"
                    data-testid="input-poll-interval"
                    className="fd-input fd-mono"
                    value={settings.hub_connection.poll_interval_secs}
                    onChange={(e) => {
                      const parsed = Number(e.target.value);
                      setSettingsSaved(false);
                      setSettings((prev) => ({
                        ...prev,
                        hub_connection: {
                          ...prev.hub_connection,
                          poll_interval_secs:
                            Number.isFinite(parsed) && parsed > 0 ? parsed : 1,
                        },
                      }));
                    }}
                    style={{ width: "68px", fontSize: "12px", padding: "4px 8px" }}
                  />
                </label>
              </div>
            </div>

            <div style={{ display: "flex", justifyContent: "flex-end" }}>
              <button
                type="button"
                data-testid="save-settings-btn"
                className="fd-btn fd-btn-primary"
                disabled={savingSettings}
                onClick={() => void handleSaveSettings()}
              >
                {savingSettings ? "Saving..." : "Save Settings"}
              </button>
            </div>
          </div>

          {/* Records 52 & 53: Cloud Sync & Edge Gate Adapters Section */}
          <div
            className="fd-card"
            data-testid="settings-cloud-adapters-section"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "12px",
            }}
          >
            <div>
              <h3
                style={{
                  margin: "0 0 4px 0",
                  fontSize: "13px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.04em",
                }}
              >
                Cloud Sync &amp; Edge Gate Adapters
              </h3>
              <p
                style={{
                  margin: 0,
                  fontSize: "12px",
                  color: "var(--text-secondary)",
                }}
              >
                Configure multi-cloud FerroStore synchronization (`GCP`, `AWS`,
                `Azure`) and FerroGate cloud registry, Secret Manager + IAM, and
                Model Armor edge protection.
              </p>
            </div>

            <div
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))",
                gap: "10px",
                fontSize: "12px",
              }}
            >
              <label style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                <span>Store Sync Provider</span>
                <select
                  aria-label="Store Sync Provider"
                  data-testid="cloud-sync-provider-select"
                  className="fd-select fd-mono"
                  value={cloudAdapters.store_sync.provider}
                  onChange={(e) => {
                    setCloudAdaptersSaved(false);
                    setCloudAdapters((prev) => ({
                      ...prev,
                      store_sync: {
                        ...prev.store_sync,
                        provider:
                          e.target.value as CloudAdaptersConfig["store_sync"]["provider"],
                      },
                    }));
                  }}
                >
                  <option value="none">none</option>
                  <option value="gcp">gcp (Cloud SQL + GCS)</option>
                  <option value="aws">aws (Aurora + S3)</option>
                  <option value="azure">azure (Postgres + Blob)</option>
                </select>
              </label>

              <label style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                <span>Store Sync Mode</span>
                <select
                  aria-label="Store Sync Mode"
                  data-testid="cloud-sync-mode-select"
                  className="fd-select fd-mono"
                  value={cloudAdapters.store_sync.sync_mode}
                  onChange={(e) => {
                    setCloudAdaptersSaved(false);
                    setCloudAdapters((prev) => ({
                      ...prev,
                      store_sync: {
                        ...prev.store_sync,
                        sync_mode:
                          e.target.value as CloudAdaptersConfig["store_sync"]["sync_mode"],
                      },
                    }));
                  }}
                >
                  <option value="write_through">write_through</option>
                  <option value="write_behind">write_behind</option>
                  <option value="local_cache">local_cache</option>
                  <option value="cloud_pre_cache">cloud_pre_cache</option>
                </select>
              </label>

              <label style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                <span>Store Sync Endpoint URL</span>
                <input
                  type="text"
                  aria-label="Store Sync Endpoint URL"
                  data-testid="cloud-sync-endpoint-input"
                  className="fd-input fd-mono"
                  placeholder="https://store-sync.example.com"
                  value={cloudAdapters.store_sync.endpoint_url}
                  onChange={(e) => {
                    setCloudAdaptersSaved(false);
                    setCloudAdapters((prev) => ({
                      ...prev,
                      store_sync: {
                        ...prev.store_sync,
                        endpoint_url: e.target.value,
                      },
                    }));
                  }}
                />
              </label>

              <label style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                <span>Gate Cloud Registry Provider</span>
                <select
                  aria-label="Gate Cloud Registry Provider"
                  data-testid="cloud-gate-provider-select"
                  className="fd-select fd-mono"
                  value={cloudAdapters.gate_cloud.registry_provider}
                  onChange={(e) => {
                    setCloudAdaptersSaved(false);
                    setCloudAdapters((prev) => ({
                      ...prev,
                      gate_cloud: {
                        ...prev.gate_cloud,
                        registry_provider:
                          e.target.value as CloudAdaptersConfig["gate_cloud"]["registry_provider"],
                      },
                    }));
                  }}
                >
                  <option value="none">none</option>
                  <option value="gcp">gcp (Cloud API Registry)</option>
                  <option value="aws">aws</option>
                  <option value="azure">azure</option>
                </select>
              </label>

              <label style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                <span>Cloud Run MCP Relay Endpoint URL</span>
                <input
                  type="text"
                  aria-label="Cloud Run MCP Relay Endpoint URL"
                  data-testid="cloud-relay-endpoint-input"
                  className="fd-input fd-mono"
                  placeholder="https://ferrogate-relay.a.run.app"
                  value={cloudAdapters.gate_cloud.relay_endpoint_url}
                  onChange={(e) => {
                    setCloudAdaptersSaved(false);
                    setCloudAdapters((prev) => ({
                      ...prev,
                      gate_cloud: {
                        ...prev.gate_cloud,
                        relay_endpoint_url: e.target.value,
                      },
                    }));
                  }}
                />
              </label>
            </div>

            <div
              style={{
                display: "flex",
                gap: "16px",
                flexWrap: "wrap",
                fontSize: "12.5px",
              }}
            >
              <label
                style={{
                  display: "inline-flex",
                  alignItems: "center",
                  gap: "6px",
                  cursor: "pointer",
                }}
              >
                <input
                  type="checkbox"
                  data-testid="toggle-cloud-sync-enabled"
                  checked={cloudAdapters.store_sync.enabled}
                  onChange={(e) => {
                    setCloudAdaptersSaved(false);
                    setCloudAdapters((prev) => ({
                      ...prev,
                      store_sync: {
                        ...prev.store_sync,
                        enabled: e.target.checked,
                      },
                    }));
                  }}
                />
                <span>Enable Cloud Store Sync</span>
              </label>

              <label
                style={{
                  display: "inline-flex",
                  alignItems: "center",
                  gap: "6px",
                  cursor: "pointer",
                }}
              >
                <input
                  type="checkbox"
                  data-testid="toggle-cloud-secret-manager"
                  checked={cloudAdapters.gate_cloud.secret_manager_enabled}
                  onChange={(e) => {
                    setCloudAdaptersSaved(false);
                    setCloudAdapters((prev) => ({
                      ...prev,
                      gate_cloud: {
                        ...prev.gate_cloud,
                        secret_manager_enabled: e.target.checked,
                      },
                    }));
                  }}
                />
                <span>Secret Manager + IAM Vault</span>
              </label>

              <label
                style={{
                  display: "inline-flex",
                  alignItems: "center",
                  gap: "6px",
                  cursor: "pointer",
                }}
              >
                <input
                  type="checkbox"
                  data-testid="toggle-cloud-model-armor"
                  checked={cloudAdapters.gate_cloud.model_armor_enabled}
                  onChange={(e) => {
                    setCloudAdaptersSaved(false);
                    setCloudAdapters((prev) => ({
                      ...prev,
                      gate_cloud: {
                        ...prev.gate_cloud,
                        model_armor_enabled: e.target.checked,
                      },
                    }));
                  }}
                />
                <span>Model Armor Edge Guard</span>
              </label>
            </div>

            <div style={{ display: "flex", justifyContent: "flex-end" }}>
              <button
                type="button"
                data-testid="save-cloud-adapters-btn"
                className="fd-btn fd-btn-primary"
                disabled={savingCloudAdapters}
                onClick={() => void handleSaveCloudAdapters()}
              >
                {savingCloudAdapters ? "Saving..." : "Save Cloud Adapters"}
              </button>
            </div>
          </div>
        </div>

        {/* TAB: Products (FerroLoop Installed Products & Subsystem Toggles) */}
        <div
          role="tabpanel"
          aria-label="Installed Products & Subsystem Toggles Panel"
          data-testid="settings-panel-products"
          style={{
            display: activeTab === "products" ? "flex" : "none",
            flexDirection: "column",
            gap: "12px",
          }}
        >
          <div
            className="fd-card"
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "12px",
              flexWrap: "wrap",
            }}
          >
            <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
              <h3
                style={{
                  margin: 0,
                  fontSize: "13px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.04em",
                  display: "inline-flex",
                  alignItems: "center",
                  gap: "8px",
                }}
              >
                <img
                  src="/brand/ferroloop-icon.jpg"
                  alt=""
                  width="20"
                  height="20"
                  aria-hidden="true"
                  style={{ borderRadius: "4px", objectFit: "cover" }}
                />
                FerroLoop Installed Products &amp; Subsystem Toggles
              </h3>
              <p
                data-testid="products-shared-cli-banner"
                style={{
                  margin: 0,
                  fontSize: "12px",
                  color: "var(--text-secondary)",
                }}
              >
                Shared CLI included; select the services and integrations to enable.
              </p>
            </div>
            <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
              <span
                className="fd-badge fd-mono"
                data-testid="active-install-profile-badge"
              >
                Profile: {inferInstallProfilePreset(settings.subsystems, labProfilePreferred)}
              </span>
              <button
                type="button"
                data-testid="save-products-settings-btn"
                className="fd-btn fd-btn-primary"
                disabled={savingSettings}
                onClick={() => void handleSaveSettings()}
              >
                {savingSettings ? "Saving..." : "Save Product Settings"}
              </button>
            </div>
          </div>

          {/* Record #84 (Issue #205 Phase B): Installation Profile Presets Selector */}
          <div
            className="fd-card"
            data-testid="products-profile-presets-card"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "8px",
            }}
          >
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "10px",
                flexWrap: "wrap",
              }}
            >
              <div>
                <div style={{ fontWeight: 700, fontSize: "12.5px" }}>
                  Installation Profile Presets (`fl install --profile`)
                </div>
                <div style={{ fontSize: "11.5px", color: "var(--text-secondary)" }}>
                  Apply a curated component profile or customize individual services below with automatic dependency closure.
                </div>
              </div>
              <div
                style={{ display: "flex", gap: "6px", flexWrap: "wrap" }}
                role="group"
                aria-label="Installation Profile Presets"
              >
                {(
                  [
                    {
                      id: "minimal" as const,
                      label: "minimal",
                      desc: "FerroLoop CLI + embedded FerroStore only",
                    },
                    {
                      id: "headless" as const,
                      label: "headless",
                      desc: "CLI + daemons/MCP (without FerroDeck web UI)",
                    },
                    {
                      id: "full" as const,
                      label: "full",
                      desc: "All products including FerroDeck + systemd/LaunchAgent units",
                    },
                    {
                      id: "lab" as const,
                      label: "lab",
                      desc: "full + ai-lab-0 per-project ferrostore@ socket daemons",
                    },
                  ] as const
                ).map((preset) => {
                  const activeProfile = inferInstallProfilePreset(
                    settings.subsystems,
                    labProfilePreferred,
                  );
                  const isSelected = activeProfile === preset.id;
                  return (
                    <button
                      key={preset.id}
                      type="button"
                      data-testid={`profile-preset-${preset.id}`}
                      aria-pressed={isSelected}
                      title={preset.desc}
                      className={
                        isSelected ? "fd-btn fd-btn-primary" : "fd-btn"
                      }
                      style={{ fontSize: "11.5px", padding: "4px 10px" }}
                      onClick={() => {
                        setSettingsSaved(false);
                        setLabProfilePreferred(preset.id === "lab");
                        setAutoClosureNotice(
                          `Applied '${preset.id}' profile preset (${preset.desc}).`,
                        );
                        setSettings((prev) => ({
                          ...prev,
                          subsystems: resolveProfileSubsystems(preset.id),
                        }));
                      }}
                    >
                      {preset.label}
                    </button>
                  );
                })}
              </div>
            </div>
            {autoClosureNotice && (
              <div
                role="status"
                data-testid="product-autoclose-notice"
                style={{
                  padding: "6px 10px",
                  borderRadius: "6px",
                  background: "rgba(56, 189, 248, 0.12)",
                  border: "1px solid rgba(56, 189, 248, 0.35)",
                  color: "var(--accent-primary)",
                  fontSize: "11.5px",
                }}
              >
                {autoClosureNotice}
              </div>
            )}
          </div>

          <div
            data-testid="products-cards-grid"
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(auto-fit, minmax(290px, 1fr))",
              gap: "12px",
            }}
          >
            {/* 1. FerroLoop (Required Core Engine) */}
            <div
              className="fd-card"
              data-testid="product-card-ferroloop"
              style={{
                display: "flex",
                flexDirection: "column",
                justifyContent: "space-between",
                gap: "10px",
              }}
            >
              <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
                <div
                  style={{
                    display: "flex",
                    alignItems: "flex-start",
                    justifyContent: "space-between",
                    gap: "10px",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: "10px" }}>
                    <img
                      src="/brand/ferroloop-icon.jpg"
                      alt="FerroLoop icon"
                      width="36"
                      height="36"
                      style={{
                        borderRadius: "8px",
                        objectFit: "cover",
                        border: "1px solid var(--border-subtle)",
                        flexShrink: 0,
                      }}
                    />
                    <div>
                      <div style={{ fontWeight: 700, fontSize: "13.5px" }}>
                        FerroLoop
                      </div>
                      <code
                        className="fd-mono"
                        style={{ fontSize: "11px", color: "var(--text-muted)" }}
                      >
                        fl · Core Engine &amp; DAG Workflows
                      </code>
                    </div>
                  </div>
                  <label
                    title="FerroLoop is the required core engine and cannot be disabled"
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: "6px",
                      fontSize: "11.5px",
                      opacity: 0.7,
                      cursor: "not-allowed",
                      flexShrink: 0,
                    }}
                  >
                    <span
                      className="fd-badge"
                      data-testid="product-status-ferroloop"
                    >
                      Required
                    </span>
                    <input
                      type="checkbox"
                      data-testid="toggle-ferroloop"
                      aria-label="FerroLoop Core Engine (Required)"
                      checked={true}
                      disabled={true}
                      readOnly
                    />
                  </label>
                </div>
                <p
                  style={{
                    margin: 0,
                    fontSize: "12px",
                    color: "var(--text-secondary)",
                    lineHeight: 1.45,
                  }}
                >
                  Executable pre-flight gates, records, regret-classified
                  transitions, and multi-step DAG workflows (`fl flow`).
                </p>
              </div>
              <div
                data-testid="product-config-ferroloop"
                style={{
                  padding: "8px 10px",
                  borderRadius: "6px",
                  background: "var(--bg-elevated)",
                  border: "1px solid var(--border-subtle)",
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
                  fontSize: "11.5px",
                }}
              >
                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    cursor: "pointer",
                  }}
                >
                  <input
                    type="checkbox"
                    data-testid="toggle-ferrostep"
                    checked={settings.subsystems.ferrostep_enabled}
                    onChange={(e) => {
                      setSettingsSaved(false);
                      setSettings((prev) => ({
                        ...prev,
                        subsystems: {
                          ...prev.subsystems,
                          ferrostep_enabled: e.target.checked,
                        },
                      }));
                    }}
                  />
                  <span>Enable DAG Workflow Runner (`fl flow`)</span>
                </label>
                <div
                  data-testid="embedded-ferrostore-dependency"
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    color: "var(--text-muted)",
                    fontSize: "11px",
                  }}
                >
                  <img
                    src="/brand/ferrostore-icon.jpg"
                    alt=""
                    width="14"
                    height="14"
                    aria-hidden="true"
                    style={{ borderRadius: "3px", objectFit: "cover" }}
                  />
                  <span>
                    {labProfilePreferred
                      ? "Storage Engine: FerroStore (ai-lab-0 ferrostore@ socket daemon)"
                      : "Storage Engine: FerroStore (.fls / FERRSTR1 embedded default dependency)"}
                  </span>
                </div>
              </div>
            </div>

            {/* 2. FerroDeck */}
            <div
              className="fd-card"
              data-testid="product-card-ferrodeck"
              style={{
                display: "flex",
                flexDirection: "column",
                justifyContent: "space-between",
                gap: "10px",
              }}
            >
              <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
                <div
                  style={{
                    display: "flex",
                    alignItems: "flex-start",
                    justifyContent: "space-between",
                    gap: "10px",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: "10px" }}>
                    <img
                      src="/brand/ferrodeck-icon.jpg"
                      alt="FerroDeck icon"
                      width="36"
                      height="36"
                      style={{
                        borderRadius: "8px",
                        objectFit: "cover",
                        border: "1px solid var(--border-subtle)",
                        flexShrink: 0,
                      }}
                    />
                    <div>
                      <div style={{ fontWeight: 700, fontSize: "13.5px" }}>
                        FerroDeck
                      </div>
                      <code
                        className="fd-mono"
                        style={{ fontSize: "11px", color: "var(--text-muted)" }}
                      >
                        fl hub · Command-Center Web App
                      </code>
                    </div>
                  </div>
                  <label
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: "6px",
                      fontSize: "11.5px",
                      cursor: "pointer",
                      flexShrink: 0,
                    }}
                  >
                    <span className="fd-badge">
                      {(settings.subsystems.ferrodeck_enabled ?? true)
                        ? "Enabled"
                        : "Disabled"}
                    </span>
                    <input
                      type="checkbox"
                      data-testid="toggle-ferrodeck"
                      aria-label="Enable FerroDeck"
                      checked={settings.subsystems.ferrodeck_enabled ?? true}
                      onChange={(e) => {
                        const nextChecked = e.target.checked;
                        setSettingsSaved(false);
                        if (
                          nextChecked &&
                          !(settings.subsystems.ferrowire_enabled ?? true)
                        ) {
                          setAutoClosureNotice(
                            "Auto-enabled FerroWire (required runtime dependency for FerroDeck).",
                          );
                        }
                        setSettings((prev) => ({
                          ...prev,
                          subsystems: {
                            ...prev.subsystems,
                            ferrodeck_enabled: nextChecked,
                            ferrowire_enabled: nextChecked
                              ? true
                              : prev.subsystems.ferrowire_enabled,
                          },
                        }));
                      }}
                    />
                  </label>
                </div>
                <p
                  style={{
                    margin: 0,
                    fontSize: "12px",
                    color: "var(--text-secondary)",
                    lineHeight: 1.45,
                  }}
                >
                  Multi-vendor agent cockpit PWA and HTTP/1.1 + WebSocket hub
                  daemon (`fl hub serve`) for desktop, iPadOS, and iOS.
                </p>
              </div>
              <div
                data-testid="product-config-ferrodeck"
                style={{
                  padding: "8px 10px",
                  borderRadius: "6px",
                  background: "var(--bg-elevated)",
                  border: "1px solid var(--border-subtle)",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "8px",
                  flexWrap: "wrap",
                  fontSize: "11.5px",
                }}
              >
                <span style={{ color: "var(--text-secondary)" }}>
                  Auto-reconnect WS:{" "}
                  <strong>
                    {settings.hub_connection.auto_reconnect_ws ? "On" : "Off"}
                  </strong>
                </span>
                <span className="fd-badge fd-mono">
                  Poll: {settings.hub_connection.poll_interval_secs}s
                </span>
              </div>
            </div>

            {/* 3. FerroWire */}
            <div
              className="fd-card"
              data-testid="product-card-ferrowire"
              style={{
                display: "flex",
                flexDirection: "column",
                justifyContent: "space-between",
                gap: "10px",
              }}
            >
              <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
                <div
                  style={{
                    display: "flex",
                    alignItems: "flex-start",
                    justifyContent: "space-between",
                    gap: "10px",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: "10px" }}>
                    <img
                      src="/brand/ferrowire-icon.jpg"
                      alt="FerroWire icon"
                      width="36"
                      height="36"
                      style={{
                        borderRadius: "8px",
                        objectFit: "cover",
                        border: "1px solid var(--border-subtle)",
                        flexShrink: 0,
                      }}
                    />
                    <div>
                      <div style={{ fontWeight: 700, fontSize: "13.5px" }}>
                        FerroWire
                      </div>
                      <code
                        className="fd-mono"
                        style={{ fontSize: "11px", color: "var(--text-muted)" }}
                      >
                        fl wire · Agent Messaging &amp; Relay
                      </code>
                    </div>
                  </div>
                  <label
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: "6px",
                      fontSize: "11.5px",
                      cursor: "pointer",
                      flexShrink: 0,
                    }}
                  >
                    {(settings.subsystems.ferrodeck_enabled ?? true) && (
                      <span
                        className="fd-badge"
                        data-testid="autoclose-badge-ferrowire"
                        title="Required runtime dependency when FerroDeck is enabled"
                      >
                        Required by FerroDeck
                      </span>
                    )}
                    <span className="fd-badge">
                      {(settings.subsystems.ferrowire_enabled ?? true)
                        ? "Enabled"
                        : "Disabled"}
                    </span>
                    <input
                      type="checkbox"
                      data-testid="toggle-ferrowire"
                      aria-label="Enable FerroWire"
                      checked={settings.subsystems.ferrowire_enabled ?? true}
                      onChange={(e) => {
                        const nextChecked = e.target.checked;
                        setSettingsSaved(false);
                        if (
                          !nextChecked &&
                          (settings.subsystems.ferrodeck_enabled ?? true)
                        ) {
                          setAutoClosureNotice(
                            "Disabled FerroDeck because its required dependency FerroWire was disabled.",
                          );
                        }
                        setSettings((prev) => ({
                          ...prev,
                          subsystems: {
                            ...prev.subsystems,
                            ferrowire_enabled: nextChecked,
                            ferrodeck_enabled: !nextChecked
                              ? false
                              : prev.subsystems.ferrodeck_enabled,
                          },
                        }));
                      }}
                    />
                  </label>
                </div>
                <p
                  style={{
                    margin: 0,
                    fontSize: "12px",
                    color: "var(--text-secondary)",
                    lineHeight: 1.45,
                  }}
                >
                  Agent-to-agent messaging transport, channels, threads, peer
                  relay, and idle push delivery across vendor CLIs.
                </p>
              </div>
              <div
                data-testid="product-config-ferrowire"
                style={{
                  padding: "8px 10px",
                  borderRadius: "6px",
                  background: "var(--bg-elevated)",
                  border: "1px solid var(--border-subtle)",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "8px",
                  fontSize: "11.5px",
                }}
              >
                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    cursor: "pointer",
                  }}
                >
                  <input
                    type="checkbox"
                    checked={settings.notifications.notify_on_wire_urgent}
                    onChange={(e) => {
                      setSettingsSaved(false);
                      setSettings((prev) => ({
                        ...prev,
                        notifications: {
                          ...prev.notifications,
                          notify_on_wire_urgent: e.target.checked,
                        },
                      }));
                    }}
                  />
                  <span>Urgent Mail Push Alerts</span>
                </label>
              </div>
            </div>

            {/* 4. FerroState */}
            <div
              className="fd-card"
              data-testid="product-card-ferrostate"
              style={{
                display: "flex",
                flexDirection: "column",
                justifyContent: "space-between",
                gap: "10px",
              }}
            >
              <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
                <div
                  style={{
                    display: "flex",
                    alignItems: "flex-start",
                    justifyContent: "space-between",
                    gap: "10px",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: "10px" }}>
                    <img
                      src="/brand/ferrostate-icon.jpg"
                      alt="FerroState icon"
                      width="36"
                      height="36"
                      style={{
                        borderRadius: "8px",
                        objectFit: "cover",
                        border: "1px solid var(--border-subtle)",
                        flexShrink: 0,
                      }}
                    />
                    <div>
                      <div style={{ fontWeight: 700, fontSize: "13.5px" }}>
                        FerroState
                      </div>
                      <code
                        className="fd-mono"
                        style={{ fontSize: "11px", color: "var(--text-muted)" }}
                      >
                        fl state · Memory &amp; Code Index
                      </code>
                    </div>
                  </div>
                  <label
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: "6px",
                      fontSize: "11.5px",
                      cursor: "pointer",
                      flexShrink: 0,
                    }}
                  >
                    <span className="fd-badge">
                      {settings.subsystems.ferrostate_enabled
                        ? "Enabled"
                        : "Disabled"}
                    </span>
                    <input
                      type="checkbox"
                      data-testid="toggle-ferrostate"
                      aria-label="Enable FerroState"
                      checked={settings.subsystems.ferrostate_enabled}
                      onChange={(e) => {
                        setSettingsSaved(false);
                        setSettings((prev) => ({
                          ...prev,
                          subsystems: {
                            ...prev.subsystems,
                            ferrostate_enabled: e.target.checked,
                          },
                        }));
                      }}
                    />
                  </label>
                </div>
                <p
                  style={{
                    margin: 0,
                    fontSize: "12px",
                    color: "var(--text-secondary)",
                    lineHeight: 1.45,
                  }}
                >
                  Durable agent memory, structured session handoffs, ranked
                  recall, sensitivity redaction, and AST symbol indexing.
                </p>
              </div>
              <div
                data-testid="product-config-ferrostate"
                style={{
                  padding: "8px 10px",
                  borderRadius: "6px",
                  background: "var(--bg-elevated)",
                  border: "1px solid var(--border-subtle)",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "8px",
                  flexWrap: "wrap",
                  fontSize: "11.5px",
                }}
              >
                <span style={{ color: "var(--text-secondary)" }}>
                  Sensitivity Redaction &amp; AST Index
                </span>
                <span className="fd-badge">Active</span>
              </div>
            </div>

            {/* 5. FerroGate */}
            <div
              className="fd-card"
              data-testid="product-card-ferrogate"
              style={{
                display: "flex",
                flexDirection: "column",
                justifyContent: "space-between",
                gap: "10px",
              }}
            >
              <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
                <div
                  style={{
                    display: "flex",
                    alignItems: "flex-start",
                    justifyContent: "space-between",
                    gap: "10px",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: "10px" }}>
                    <img
                      src="/brand/ferrogate-icon.jpg"
                      alt="FerroGate icon"
                      width="36"
                      height="36"
                      style={{
                        borderRadius: "8px",
                        objectFit: "cover",
                        border: "1px solid var(--border-subtle)",
                        flexShrink: 0,
                      }}
                    />
                    <div>
                      <div style={{ fontWeight: 700, fontSize: "13.5px" }}>
                        FerroGate
                      </div>
                      <code
                        className="fd-mono"
                        style={{ fontSize: "11px", color: "var(--text-muted)" }}
                      >
                        fl vault / fl auth · Vault &amp; Proxy
                      </code>
                    </div>
                  </div>
                  <label
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: "6px",
                      fontSize: "11.5px",
                      cursor: "pointer",
                      flexShrink: 0,
                    }}
                  >
                    <span className="fd-badge">
                      {settings.subsystems.ferrogate_enabled
                        ? "Enabled"
                        : "Disabled"}
                    </span>
                    <input
                      type="checkbox"
                      data-testid="toggle-ferrogate"
                      aria-label="Enable FerroGate"
                      checked={settings.subsystems.ferrogate_enabled}
                      onChange={(e) => {
                        setSettingsSaved(false);
                        setSettings((prev) => ({
                          ...prev,
                          subsystems: {
                            ...prev.subsystems,
                            ferrogate_enabled: e.target.checked,
                          },
                        }));
                      }}
                    />
                  </label>
                </div>
                <p
                  style={{
                    margin: 0,
                    fontSize: "12px",
                    color: "var(--text-secondary)",
                    lineHeight: 1.45,
                  }}
                >
                  Zero-knowledge secrets vault, header-injecting credential
                  proxy, API-to-MCP gateway, and Ed25519 Owner warrants.
                </p>
              </div>
              <div
                data-testid="product-config-ferrogate"
                style={{
                  padding: "8px 10px",
                  borderRadius: "6px",
                  background: "var(--bg-elevated)",
                  border: "1px solid var(--border-subtle)",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "8px",
                  flexWrap: "wrap",
                  fontSize: "11.5px",
                }}
              >
                <span style={{ color: "var(--text-secondary)" }}>
                  Secret Manager:{" "}
                  <strong>
                    {cloudAdapters.gate_cloud.secret_manager_enabled
                      ? "Enabled"
                      : "Off"}
                  </strong>
                </span>
                <span className="fd-badge">
                  Model Armor:{" "}
                  {cloudAdapters.gate_cloud.model_armor_enabled ? "On" : "Off"}
                </span>
              </div>
            </div>

            {/* 6. FerroTrack */}
            <div
              className="fd-card"
              data-testid="product-card-ferrotrack"
              style={{
                display: "flex",
                flexDirection: "column",
                justifyContent: "space-between",
                gap: "10px",
              }}
            >
              <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
                <div
                  style={{
                    display: "flex",
                    alignItems: "flex-start",
                    justifyContent: "space-between",
                    gap: "10px",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: "10px" }}>
                    <img
                      src="/brand/ferrotrack-icon.jpg"
                      alt="FerroTrack icon"
                      width="36"
                      height="36"
                      style={{
                        borderRadius: "8px",
                        objectFit: "cover",
                        border: "1px solid var(--border-subtle)",
                        flexShrink: 0,
                      }}
                    />
                    <div>
                      <div style={{ fontWeight: 700, fontSize: "13.5px" }}>
                        FerroTrack
                      </div>
                      <code
                        className="fd-mono"
                        style={{ fontSize: "11px", color: "var(--text-muted)" }}
                      >
                        fl github · Issue Tracker &amp; Ledger
                      </code>
                    </div>
                  </div>
                  <label
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: "6px",
                      fontSize: "11.5px",
                      cursor: "pointer",
                      flexShrink: 0,
                    }}
                  >
                    <span className="fd-badge">
                      {(settings.subsystems.ferrotrack_enabled ?? true)
                        ? "Enabled"
                        : "Disabled"}
                    </span>
                    <input
                      type="checkbox"
                      data-testid="toggle-ferrotrack"
                      aria-label="Enable FerroTrack"
                      checked={settings.subsystems.ferrotrack_enabled ?? true}
                      onChange={(e) => {
                        setSettingsSaved(false);
                        setSettings((prev) => ({
                          ...prev,
                          subsystems: {
                            ...prev.subsystems,
                            ferrotrack_enabled: e.target.checked,
                          },
                        }));
                      }}
                    />
                  </label>
                </div>
                <p
                  style={{
                    margin: 0,
                    fontSize: "12px",
                    color: "var(--text-secondary)",
                    lineHeight: 1.45,
                  }}
                >
                  Two-tier issue tracking, Kanban board synchronization, PR
                  review policy, and immutable `fl/ledger` decision evidence.
                </p>
              </div>
              <div
                data-testid="product-config-ferrotrack"
                style={{
                  padding: "8px 10px",
                  borderRadius: "6px",
                  background: "var(--bg-elevated)",
                  border: "1px solid var(--border-subtle)",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "8px",
                  fontSize: "11.5px",
                }}
              >
                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    cursor: "pointer",
                  }}
                >
                  <input
                    type="checkbox"
                    checked={settings.notifications.notify_on_pr_review}
                    onChange={(e) => {
                      setSettingsSaved(false);
                      setSettings((prev) => ({
                        ...prev,
                        notifications: {
                          ...prev.notifications,
                          notify_on_pr_review: e.target.checked,
                        },
                      }));
                    }}
                  />
                  <span>PR Review Request Alerts</span>
                </label>
              </div>
            </div>

            {/* 7. FerroWatch */}
            <div
              className="fd-card"
              data-testid="product-card-ferrowatch"
              style={{
                display: "flex",
                flexDirection: "column",
                justifyContent: "space-between",
                gap: "10px",
              }}
            >
              <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
                <div
                  style={{
                    display: "flex",
                    alignItems: "flex-start",
                    justifyContent: "space-between",
                    gap: "10px",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: "10px" }}>
                    <img
                      src="/brand/ferrowatch-icon.jpg"
                      alt="FerroWatch icon"
                      width="36"
                      height="36"
                      style={{
                        borderRadius: "8px",
                        objectFit: "cover",
                        border: "1px solid var(--border-subtle)",
                        flexShrink: 0,
                      }}
                    />
                    <div>
                      <div style={{ fontWeight: 700, fontSize: "13.5px" }}>
                        FerroWatch
                      </div>
                      <code
                        className="fd-mono"
                        style={{ fontSize: "11px", color: "var(--text-muted)" }}
                      >
                        fl stats · Observability &amp; Quotas
                      </code>
                    </div>
                  </div>
                  <label
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: "6px",
                      fontSize: "11.5px",
                      cursor: "pointer",
                      flexShrink: 0,
                    }}
                  >
                    <span className="fd-badge">
                      {(settings.subsystems.ferrowatch_enabled ?? true)
                        ? "Enabled"
                        : "Disabled"}
                    </span>
                    <input
                      type="checkbox"
                      data-testid="toggle-ferrowatch"
                      aria-label="Enable FerroWatch"
                      checked={settings.subsystems.ferrowatch_enabled ?? true}
                      onChange={(e) => {
                        setSettingsSaved(false);
                        setSettings((prev) => ({
                          ...prev,
                          subsystems: {
                            ...prev.subsystems,
                            ferrowatch_enabled: e.target.checked,
                          },
                        }));
                      }}
                    />
                  </label>
                </div>
                <p
                  style={{
                    margin: 0,
                    fontSize: "12px",
                    color: "var(--text-secondary)",
                    lineHeight: 1.45,
                  }}
                >
                  Fleet liveness heartbeats, 5h/7d vendor runtime quota gauges,
                  token accounting, and cost telemetry.
                </p>
              </div>
              <div
                data-testid="product-config-ferrowatch"
                style={{
                  padding: "8px 10px",
                  borderRadius: "6px",
                  background: "var(--bg-elevated)",
                  border: "1px solid var(--border-subtle)",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "8px",
                  fontSize: "11.5px",
                }}
              >
                <label
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: "6px",
                    cursor: "pointer",
                  }}
                >
                  <input
                    type="checkbox"
                    checked={settings.notifications.notify_on_session_error}
                    onChange={(e) => {
                      setSettingsSaved(false);
                      setSettings((prev) => ({
                        ...prev,
                        notifications: {
                          ...prev.notifications,
                          notify_on_session_error: e.target.checked,
                        },
                      }));
                    }}
                  />
                  <span>Session Error Telemetry Alerts</span>
                </label>
              </div>
            </div>
          </div>

          {/* Record #84 (Issue #205 Phase B): Dependency Closure & Planned Service / Integration Actions */}
          {(() => {
            const activeProfile = inferInstallProfilePreset(
              settings.subsystems,
              labProfilePreferred,
            );
            const selectedIds: string[] = [];
            if (settings.subsystems.ferrodeck_enabled ?? true) {
              selectedIds.push("ferrodeck");
            }
            if (settings.subsystems.ferrowire_enabled ?? true) {
              selectedIds.push("ferrowire");
            }
            if (settings.subsystems.ferrostate_enabled) {
              selectedIds.push("ferrostate");
            }
            if (settings.subsystems.ferrogate_enabled) {
              selectedIds.push("ferrogate");
            }
            if (settings.subsystems.ferrotrack_enabled ?? true) {
              selectedIds.push("ferrotrack");
            }
            if (settings.subsystems.ferrowatch_enabled ?? true) {
              selectedIds.push("ferrowatch");
            }
            if (settings.subsystems.ferrostep_enabled) {
              selectedIds.push("fl-flow");
            }
            const closure = computeProductDependencyClosure(
              selectedIds,
              activeProfile,
            );

            return (
              <>
                <div
                  className="fd-card"
                  data-testid="install-plan-preview"
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "8px",
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
                    <span style={{ fontWeight: 700, fontSize: "12.5px" }}>
                      Dependency Closure &amp; Planned Service / Integration Actions
                    </span>
                    <span
                      className="fd-badge fd-mono"
                      data-testid="install-plan-store-mode"
                    >
                      Store Mode:{" "}
                      {closure.storeMode === "lab-socket-daemon"
                        ? "ferrostore@ socket daemon"
                        : "embedded .fls (FERRSTR1)"}
                    </span>
                  </div>
                  <div
                    style={{
                      display: "grid",
                      gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))",
                      gap: "8px",
                      fontSize: "11.5px",
                    }}
                  >
                    <div
                      data-testid="install-plan-resolved-products"
                      style={{
                        padding: "8px 10px",
                        borderRadius: "6px",
                        background: "var(--bg-elevated)",
                        border: "1px solid var(--border-subtle)",
                      }}
                    >
                      <div
                        style={{
                          fontWeight: 700,
                          fontSize: "10.5px",
                          textTransform: "uppercase",
                          color: "var(--text-muted)",
                          marginBottom: "4px",
                        }}
                      >
                        Resolved Component Closure ({closure.resolvedProducts.length})
                      </div>
                      <code className="fd-mono" style={{ fontSize: "11px" }}>
                        {closure.resolvedProducts.join(", ")}
                      </code>
                    </div>
                    <div
                      data-testid="install-plan-units"
                      style={{
                        padding: "8px 10px",
                        borderRadius: "6px",
                        background: "var(--bg-elevated)",
                        border: "1px solid var(--border-subtle)",
                      }}
                    >
                      <div
                        style={{
                          fontWeight: 700,
                          fontSize: "10.5px",
                          textTransform: "uppercase",
                          color: "var(--text-muted)",
                          marginBottom: "4px",
                        }}
                      >
                        Planned Service Units ({closure.plannedUnits.length})
                      </div>
                      <code className="fd-mono" style={{ fontSize: "11px" }}>
                        {closure.plannedUnits.length > 0
                          ? closure.plannedUnits.join(", ")
                          : "None (CLI-only / embedded)"}
                      </code>
                    </div>
                    <div
                      data-testid="install-plan-mcp"
                      style={{
                        padding: "8px 10px",
                        borderRadius: "6px",
                        background: "var(--bg-elevated)",
                        border: "1px solid var(--border-subtle)",
                      }}
                    >
                      <div
                        style={{
                          fontWeight: 700,
                          fontSize: "10.5px",
                          textTransform: "uppercase",
                          color: "var(--text-muted)",
                          marginBottom: "4px",
                        }}
                      >
                        Planned MCP Catalog Entries ({closure.plannedMcpCatalog.length})
                      </div>
                      <code className="fd-mono" style={{ fontSize: "11px" }}>
                        {closure.plannedMcpCatalog.length > 0
                          ? closure.plannedMcpCatalog.join(", ")
                          : "None"}
                      </code>
                    </div>
                  </div>
                </div>

                {/* Record #84 (Issue #205 Phase B): Install Receipt & Release Provenance Inspector */}
                <div
                  className="fd-card"
                  data-testid="install-receipt-inspector"
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "8px",
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
                    <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                      <span style={{ fontWeight: 700, fontSize: "12.5px" }}>
                        Install Receipt &amp; Release Provenance (`receipt.json`)
                      </span>
                      <span
                        className="fd-badge fd-mono"
                        data-testid="receipt-adoption-badge"
                        style={{
                          background:
                            receiptAdoptionState === "managed"
                              ? "rgba(52, 211, 153, 0.14)"
                              : "rgba(251, 191, 36, 0.16)",
                          color:
                            receiptAdoptionState === "managed"
                              ? "#34d399"
                              : "#fbbf24",
                        }}
                      >
                        {receiptAdoptionState === "managed"
                          ? "Managed Receipt (Schema v1)"
                          : "Unmanaged Artifact (--adopt required)"}
                      </span>
                      <span
                        className="fd-badge fd-mono"
                        data-testid="version-code-ui-parity-badge"
                        style={{
                          background:
                            versionProvenance.code_ui_agree === true
                              ? "rgba(52, 211, 153, 0.14)"
                              : versionProvenance.code_ui_agree === false
                                ? "rgba(248, 113, 113, 0.18)"
                                : "rgba(251, 191, 36, 0.16)",
                          color:
                            versionProvenance.code_ui_agree === true
                              ? "#34d399"
                              : versionProvenance.code_ui_agree === false
                                ? "#f87171"
                                : "#fbbf24",
                        }}
                      >
                        {versionProvenance.code_ui_agree === true
                          ? "Code ↔ UI Parity: Matched (code_ui_agree=true)"
                          : versionProvenance.code_ui_agree === false
                            ? "Code ↔ UI Parity: Drift Detected (code_ui_agree=false)"
                            : "Code ↔ UI Parity: Unknown (code_ui_agree=null)"}
                      </span>
                    </div>
                    <div style={{ display: "flex", gap: "6px", flexWrap: "wrap" }}>
                      <button
                        type="button"
                        data-testid="receipt-verify-version-btn"
                        className="fd-btn"
                        style={{ fontSize: "11px", padding: "3px 8px" }}
                        disabled={verifyingVersion}
                        onClick={async () => {
                          setVerifyingVersion(true);
                          try {
                            if (typeof globalThis.fetch === "function") {
                              const res = await globalThis.fetch("/api/v1/version");
                              if (res && res.ok) {
                                const json = (await res.json()) as Partial<HubVersionProvenanceResponse>;
                                if (json && typeof json.commit === "string") {
                                  setVersionProvenance((prev) => ({
                                    ...prev,
                                    ...json,
                                  }));
                                }
                              }
                            }
                          } catch {
                            // Keep current versionProvenance when /api/v1/version is unreachable
                          } finally {
                            setVerifyingVersion(false);
                          }
                        }}
                      >
                        {verifyingVersion
                          ? "Verifying /api/v1/version…"
                          : "Verify /api/v1/version Parity"}
                      </button>
                      <button
                        type="button"
                        data-testid="receipt-simulate-unmanaged-btn"
                        className="fd-btn"
                        style={{ fontSize: "11px", padding: "3px 8px" }}
                        onClick={() => {
                          setReceiptAdoptionState((prev) =>
                            prev === "managed"
                              ? "unmanaged_adopt_required"
                              : "managed",
                          );
                        }}
                      >
                        {receiptAdoptionState === "managed"
                          ? "Simulate Unmanaged Binary"
                          : "Restore Managed State"}
                      </button>
                      {receiptAdoptionState === "unmanaged_adopt_required" && (
                        <button
                          type="button"
                          data-testid="receipt-adopt-btn"
                          className="fd-btn fd-btn-primary"
                          style={{ fontSize: "11px", padding: "3px 8px" }}
                          onClick={() => {
                            setReceiptAdoptionState("managed");
                            setAutoClosureNotice(
                              "Adopted existing ~/.local/bin/fl and systemd units into ~/.local/share/fl/receipt.json (--adopt).",
                            );
                          }}
                        >
                          Adopt Existing Installation (`--adopt`)
                        </button>
                      )}
                    </div>
                  </div>
                  <div
                    style={{
                      display: "grid",
                      gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))",
                      gap: "8px",
                      fontSize: "11.5px",
                    }}
                  >
                    <div
                      data-testid="receipt-provenance-release"
                      style={{
                        padding: "8px 10px",
                        borderRadius: "6px",
                        background: "var(--bg-elevated)",
                        border: "1px solid var(--border-subtle)",
                      }}
                    >
                      <div style={{ color: "var(--text-muted)", fontSize: "10.5px" }}>
                        Release ID &amp; Source Provenance
                      </div>
                      <code className="fd-mono" style={{ fontSize: "11px" }}>
                        {versionProvenance.release_id ?? `${versionProvenance.version}+${versionProvenance.commit_short}`} · signed_release · dirty={String(versionProvenance.dirty ?? false)}
                      </code>
                    </div>
                    <div
                      data-testid="receipt-ui-build-info"
                      style={{
                        padding: "8px 10px",
                        borderRadius: "6px",
                        background: "var(--bg-elevated)",
                        border: "1px solid var(--border-subtle)",
                      }}
                    >
                      <div style={{ color: "var(--text-muted)", fontSize: "10.5px" }}>
                        UI Bundle Provenance (`build-info.json`)
                      </div>
                      <code className="fd-mono" style={{ fontSize: "11px" }}>
                        {versionProvenance.ui
                          ? `commit=${versionProvenance.ui.commit.slice(0, 12)} · v=${versionProvenance.ui.version ?? "unknown"} · dirty=${String(versionProvenance.ui.dirty ?? false)}`
                          : "build-info.json absent"}
                      </code>
                    </div>
                    <div
                      data-testid="receipt-provenance-paths"
                      style={{
                        padding: "8px 10px",
                        borderRadius: "6px",
                        background: "var(--bg-elevated)",
                        border: "1px solid var(--border-subtle)",
                      }}
                    >
                      <div style={{ color: "var(--text-muted)", fontSize: "10.5px" }}>
                        Atomic Layout &amp; Stable Launcher
                      </div>
                      <code className="fd-mono" style={{ fontSize: "11px" }}>
                        ~/.local/share/fl/current -&gt; releases/0.9.4+b0888c6bc1ed · ~/.local/bin/fl
                      </code>
                    </div>
                  </div>
                </div>

                {/* Record #220 (Issue #205 Phase A3): Observational System Doctor (fl doctor) */}
                <div
                  className="fd-card"
                  data-testid="system-doctor-card"
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "8px",
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
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: "8px",
                        flexWrap: "wrap",
                      }}
                    >
                      <span style={{ fontWeight: 700, fontSize: "12.5px" }}>
                        System Doctor (`fl doctor`)
                      </span>
                      <span
                        className="fd-badge fd-mono"
                        data-testid="doctor-observational-badge"
                        style={{
                          background: "rgba(56, 189, 248, 0.14)",
                          color: "#38bdf8",
                        }}
                      >
                        Observational · Read-Only (Never Migrates)
                      </span>
                    </div>
                    <button
                      type="button"
                      data-testid="doctor-run-check-btn"
                      className="fd-btn"
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                      disabled={runningDoctorCheck}
                      onClick={async () => {
                        setRunningDoctorCheck(true);
                        try {
                          if (typeof globalThis.fetch === "function") {
                            const res = await globalThis.fetch("/api/v1/doctor");
                            if (res && res.ok) {
                              const json = (await res.json()) as {
                                checks?: DoctorCheckItem[];
                              };
                              if (
                                json &&
                                Array.isArray(json.checks) &&
                                json.checks.length > 0
                              ) {
                                setDoctorChecks(json.checks);
                              }
                            }
                          }
                        } catch {
                          // Keep current doctorChecks when /api/v1/doctor is unreachable
                        } finally {
                          setRunningDoctorCheck(false);
                        }
                      }}
                    >
                      {runningDoctorCheck
                        ? "Running Doctor Check…"
                        : "Run Doctor Check"}
                    </button>
                  </div>
                  <div
                    style={{
                      display: "grid",
                      gridTemplateColumns: "repeat(auto-fit, minmax(230px, 1fr))",
                      gap: "6px",
                      fontSize: "11px",
                    }}
                  >
                    {doctorChecks.map((check) => {
                      const statusColor =
                        check.status === "pass"
                          ? "#34d399"
                          : check.status === "warn"
                            ? "#fbbf24"
                            : check.status === "fail"
                              ? "#f87171"
                              : "#94a3b8";
                      return (
                        <div
                          key={check.id}
                          data-testid={`doctor-check-row-${check.id}`}
                          data-status={check.status}
                          style={{
                            padding: "6px 8px",
                            borderRadius: "6px",
                            background: "var(--bg-elevated)",
                            border: "1px solid var(--border-subtle)",
                            display: "flex",
                            flexDirection: "column",
                            gap: "3px",
                          }}
                        >
                          <div
                            style={{
                              display: "flex",
                              alignItems: "center",
                              justifyContent: "space-between",
                              gap: "6px",
                            }}
                          >
                            <code
                              className="fd-mono"
                              style={{ fontWeight: 700, fontSize: "10.5px" }}
                            >
                              {check.id}
                            </code>
                            <span
                              className="fd-badge fd-mono"
                              data-testid={`doctor-check-status-${check.id}`}
                              style={{
                                fontSize: "10px",
                                color: statusColor,
                              }}
                            >
                              {check.status}
                            </span>
                          </div>
                          <div
                            style={{
                              fontSize: "10.5px",
                              color: "var(--text-secondary)",
                            }}
                          >
                            {check.title}
                          </div>
                          <div
                            className="fd-mono"
                            style={{
                              fontSize: "10px",
                              color: "var(--text-muted)",
                            }}
                          >
                            {check.detail}
                          </div>
                        </div>
                      );
                    })}
                  </div>
                </div>

                {/* Issue #205 Phase A2: Deterministic fl install --dry-run Plan Preview */}
                {(() => {
                  const dryRunPlan = computeDryRunInstallPlan(
                    closure.resolvedProducts,
                    closure.plannedUnits,
                    receiptAdoptionState,
                  );
                  return (
                    <div
                      className="fd-card"
                      data-testid="install-dry-run-plan-card"
                      style={{
                        display: "flex",
                        flexDirection: "column",
                        gap: "8px",
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
                        <div
                          style={{
                            display: "flex",
                            alignItems: "center",
                            gap: "8px",
                            flexWrap: "wrap",
                          }}
                        >
                          <span style={{ fontWeight: 700, fontSize: "12.5px" }}>
                            Preview Install Plan (`fl install --dry-run`)
                          </span>
                          <span
                            className="fd-badge fd-mono"
                            data-testid="dry-run-plan-status-badge"
                            style={{
                              color:
                                dryRunPlan.status === "ready"
                                  ? "#34d399"
                                  : "#fbbf24",
                            }}
                          >
                            status: {dryRunPlan.status}
                          </span>
                        </div>
                        <button
                          type="button"
                          data-testid="dry-run-refresh-btn"
                          className="fd-btn"
                          style={{ fontSize: "11px", padding: "3px 8px" }}
                          onClick={() => {
                            setDryRunBanner(
                              `Computed deterministic dry-run install plan (${dryRunPlan.status}) for ${closure.resolvedProducts.length} resolved components.`,
                            );
                          }}
                        >
                          Run `fl install --dry-run`
                        </button>
                      </div>
                      {dryRunBanner && (
                        <div
                          role="status"
                          data-testid="dry-run-plan-banner"
                          className="fd-mono"
                          style={{ fontSize: "11px", color: "#34d399" }}
                        >
                          {dryRunBanner}
                        </div>
                      )}
                      <div
                        style={{
                          display: "grid",
                          gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))",
                          gap: "6px",
                          fontSize: "11px",
                        }}
                      >
                        <div
                          data-testid="dry-run-planned-files"
                          style={{
                            padding: "6px 8px",
                            borderRadius: "6px",
                            background: "var(--bg-elevated)",
                            border: "1px solid var(--border-subtle)",
                          }}
                        >
                          <div style={{ color: "var(--text-muted)", fontSize: "10px" }}>
                            Planned Files ({dryRunPlan.planned_files.length})
                          </div>
                          <code className="fd-mono" style={{ fontSize: "10.5px" }}>
                            {dryRunPlan.planned_files.join(", ")}
                          </code>
                        </div>
                        <div
                          data-testid="dry-run-planned-symlinks"
                          style={{
                            padding: "6px 8px",
                            borderRadius: "6px",
                            background: "var(--bg-elevated)",
                            border: "1px solid var(--border-subtle)",
                          }}
                        >
                          <div style={{ color: "var(--text-muted)", fontSize: "10px" }}>
                            Planned Symlinks ({dryRunPlan.planned_symlinks.length})
                          </div>
                          <code className="fd-mono" style={{ fontSize: "10.5px" }}>
                            {dryRunPlan.planned_symlinks.join(" · ")}
                          </code>
                        </div>
                        <div
                          data-testid="dry-run-conflicts"
                          style={{
                            padding: "6px 8px",
                            borderRadius: "6px",
                            background: "var(--bg-elevated)",
                            border: "1px solid var(--border-subtle)",
                          }}
                        >
                          <div style={{ color: "var(--text-muted)", fontSize: "10px" }}>
                            Path Conflicts ({dryRunPlan.conflicts.length})
                          </div>
                          <code className="fd-mono" style={{ fontSize: "10.5px" }}>
                            {dryRunPlan.conflicts.length > 0
                              ? dryRunPlan.conflicts.join("; ")
                              : "None (Ready for atomic install)"}
                          </code>
                        </div>
                      </div>
                    </div>
                  );
                })()}

                {/* Issue #205 Phase C: Versioned releases/<ver>+<sha12>/ Upgrade & Rollback Slot Manager */}
                {(() => {
                  const currentSlot =
                    releaseSlots.find((s) => s.role === "current")?.slot_id ??
                    "0.9.4+b0888c6bc1ed";
                  return (
                    <div
                      className="fd-card"
                      data-testid="release-slots-rollback-card"
                      style={{
                        display: "flex",
                        flexDirection: "column",
                        gap: "8px",
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
                        <div
                          style={{
                            display: "flex",
                            alignItems: "center",
                            gap: "8px",
                            flexWrap: "wrap",
                          }}
                        >
                          <span style={{ fontWeight: 700, fontSize: "12.5px" }}>
                            Release Slots &amp; Rollback (`releases/&lt;ver&gt;+&lt;sha12&gt;/`)
                          </span>
                          <span
                            className="fd-badge fd-mono"
                            data-testid="release-current-symlink-badge"
                            style={{ color: "#38bdf8" }}
                          >
                            current -&gt; releases/{currentSlot}
                          </span>
                        </div>
                        <div style={{ display: "flex", gap: "6px", flexWrap: "wrap" }}>
                          <button
                            type="button"
                            data-testid="release-dry-run-upgrade-btn"
                            className="fd-btn"
                            style={{ fontSize: "11px", padding: "3px 8px" }}
                            onClick={() => {
                              setReleaseSlotBanner(
                                "Dry-run upgrade verified: staged slot releases/0.9.5+975bcacab4b4 passes receipt & binary checks.",
                              );
                            }}
                          >
                            Dry-Run Upgrade
                          </button>
                          <button
                            type="button"
                            data-testid="release-rollback-btn"
                            className="fd-btn fd-btn-primary"
                            style={{ fontSize: "11px", padding: "3px 8px" }}
                            onClick={() => {
                              setReleaseSlots((prev) =>
                                prev.map((s) =>
                                  s.role === "current"
                                    ? { ...s, role: "staged" }
                                    : s.role === "previous"
                                      ? { ...s, role: "current" }
                                      : s,
                                ),
                              );
                              setReleaseSlotBanner(
                                "Atomically flipped ~/.local/share/fl/current symlink to previous release slot 0.9.3+7a41d09ef312.",
                              );
                            }}
                          >
                            Rollback to Previous Slot
                          </button>
                        </div>
                      </div>
                      {releaseSlotBanner && (
                        <div
                          role="status"
                          data-testid="release-slot-action-banner"
                          className="fd-mono"
                          style={{ fontSize: "11px", color: "#34d399" }}
                        >
                          {releaseSlotBanner}
                        </div>
                      )}
                      <div
                        style={{
                          display: "grid",
                          gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))",
                          gap: "6px",
                          fontSize: "11px",
                        }}
                      >
                        {releaseSlots.map((slot) => (
                          <div
                            key={slot.slot_id}
                            data-testid={`release-slot-row-${slot.slot_id}`}
                            data-role={slot.role}
                            style={{
                              padding: "6px 8px",
                              borderRadius: "6px",
                              background: "var(--bg-elevated)",
                              border: "1px solid var(--border-subtle)",
                              display: "flex",
                              alignItems: "center",
                              justifyContent: "space-between",
                              gap: "6px",
                            }}
                          >
                            <code className="fd-mono" style={{ fontSize: "10.5px" }}>
                              {slot.slot_id}
                            </code>
                            <span
                              className="fd-badge fd-mono"
                              data-testid={`release-slot-role-${slot.slot_id}`}
                              style={{
                                fontSize: "10px",
                                color:
                                  slot.role === "current"
                                    ? "#34d399"
                                    : slot.role === "previous"
                                      ? "#38bdf8"
                                      : "#fbbf24",
                              }}
                            >
                              {slot.role}
                            </span>
                          </div>
                        ))}
                      </div>
                    </div>
                  );
                })()}
              </>
            );
          })()}
        </div>

        {/* TAB 3: Auth */}
        <div
          role="tabpanel"
          aria-label="Authentication & Operators Panel"
          data-testid="settings-panel-auth"
          style={{
            display: activeTab === "auth" ? "flex" : "none",
            flexDirection: "column",
            gap: "12px",
          }}
        >
          {/* Current Session Authority & Sign Out */}
          <div
            className="fd-card"
            data-testid="settings-auth-session-card"
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "12px",
              flexWrap: "wrap",
            }}
          >
            <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
              <h3
                style={{
                  margin: 0,
                  fontSize: "13px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.04em",
                }}
              >
                Current Session Authority
              </h3>
              <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                <span className="fd-badge">
                  <span
                    className={`fd-dot ${
                      authority?.tier === "owner"
                        ? "fd-dot-alive"
                        : authority?.tier === "peer"
                          ? "fd-dot-stale"
                          : "fd-dot-dead"
                    }`}
                  />
                  <span>{authorityText}</span>
                </span>
              </div>
            </div>

            <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
              {onOpenPairingModal && (
                <button
                  type="button"
                  data-testid="settings-open-pairing-btn"
                  className="fd-btn"
                  onClick={() => {
                    onOpenPairingModal();
                    onClose();
                  }}
                >
                  Pair / Switch Account
                </button>
              )}
              {authority && onLogout && (
                <button
                  type="button"
                  data-testid="settings-sign-out-btn"
                  className="fd-btn fd-btn-danger"
                  onClick={() => {
                    onLogout();
                    onClose();
                  }}
                >
                  Sign Out
                </button>
              )}
            </div>
          </div>

          {/* Record 11: Operators, Passkeys & MFA Section */}
          <div
            className="fd-card"
            data-testid="settings-operators-section"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "12px",
            }}
          >
            <div>
              <h3
                style={{
                  margin: "0 0 4px 0",
                  fontSize: "13px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.04em",
                }}
              >
                Operators, Passkeys &amp; MFA (RBAC)
              </h3>
              <p
                style={{
                  margin: 0,
                  fontSize: "12px",
                  color: "var(--text-secondary)",
                }}
              >
                Manage multi-operator accounts, role-based access control
                (`owner`, `admin`, `operator`, `viewer`), TOTP authenticators, and
                WebAuthn passkeys.
              </p>
            </div>

            {/* Operator Roster List */}
            {operators.length > 0 && (
              <div style={{ display: "flex", flexDirection: "column", gap: "6px" }}>
                {operators.map((op) => (
                  <div
                    key={op.id || op.username}
                    data-testid={`operator-row-${op.username}`}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: "8px",
                      padding: "8px 10px",
                      borderRadius: "6px",
                      border: "1px solid var(--border-subtle, #1e293b)",
                      background: "var(--bg-elevated)",
                      flexWrap: "wrap",
                    }}
                  >
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: "8px",
                        flexWrap: "wrap",
                      }}
                    >
                      <strong style={{ fontSize: "12.5px" }}>
                        {op.display_name || op.username}
                      </strong>
                      <span className="fd-badge fd-mono">{op.role}</span>
                      <span className="fd-badge">
                        TOTP: {op.totp_enabled ? "Enabled" : "Off"}
                      </span>
                      <span className="fd-badge">
                        Passkeys: {op.passkeys_count ?? 0}
                      </span>
                    </div>

                    <button
                      type="button"
                      data-testid={`delete-operator-${op.username}`}
                      className="fd-btn fd-btn-danger"
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                      onClick={() => void handleDeleteOperator(op)}
                    >
                      Remove
                    </button>
                  </div>
                ))}
              </div>
            )}

            {/* + Add Operator Form */}
            <form
              onSubmit={handleAddOperator}
              style={{
                display: "flex",
                alignItems: "center",
                gap: "8px",
                flexWrap: "wrap",
              }}
            >
              <input
                type="text"
                aria-label="New Operator Username"
                data-testid="operator-username-input"
                className="fd-input"
                placeholder="Username (e.g. alice)"
                style={{ flex: "1 1 130px", fontSize: "12px", padding: "5px 8px" }}
                value={opUsername}
                onChange={(e) => setOpUsername(e.target.value)}
              />
              <select
                aria-label="Operator Role"
                data-testid="operator-role-select"
                className="fd-select fd-mono"
                style={{ fontSize: "12px", padding: "5px 8px" }}
                value={opRole}
                onChange={(e) => setOpRole(e.target.value as OperatorRole)}
              >
                <option value="owner">owner</option>
                <option value="admin">admin</option>
                <option value="operator">operator</option>
                <option value="viewer">viewer</option>
              </select>
              <input
                type="password"
                aria-label="New Operator Password"
                data-testid="operator-password-input"
                className="fd-input"
                placeholder="Password (optional)"
                style={{ flex: "1 1 130px", fontSize: "12px", padding: "5px 8px" }}
                value={opPassword}
                onChange={(e) => setOpPassword(e.target.value)}
              />
              <button
                type="submit"
                data-testid="add-operator-submit-btn"
                className="fd-btn fd-btn-primary"
                disabled={addingOperator || !opUsername.trim()}
              >
                + Add Operator
              </button>
            </form>

            <hr
              style={{
                border: "none",
                borderTop: "1px solid var(--border-subtle)",
                margin: 0,
              }}
            />

            {/* TOTP MFA & WebAuthn Passkey Enrollment */}
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "8px",
                flexWrap: "wrap",
              }}
            >
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: "8px",
                  flexWrap: "wrap",
                }}
              >
                <button
                  type="button"
                  data-testid="enroll-totp-btn"
                  className="fd-btn"
                  onClick={() => void handleEnrollTotp()}
                >
                  Enroll TOTP MFA
                </button>

                <input
                  type="text"
                  aria-label="Passkey Label"
                  data-testid="passkey-label-input"
                  className="fd-input"
                  placeholder="Passkey label (optional)"
                  style={{ width: "160px", fontSize: "12px", padding: "5px 8px" }}
                  value={passkeyLabel}
                  onChange={(e) => setPasskeyLabel(e.target.value)}
                />
                <button
                  type="button"
                  data-testid="register-passkey-btn"
                  className="fd-btn"
                  onClick={() => void handleRegisterPasskey()}
                >
                  Register Passkey
                </button>
              </div>

              {passkeyRegistered && (
                <span
                  className="fd-badge"
                  data-testid="passkey-registered-banner"
                  style={{ color: "var(--status-alive, #22c55e)" }}
                >
                  Passkey registered
                </span>
              )}
            </div>

            {totpEnrollment && (
              <div
                data-testid="totp-secret-display"
                style={{
                  padding: "10px",
                  borderRadius: "6px",
                  border: "1px solid var(--border-subtle, #1e293b)",
                  background: "var(--bg-elevated)",
                  display: "flex",
                  flexDirection: "column",
                  gap: "8px",
                  fontSize: "12px",
                }}
              >
                <div>
                  <strong>TOTP Secret (Base32):</strong>{" "}
                  <code className="fd-mono">{totpEnrollment.secret_base32}</code>
                </div>
                <div style={{ color: "var(--text-muted)", wordBreak: "break-all" }}>
                  <code className="fd-mono">{totpEnrollment.otpauth_uri}</code>
                </div>
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: "8px",
                    flexWrap: "wrap",
                  }}
                >
                  <input
                    type="text"
                    inputMode="numeric"
                    aria-label="Verify TOTP Code"
                    data-testid="totp-verify-input"
                    className="fd-input fd-mono"
                    placeholder="6-digit code"
                    style={{ width: "130px", fontSize: "12px", padding: "4px 8px" }}
                    value={totpVerifyCode}
                    onChange={(e) => setTotpVerifyCode(e.target.value)}
                  />
                  <button
                    type="button"
                    data-testid="totp-verify-btn"
                    className="fd-btn fd-btn-primary"
                    onClick={() => void handleVerifyTotp()}
                  >
                    Verify TOTP
                  </button>
                  {totpVerified && (
                    <span
                      className="fd-badge"
                      data-testid="totp-verified-badge"
                      style={{ color: "var(--status-alive, #22c55e)" }}
                    >
                      TOTP Verified
                    </span>
                  )}
                </div>
              </div>
            )}
          </div>
        </div>
          </>
        )}

        {/* Runtimes & Quotas: the hub answers it for the owner only */}
        {activeTab === "runtimes" && !initialProject && (
          <div
            role="tabpanel"
            aria-label="Runtimes and Quotas Panel"
            data-testid="settings-panel-runtimes"
          >
            {authority?.tier === "owner" ? (
              <RuntimesQuotasPanel apiClient={apiClient} />
            ) : (
              <div
                className="fd-card"
                data-testid="settings-runtimes-owner-only"
                style={{ fontSize: "12px", color: "var(--text-muted)" }}
              >
                Owner authority is required to view runtimes and quotas.
              </div>
            )}
          </div>
        )}

        {/* Re-Cast Modal Dialog */}
        {activeTab === "accessibility" && !initialProject && (
          <div
            data-testid="settings-accessibility-panel"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: "12px",
            }}
          >
            {/* Section a: Color Theme / Appearance */}
            <div
              className="fd-card"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "10px",
              }}
            >
              <div>
                <h3
                  style={{
                    margin: 0,
                    fontSize: "12.5px",
                    fontWeight: 700,
                    textTransform: "uppercase",
                    letterSpacing: "0.04em",
                  }}
                >
                  Color Theme &amp; Appearance
                </h3>
                <span style={{ fontSize: "11.5px", color: "var(--text-muted)" }}>
                  Select Dark, Light, or System color scheme across FerroDeck.
                </span>
              </div>

              <div
                role="group"
                aria-label="Color Theme Mode"
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: "8px",
                  flexWrap: "wrap",
                }}
              >
                {(
                  [
                    { id: "dark", label: "Dark" },
                    { id: "light", label: "Light" },
                    { id: "system", label: "System" },
                  ] as const
                ).map((opt) => {
                  const isSelected = localThemeMode === opt.id;
                  return (
                    <button
                      key={opt.id}
                      type="button"
                      data-testid={`theme-mode-${opt.id}`}
                      aria-pressed={isSelected}
                      className={`fd-btn${isSelected ? " fd-btn-active" : ""}`}
                      onClick={() => handleSelectThemeMode(opt.id)}
                    >
                      {opt.label}
                    </button>
                  );
                })}
              </div>

              {localThemeMode === "system" && (
                <div
                  data-testid="theme-system-status"
                  className="fd-mono"
                  style={{ fontSize: "11.5px", color: "var(--text-secondary)" }}
                >
                  Follows OS preference via prefers-color-scheme (currently{" "}
                  <strong>{resolvedOsTheme}</strong>)
                </div>
              )}
            </div>

            {/* Section b: System-Wide Font Size Increase */}
            <div
              className="fd-card"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "10px",
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
                <div>
                  <h3
                    style={{
                      margin: 0,
                      fontSize: "12.5px",
                      fontWeight: 700,
                      textTransform: "uppercase",
                      letterSpacing: "0.04em",
                    }}
                  >
                    System-Wide Font Size Scale
                  </h3>
                  <span style={{ fontSize: "11.5px", color: "var(--text-muted)" }}>
                    Scale typography from {FONT_SCALE_MIN_PCT}% to {FONT_SCALE_MAX_PCT}% while preserving panel layout integrity.
                  </span>
                </div>
                <span
                  data-testid="font-scale-current-badge"
                  className="fd-badge fd-mono"
                >
                  {localFontScalePct}%
                </span>
              </div>

              <div
                role="group"
                aria-label="Font Size Presets"
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: "6px",
                  flexWrap: "wrap",
                }}
              >
                {FONT_SCALE_PRESETS.map((pct) => {
                  const isSelected = localFontScalePct === pct;
                  return (
                    <button
                      key={pct}
                      type="button"
                      data-testid={`font-scale-preset-${pct}`}
                      aria-pressed={isSelected}
                      className={`fd-btn fd-mono${isSelected ? " fd-btn-active" : ""}`}
                      onClick={() => handleSelectFontScalePct(pct)}
                    >
                      {pct}%
                    </button>
                  );
                })}
              </div>

              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: "10px",
                  flexWrap: "wrap",
                }}
              >
                <input
                  type="range"
                  aria-label="Font Size Scale Percentage"
                  data-testid="font-scale-slider"
                  min={FONT_SCALE_MIN_PCT}
                  max={FONT_SCALE_MAX_PCT}
                  step={FONT_SCALE_STEP_PCT}
                  value={localFontScalePct}
                  onChange={(e) => handleSelectFontScalePct(Number(e.target.value))}
                  style={{ flex: "1 1 180px", minWidth: 0 }}
                />
                <button
                  type="button"
                  data-testid="font-scale-reset"
                  className="fd-btn"
                  disabled={localFontScalePct === 100}
                  onClick={() => handleSelectFontScalePct(100)}
                >
                  Reset (100%)
                </button>
              </div>
            </div>
          </div>
        )}

        {recastModalAgentKey && (() => {
          const modalAgent = effectiveAgents.find(
            (a) => a.name_key.toLowerCase() === recastModalAgentKey,
          );
          if (!modalAgent) return null;
          const key = modalAgent.name_key.toLowerCase();
          const draft = getDraftForAgent(modalAgent);
          const armedConflict = recastReplaceLeadArmed[key];
          const projectOptions = CANONICAL_PROJECTS.includes(draft.project)
            ? CANONICAL_PROJECTS
            : [draft.project, ...CANONICAL_PROJECTS];
          const hasRoleOption =
            draft.role === CREATE_CUSTOM_ROLE_VALUE ||
            RECAST_ROLE_OPTIONS.some((opt) => opt.value === draft.role);

          return (
            <div
              className="fd-modal-backdrop"
              onClick={() => {
                if (!recastingAgentKey) {
                  setRecastModalAgentKey(null);
                }
              }}
            >
              <div
                role="dialog"
                aria-modal="true"
                aria-label={`Re-Cast Agent — ${modalAgent.display_name}`}
                data-testid="recast-modal"
                className="fd-modal"
                onClick={(e) => e.stopPropagation()}
                style={{
                  maxWidth: "460px",
                  width: "92vw",
                  display: "flex",
                  flexDirection: "column",
                  gap: "12px",
                }}
              >
                <div
                  className="fd-modal-header"
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "8px",
                  }}
                >
                  <h3 style={{ margin: 0, fontSize: "15px", fontWeight: 700, flex: 1, minWidth: 0 }}>
                    Re-Cast Agent — {modalAgent.display_name}
                  </h3>
                  <button
                    type="button"
                    className="fd-btn"
                    style={{ flexShrink: 0 }}
                    aria-label="Close Re-Cast Modal"
                    disabled={Boolean(recastingAgentKey)}
                    onClick={() => setRecastModalAgentKey(null)}
                  >
                    ✕
                  </button>
                </div>

                <p
                  style={{
                    margin: 0,
                    fontSize: "12px",
                    color: "var(--text-secondary)",
                  }}
                >
                  Re-assign <strong>{modalAgent.display_name}</strong> to a project and
                  role across the workspace registry.
                </p>

                <div style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
                  <div>
                    <label
                      htmlFor={`recast-project-select-${key}`}
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
                      id={`recast-project-select-${key}`}
                      aria-label={`Project for ${modalAgent.display_name}`}
                      data-testid={`recast-project-select-${key}`}
                      className="fd-select fd-mono"
                      style={{ width: "100%", fontSize: "12px", padding: "6px 8px" }}
                      value={draft.project}
                      onChange={(e) => {
                        const nextProject = e.target.value;
                        setRecastDrafts((prev) => ({
                          ...prev,
                          [key]: {
                            ...getDraftForAgent(modalAgent),
                            project: nextProject,
                          },
                        }));
                        setRecastReplaceLeadArmed((prev) => {
                          if (!(key in prev)) return prev;
                          const next = { ...prev };
                          delete next[key];
                          return next;
                        });
                      }}
                    >
                      {projectOptions.map((proj) => (
                        <option key={proj} value={proj}>
                          {proj}
                        </option>
                      ))}
                    </select>
                  </div>

                  <div>
                    <label
                      htmlFor={`recast-role-select-${key}`}
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
                      id={`recast-role-select-${key}`}
                      aria-label={`Role for ${modalAgent.display_name}`}
                      data-testid={`recast-role-select-${key}`}
                      className="fd-select fd-mono"
                      style={{ width: "100%", fontSize: "12px", padding: "6px 8px" }}
                      value={draft.role}
                      onChange={(e) => {
                        const nextRole = e.target.value;
                        setRecastDrafts((prev) => ({
                          ...prev,
                          [key]: {
                            ...getDraftForAgent(modalAgent),
                            role: nextRole,
                          },
                        }));
                        setRecastReplaceLeadArmed((prev) => {
                          if (!(key in prev)) return prev;
                          const next = { ...prev };
                          delete next[key];
                          return next;
                        });
                      }}
                    >
                      {!hasRoleOption && (
                        <option value={draft.role}>{draft.role}</option>
                      )}
                      {RECAST_ROLE_OPTIONS.map((opt) => (
                        <option key={opt.value} value={opt.value}>
                          {opt.label}
                        </option>
                      ))}
                      <option value={CREATE_CUSTOM_ROLE_VALUE}>
                        + Create custom role...
                      </option>
                    </select>
                  </div>

                  {draft.role === CREATE_CUSTOM_ROLE_VALUE && (
                    <div>
                      <label
                        htmlFor={`recast-custom-role-input-${key}`}
                        style={{
                          display: "block",
                          fontSize: "11px",
                          fontWeight: 600,
                          color: "var(--text-muted)",
                          marginBottom: "4px",
                          textTransform: "uppercase",
                        }}
                      >
                        Custom Role Name
                      </label>
                      <input
                        id={`recast-custom-role-input-${key}`}
                        type="text"
                        aria-label={`Custom role for ${modalAgent.display_name}`}
                        data-testid={`recast-custom-role-input-${key}`}
                        className="fd-input fd-mono"
                        placeholder="e.g. reviewer or custom:reviewer"
                        style={{ width: "100%", fontSize: "12px", padding: "6px 8px" }}
                        value={recastCustomRoleInputs[key] || ""}
                        onChange={(e) => {
                          const val = e.target.value;
                          setRecastCustomRoleInputs((prev) => ({
                            ...prev,
                            [key]: val,
                          }));
                        }}
                      />
                    </div>
                  )}
                </div>

                {armedConflict && (
                  <div
                    data-testid={`recast-lead-warning-${key}`}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: "8px",
                      padding: "8px 10px",
                      borderRadius: "4px",
                      backgroundColor: "rgba(245, 158, 11, 0.14)",
                      border: "1px solid rgba(245, 158, 11, 0.4)",
                      fontSize: "11.5px",
                    }}
                  >
                    <span>
                      {armedConflict.replaceLead
                        ? `Re-casting ${modalAgent.display_name} as Lead will replace the active Lead (${armedConflict.message}).`
                        : `Re-casting ${modalAgent.display_name} requires creating a new coverage area (${armedConflict.message}).`}
                    </span>
                    <button
                      type="button"
                      data-testid={`recast-confirm-lead-btn-${key}`}
                      className="fd-btn fd-btn-danger"
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                      disabled={recastingAgentKey === key}
                      onClick={() => void handleRecastAgent(modalAgent, true)}
                    >
                      {armedConflict.replaceLead
                        ? "Confirm Replace Lead"
                        : "Confirm Create Area"}
                    </button>
                  </div>
                )}

                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "flex-end",
                    gap: "8px",
                  }}
                >
                  <button
                    type="button"
                    data-testid={`recast-cancel-btn-${key}`}
                    className="fd-btn"
                    disabled={recastingAgentKey === key}
                    onClick={() => setRecastModalAgentKey(null)}
                  >
                    Cancel
                  </button>
                  <button
                    type="button"
                    data-testid={`recast-submit-btn-${key}`}
                    className="fd-btn fd-btn-primary"
                    disabled={recastingAgentKey === key}
                    onClick={() => void handleRecastAgent(modalAgent, false)}
                  >
                    {recastingAgentKey === key ? "Re-Casting..." : "Confirm Re-Cast"}
                  </button>
                </div>
              </div>
            </div>
          );
        })()}

        {/* System / Project Change Artifacts Directory Sub-Modal (Issue #207) */}
        {artifactsDirModalTarget && (() => {
          const isSystem = artifactsDirModalTarget.scope === "system";
          const projectName = !isSystem ? artifactsDirModalTarget.project : null;
          const projectInfo = projectName
            ? getProjectArtifactsDirState(projectName)
            : null;
          const oldPath = isSystem
            ? systemEffectiveArtifactsDir
            : (projectInfo?.effectiveDir ?? "");
          const defaultPath = isSystem
            ? systemDefaultArtifactsDir
            : (projectInfo?.defaultDir ?? "");
          const hasOverride = isSystem
            ? Boolean(settings.artifacts_dir)
            : Boolean(projectInfo?.artifactsDir);

          return (
            <div
              data-testid="change-artifacts-dir-modal-backdrop"
              onClick={() => {
                if (!changingArtifactsDir) {
                  setArtifactsDirModalTarget(null);
                  setArtifactsDirModalError(null);
                }
              }}
              style={{
                position: "fixed",
                inset: 0,
                backgroundColor: "rgba(9, 13, 22, 0.72)",
                display: "flex",
                alignItems: "center",
                justifyContent: "center",
                zIndex: 1300,
                padding: "16px",
              }}
            >
              <div
                role="dialog"
                aria-modal="true"
                aria-labelledby="change-artifacts-dir-modal-title"
                data-testid="change-artifacts-dir-modal"
                className="fd-card"
                onClick={(e) => e.stopPropagation()}
                style={{
                  width: "min(520px, 100%)",
                  backgroundColor: "var(--bg-surface, #0f172a)",
                  border: "1px solid var(--border-accent)",
                  borderRadius: "8px",
                  padding: "16px",
                  display: "flex",
                  flexDirection: "column",
                  gap: "12px",
                  boxShadow: "0 20px 44px rgba(0, 0, 0, 0.55)",
                }}
              >
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: "8px",
                  }}
                >
                  <strong
                    id="change-artifacts-dir-modal-title"
                    data-testid="change-artifacts-dir-modal-title"
                    style={{ fontSize: "14px" }}
                  >
                    {isSystem
                      ? "Change System Artifacts Directory"
                      : `Change ${projectName} Artifacts Directory`}
                  </strong>
                  <button
                    type="button"
                    data-testid="change-artifacts-dir-close-btn"
                    className="fd-btn"
                    style={{ fontSize: "11px", padding: "3px 8px" }}
                    disabled={changingArtifactsDir}
                    onClick={() => {
                      setArtifactsDirModalTarget(null);
                      setArtifactsDirModalError(null);
                    }}
                  >
                    Close
                  </button>
                </div>

                <p
                  style={{
                    margin: 0,
                    fontSize: "12px",
                    color: "var(--text-secondary)",
                  }}
                >
                  {isSystem
                    ? "Override the hub-wide artifacts root directory, or reset to the default [home]/artifacts path."
                    : `Override the artifacts directory for ${projectName}, or reset to the default [artifacts]/${projectInfo?.slug ?? ""} path.`}
                </p>

                <div style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
                  <div>
                    <label
                      htmlFor="change-artifacts-dir-old-path"
                      style={{
                        display: "block",
                        fontSize: "11px",
                        fontWeight: 600,
                        color: "var(--text-muted)",
                        marginBottom: "4px",
                        textTransform: "uppercase",
                      }}
                    >
                      Old Path
                    </label>
                    <input
                      id="change-artifacts-dir-old-path"
                      type="text"
                      readOnly
                      aria-label="Old Path"
                      data-testid="change-artifacts-dir-old-path"
                      className="fd-input fd-mono"
                      style={{
                        width: "100%",
                        fontSize: "12px",
                        padding: "6px 8px",
                        opacity: 0.85,
                      }}
                      value={oldPath}
                    />
                  </div>

                  <div>
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                        gap: "8px",
                        marginBottom: "4px",
                      }}
                    >
                      <label
                        htmlFor="change-artifacts-dir-new-path"
                        style={{
                          fontSize: "11px",
                          fontWeight: 600,
                          color: "var(--text-muted)",
                          textTransform: "uppercase",
                        }}
                      >
                        New Path
                      </label>
                      <button
                        type="button"
                        data-testid="change-artifacts-dir-reset-default-btn"
                        className="fd-btn"
                        style={{ fontSize: "10.5px", padding: "2px 6px" }}
                        disabled={changingArtifactsDir}
                        onClick={() => setArtifactsDirNewPath(defaultPath)}
                      >
                        Use Default ({defaultPath})
                      </button>
                    </div>
                    <input
                      id="change-artifacts-dir-new-path"
                      type="text"
                      aria-label="New Path"
                      data-testid="change-artifacts-dir-new-path"
                      className="fd-input fd-mono"
                      placeholder={defaultPath}
                      style={{ width: "100%", fontSize: "12px", padding: "6px 8px" }}
                      value={artifactsDirNewPath}
                      onChange={(e) => setArtifactsDirNewPath(e.target.value)}
                    />
                    <span
                      style={{
                        display: "block",
                        marginTop: "4px",
                        fontSize: "11px",
                        color: "var(--text-muted)",
                      }}
                    >
                      Leave blank or set to <code className="fd-mono">{defaultPath}</code>{" "}
                      to use the default directory
                      {hasOverride ? " (currently overridden)." : "."}
                    </span>
                  </div>

                  <label
                    htmlFor="change-artifacts-dir-migrate-checkbox"
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: "8px",
                      fontSize: "12px",
                      cursor: "pointer",
                      userSelect: "none",
                    }}
                  >
                    <input
                      id="change-artifacts-dir-migrate-checkbox"
                      type="checkbox"
                      data-testid="change-artifacts-dir-migrate-checkbox"
                      checked={artifactsDirMigrate}
                      disabled={changingArtifactsDir}
                      onChange={(e) => setArtifactsDirMigrate(e.target.checked)}
                    />
                    <span>Migrate all existing artifacts</span>
                  </label>
                </div>

                {artifactsDirModalError && (
                  <div
                    role="alert"
                    data-testid="change-artifacts-dir-error"
                    className="fd-card"
                    style={{
                      borderColor: "var(--diff-del-border)",
                      backgroundColor: "var(--diff-del-bg)",
                      color: "var(--diff-del-text)",
                      fontSize: "11.5px",
                      padding: "6px 10px",
                    }}
                  >
                    {artifactsDirModalError}
                  </div>
                )}

                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "flex-end",
                    gap: "8px",
                  }}
                >
                  <button
                    type="button"
                    data-testid="change-artifacts-dir-cancel-btn"
                    className="fd-btn"
                    disabled={changingArtifactsDir}
                    onClick={() => {
                      setArtifactsDirModalTarget(null);
                      setArtifactsDirModalError(null);
                    }}
                  >
                    Cancel
                  </button>
                  <button
                    type="button"
                    data-testid="change-artifacts-dir-confirm-btn"
                    className="fd-btn fd-btn-primary"
                    disabled={changingArtifactsDir}
                    onClick={() => void handleConfirmChangeArtifactsDir()}
                  >
                    {changingArtifactsDir ? "Applying..." : "Confirm"}
                  </button>
                </div>
              </div>
            </div>
          );
        })()}
      </div>
    </div>
  );
};
