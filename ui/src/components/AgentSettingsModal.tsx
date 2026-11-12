import React, { useEffect, useRef, useState } from "react";
import { HubApiClient, HubApiError } from "../api/client";
import { roleLabel, roleOptions, selectedRoleOptions, useRoles } from "../hooks/useRoles";
import type { HubAuthority, ProjectSummary, SessionQuotaUsage } from "../types/api";
import { projectChoices } from "../hooks/useProjects";
import {
  isLiveSessionStatus,
  PinIcon,
  togglePinnedAgent,
  usePinnedState,
} from "./RosterSidebar";
import { evaluateQuotaStatus, resolveQuotaPoolKey } from "../utils/quotaStatus";
import {
  canonicalizeDefaultRole,
  CREATE_CUSTOM_ROLE_VALUE,
  extractRecastConflict,
  normalizeRecastRole,
  RECAST_ROLE_OPTIONS,
  type RecastConflictState,
} from "./SettingsModal";
import {
  formatModelOptionLabel,
  contextWindowSourceTitle,
  formatContextMax,
  contextAtCompactThreshold,
  formatTokenCount,
  getVendorModelOptions,
  resolveSessionVendor,
  canonicalAdapter,
  isAntigravityAdapter,
  agyModelRequiresThinkingLevel,
  type EstimatedContextUsage,
} from "./ChatPane";
import {
  DEFAULT_RULE_PRESET_MATRICES,
  DEFAULT_UNIVERSAL_PERMISSION_MATRICES,
  UNIVERSAL_PERMISSION_CAPABILITIES,
  buildCanonicalPermissionPolicy,
  canonicalDefaultEffect,
  compileUniversalPermissions,
  computeWorktreeBootstrapPreview,
  resolvePermissionChangeClass,
  resolveVendorPermissionPreview,
  type CanonicalPermissionPolicy,
  type CanonicalPermissionRule,
  type CanonicalRulePattern,
  type PermissionChangeClass,
  type PermissionCompileReceipt,
  type PermissionRuleAction,
  type ProtectedPathClass,
  type UnenforcedDefaultsMode,
  type UniversalPermissionCapability,
  type UniversalPermissionMatrix,
  type UniversalPermissionMode,
  type UniversalPermissionPosture,
  type UniversalRulePreset,
  type UniversalSandboxMode,
  type WorktreePolicyMode,
} from "./SpinUpAgentModal";

export {
  DEFAULT_RULE_PRESET_MATRICES,
  DEFAULT_UNIVERSAL_PERMISSION_MATRICES,
  UNIVERSAL_PERMISSION_CAPABILITIES,
  buildCanonicalPermissionPolicy,
  canonicalDefaultEffect,
  compileUniversalPermissions,
  resolvePermissionChangeClass,
  resolveVendorPermissionPreview,
  type CanonicalPermissionPolicy,
  type CanonicalPermissionRule,
  type CanonicalRulePattern,
  type PermissionChangeClass,
  type PermissionCompileReceipt,
  type PermissionRuleAction,
  type ProtectedPathClass,
  type UnenforcedDefaultsMode,
  type UniversalPermissionCapability,
  type UniversalPermissionMatrix,
  type UniversalPermissionMode,
  type UniversalPermissionPosture,
  type UniversalRulePreset,
  type UniversalSandboxMode,
};

/**
 * Record #82 (Issue #217): Native `fl host` supervisor telemetry per agent.
 */
export type NativeHostTransportMode = "pipe-rpc" | "pty-vt100" | "legacy-tmux";

export interface AgentHostSupervisorTelemetry {
  agent: string;
  transportMode: NativeHostTransportMode;
  routeAuthority: "FerroWire Store";
  ringBufferUsedBytes: number;
  ringBufferCapacityBytes: number;
  supervisorUnit: string;
}

export function resolveAgentHostSupervisorTelemetry(
  adapter?: string | null,
  agentName?: string | null,
): AgentHostSupervisorTelemetry {
  const key = (adapter ?? "").trim().toLowerCase();
  const agent = (agentName ?? "agent").trim().toLowerCase() || "agent";
  const transportMode: NativeHostTransportMode =
    key.includes("claude") ||
    key.includes("codex") ||
    key.includes("antigravity") ||
    key.includes("agy") ||
    key.includes("acp")
      ? "pipe-rpc"
      : key.includes("tmux")
        ? "legacy-tmux"
        : "pty-vt100";
  return {
    agent,
    transportMode,
    routeAuthority: "FerroWire Store",
    ringBufferUsedBytes: 131072,
    ringBufferCapacityBytes: 1048576,
    supervisorUnit: `fl-host@${agent}.service`,
  };
}

const EXECUTION_MODE_LABELS: Readonly<Record<string, string>> = {
  auto: "Auto",
  yolo: "Yolo",
  plan: "Plan",
  default: "Default",
  "accept-edits": "Accept edits",
};

/** Display label for the hub's normalized execution mode; a vendor value the hub passed through is shown as is. */
export function formatExecutionMode(mode?: string | null): string {
  const raw = (mode ?? "").trim();
  if (!raw) return "Unknown";
  return EXECUTION_MODE_LABELS[raw.toLowerCase()] ?? raw;
}

export const THINKING_LEVEL_OPTIONS = [
  { value: "low", label: "Fast" },
  { value: "medium", label: "Balanced" },
  { value: "high", label: "High — Default" },
  { value: "xhigh", label: "Maximum Reasoning" },
] as const;

export const CANONICAL_DESIRED_MCP_SERVERS = [
  "fl-state",
  "agent-bus",
] as const;

export type PostureDriftKind =
  | "synced"
  | "unapplied_catalog"
  | "stale_session"
  | "extra_server";

export interface AgentMcpPostureInput {
  agentName?: string | null;
  project?: string | null;
  role?: string | null;
  adapter?: string | null;
  sessionId?: string | null;
  desiredMcpServers?: string[];
  appliedMcpServers?: string[];
  liveMcpServers?: string[];
  sessionStartedBeforeSync?: boolean;
  sessionRole?: string | null;
  sessionProject?: string | null;
}

export interface AgentMcpPostureReport {
  agentKey: string;
  driftKind: PostureDriftKind;
  badgeLabel: string;
  summary: string;
  vendorConfigPath: string;
  desiredServers: string[];
  appliedServers: string[];
  liveServers: string[];
  missingInApplied: string[];
  missingInLive: string[];
  extraInAppliedOrLive: string[];
  remediationLabel: string;
}

export function resolveVendorConfigPath(adapter?: string | null): string {
  const key = (adapter ?? "").trim().toLowerCase();
  if (key.includes("claude")) return "~/.claude.json";
  if (key.includes("antigravity") || key.includes("gemini")) {
    return "~/.gemini/settings.json";
  }
  if (key.includes("codex")) return "~/.codex/config.toml";
  if (key.includes("grok")) return "~/.grok/settings.json";
  if (key.includes("muse")) return "~/.muse/config.json";
  return "~/.config/fl/mcp-vendors.json";
}

export function evaluateAgentMcpPosture(
  input: AgentMcpPostureInput,
): AgentMcpPostureReport {
  const agentKey = (input.agentName || input.sessionId || "agent")
    .trim()
    .toLowerCase();
  const vendorConfigPath = resolveVendorConfigPath(input.adapter);
  const desiredServers = Array.from(
    new Set(
      (input.desiredMcpServers && input.desiredMcpServers.length > 0
        ? input.desiredMcpServers
        : [...CANONICAL_DESIRED_MCP_SERVERS]
      ).map((s) => s.trim()),
    ),
  ).filter(Boolean);

  const appliedServers = Array.from(
    new Set(
      (input.appliedMcpServers !== undefined
        ? input.appliedMcpServers
        : desiredServers
      ).map((s) => s.trim()),
    ),
  ).filter(Boolean);

  const hasLiveSession = Boolean((input.sessionId ?? "").trim());
  const liveServers = Array.from(
    new Set(
      (input.liveMcpServers !== undefined
        ? input.liveMcpServers
        : hasLiveSession
          ? appliedServers
          : desiredServers
      ).map((s) => s.trim()),
    ),
  ).filter(Boolean);

  const desiredSet = new Set(desiredServers);
  const appliedSet = new Set(appliedServers);
  const liveSet = new Set(liveServers);

  const missingInApplied = desiredServers.filter((s) => !appliedSet.has(s));
  const missingInLive = appliedServers.filter((s) => !liveSet.has(s));
  const extraInAppliedOrLive = Array.from(
    new Set([
      ...appliedServers.filter((s) => !desiredSet.has(s)),
      ...liveServers.filter((s) => !desiredSet.has(s)),
    ]),
  );

  const roleMismatch =
    Boolean(input.role && input.sessionRole) &&
    canonicalizeDefaultRole(input.role ?? "") !==
      canonicalizeDefaultRole(input.sessionRole ?? "");
  const projectMismatch =
    Boolean(input.project && input.sessionProject) &&
    input.project!.trim().toLowerCase() !==
      input.sessionProject!.trim().toLowerCase();

  if (missingInApplied.length > 0) {
    return {
      agentKey,
      driftKind: "unapplied_catalog",
      badgeLabel: "Drift: Unapplied Catalog Change",
      summary: `Desired catalog defines [${missingInApplied.join(", ")}] not yet synced to ${vendorConfigPath}.`,
      vendorConfigPath,
      desiredServers,
      appliedServers,
      liveServers,
      missingInApplied,
      missingInLive,
      extraInAppliedOrLive,
      remediationLabel: "Sync MCP Config",
    };
  }

  if (extraInAppliedOrLive.length > 0) {
    return {
      agentKey,
      driftKind: "extra_server",
      badgeLabel: "Drift: Unregistered Extra Server",
      summary: `Unregistered MCP server(s) [${extraInAppliedOrLive.join(", ")}] present outside Desired catalog.`,
      vendorConfigPath,
      desiredServers,
      appliedServers,
      liveServers,
      missingInApplied,
      missingInLive,
      extraInAppliedOrLive,
      remediationLabel: "Prune Extra Servers",
    };
  }

  if (
    input.sessionStartedBeforeSync === true ||
    missingInLive.length > 0 ||
    roleMismatch ||
    projectMismatch
  ) {
    const reasons: string[] = [];
    if (missingInLive.length > 0) {
      reasons.push(`live session missing [${missingInLive.join(", ")}]`);
    }
    if (input.sessionStartedBeforeSync) {
      reasons.push("session started before last MCP config sync");
    }
    if (roleMismatch || projectMismatch) {
      reasons.push("live session role/project predates latest registry assignment");
    }
    return {
      agentKey,
      driftKind: "stale_session",
      badgeLabel: "Drift: Stale Session (Needs Reforge)",
      summary: `Applied config matches Desired catalog, but ${reasons.join("; ") || "live session needs reforge"}.`,
      vendorConfigPath,
      desiredServers,
      appliedServers,
      liveServers,
      missingInApplied,
      missingInLive,
      extraInAppliedOrLive,
      remediationLabel: "Reforge / Restart Session",
    };
  }

  return {
    agentKey,
    driftKind: "synced",
    badgeLabel: "3-Tier Posture: Synced",
    summary: `Desired catalog (${desiredServers.length} MCP), Applied config (${vendorConfigPath}), and Live-Attested session are in sync.`,
    vendorConfigPath,
    desiredServers,
    appliedServers,
    liveServers,
    missingInApplied,
    missingInLive,
    extraInAppliedOrLive,
    remediationLabel: "Verify Posture",
  };
}

export interface AgentSettingsModalProps {
  isOpen: boolean;
  onClose: () => void;
  agentName?: string | null;
  agentDisplayName?: string | null;
  agentRole?: string | null;
  agentProject?: string | null;
  adapter?: string | null;
  sessionId?: string | null;
  selectedModel: string;
  selectedThinkingLevel: string;
  availableModels?: string[] | null;
  switchingModel?: boolean;
  onChangeModelAndThinking: (nextModel: string, nextThinkingLevel: string) => void;
  contextEstimate: EstimatedContextUsage;
  /**
   * Session usage allowance telemetry (`primary_window`, `secondary_window`, `exhausted_reason`,
   * `observed_at_ms`) evaluated alongside liveness in the Status & Allowance detail block.
   */
  quotaUsage?: SessionQuotaUsage | null;
  /**
   * Process liveness state (`alive`, `stale`, `dead`) so the Status & Allowance detail can show
   * both process liveness and usage allowance without conflating them.
   */
  livenessState?: "alive" | "stale" | "dead" | null;
  /**
   * Effective execution mode the hub reports for the session (`auto`, `yolo`, `plan`, `default`,
   * `accept-edits`, or a vendor value). `null` means the hub reported none ("Unknown"). When the
   * prop is omitted the modal reads the mode from `GET /api/v1/sessions` through `apiClient`.
   */
  executionMode?: string | null;
  turnCount?: number;
  canCompact: boolean;
  compacting: boolean;
  onCompactContext: () => void;
  canClear: boolean;
  clearing: boolean;
  onOpenClearModal: () => void;
  canDecommission: boolean;
  decommissioning: boolean;
  onOpenDecommissionModal: () => void;
  apiClient?: HubApiClient;
  onAgentsUpdated?: () => void;
  postureInput?: Partial<AgentMcpPostureInput>;
  onRemediatePosture?: (
    action: "sync_mcp" | "reforge_session" | "prune_extra",
    report: AgentMcpPostureReport,
  ) => void;
  initialTab?: "overview" | "permissions" | "runtime";
  /** Record #109: Breadcrumb label and callback to return to originating SettingsModal (Project Settings or Fleet Settings). */
  returnToSettingsLabel?: string | null;
  onReturnToSettings?: () => void;
  /** The shared project list for the Re-Cast project dropdown. Defaults to the built-in list. */
  projects?: readonly ProjectSummary[];
  /**
   * Who is operating FerroDeck. The context window can be chosen only by an owner, so the
   * selector is absent unless `authority.tier` is `owner` (the hub refuses a peer anyway).
   */
  authority?: HubAuthority | null;
}

/** What the last context window request answered; shown in the status line or the alert. */
type ContextWindowNotice =
  | { kind: "ok"; requested: number; method: "operator-prompt" | "stored"; persisted: boolean }
  | { kind: "error"; text: string }
  | { kind: "timed-out" };

/** An operator's pick, kept over the reported window until the hub reports a different one. */
interface ContextWindowPick {
  value: number;
  /** The `max_tokens` the hub reported when the operator picked. */
  reportedAtPick: number | null;
}

/** How long a prompt sent to the session may go without a changed `max_tokens` report. */
const CONTEXT_WINDOW_PICK_TIMEOUT_MS = 25_000;
const CONTEXT_WINDOW_STATUS_ID = "fd-agent-settings-context-window-status";
const CONTEXT_WINDOW_ERROR_ID = "fd-agent-settings-context-window-error";

function vendorDisplayName(vendor: string): string {
  const name = canonicalAdapter(vendor);
  return name ? name.charAt(0).toUpperCase() + name.slice(1) : "";
}

export type AgentSettingsTabId = "overview" | "permissions" | "runtime";

interface FetchedSessionState {
  executionMode: string | null;
  adapter: string | null;
  model: string | null;
  thinkingLevel: string | null;
  availableModels: string[] | null;
  quotaUsage: SessionQuotaUsage | null;
  status: string | null;
  /** Prop values in effect when the fetch was issued. */
  props: SessionPropSnapshot;
}

interface SessionPropSnapshot {
  adapter?: string | null;
  model?: string;
  thinkingLevel?: string;
  executionMode?: string | null;
  quotaUsage?: SessionQuotaUsage | null;
}

/** Fields whose prop value is newer than the fetched session value. */
interface OverrideLatch {
  model: boolean;
  thinkingLevel: boolean;
  executionMode: boolean;
  adapter: boolean;
  quotaUsage: boolean;
}

const NO_OVERRIDES: OverrideLatch = {
  model: false,
  thinkingLevel: false,
  executionMode: false,
  adapter: false,
  quotaUsage: false,
};

function sameModalQuotaUsage(
  a: SessionQuotaUsage | null | undefined,
  b: SessionQuotaUsage | null | undefined,
): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  return (
    a.tier === b.tier &&
    a.observed_at_ms === b.observed_at_ms &&
    a.exhausted_reason === b.exhausted_reason &&
    a.primary_window?.used_percent === b.primary_window?.used_percent &&
    a.primary_window?.window_minutes === b.primary_window?.window_minutes &&
    a.primary_window?.resets_at_ms === b.primary_window?.resets_at_ms &&
    a.secondary_window?.used_percent === b.secondary_window?.used_percent &&
    a.secondary_window?.window_minutes === b.secondary_window?.window_minutes &&
    a.secondary_window?.resets_at_ms === b.secondary_window?.resets_at_ms
  );
}

export const AgentSettingsModal: React.FC<AgentSettingsModalProps> = ({
  isOpen,
  onClose,
  agentName,
  agentDisplayName,
  agentRole,
  agentProject,
  adapter,
  sessionId,
  selectedModel,
  selectedThinkingLevel,
  availableModels,
  switchingModel = false,
  onChangeModelAndThinking,
  contextEstimate,
  quotaUsage,
  livenessState,
  executionMode,
  turnCount,
  canCompact,
  compacting,
  onCompactContext,
  canClear,
  clearing,
  onOpenClearModal,
  canDecommission,
  decommissioning,
  onOpenDecommissionModal,
  apiClient,
  onAgentsUpdated,
  postureInput,
  onRemediatePosture,
  initialTab = "overview",
  returnToSettingsLabel = null,
  onReturnToSettings,
  projects = [],
  authority = null,
}) => {
  const { pinnedAgents } = usePinnedState();
  const [activeTab, setActiveTab] = useState<AgentSettingsTabId>(initialTab);
  const [agentWorktreePolicy, setAgentWorktreePolicy] =
    useState<WorktreePolicyMode>("create");
  const [agentPosture, setAgentPosture] =
    useState<UniversalPermissionPosture>("ask");
  const [agentSandbox, setAgentSandbox] =
    useState<UniversalSandboxMode>("vendor-default");
  const [agentRulePreset, setAgentRulePreset] =
    useState<UniversalRulePreset>("workspace");
  const [agentPermissionMode, setAgentPermissionMode] =
    useState<UniversalPermissionMode>("auto-accept");
  const [agentPermissionMatrix, setAgentPermissionMatrix] =
    useState<UniversalPermissionMatrix>(
      () => DEFAULT_RULE_PRESET_MATRICES.workspace,
    );
  const [agentRequireFsEnforcement, setAgentRequireFsEnforcement] =
    useState(false);
  const [agentUnenforcedDefaults, setAgentUnenforcedDefaults] =
    useState<UnenforcedDefaultsMode>("refuse");
  const [agentPermissionSavedStatus, setAgentPermissionSavedStatus] =
    useState<string | null>(null);
  const roleCatalog = useRoles(isOpen, apiClient);
  const recastRoleOptions = roleOptions(RECAST_ROLE_OPTIONS, roleCatalog.roles);
  const [recastModalOpen, setRecastModalOpen] = useState(false);
  const [recastProject, setRecastProject] = useState(agentProject || "Workspace");
  const [recastRole, setRecastRole] = useState(
    canonicalizeDefaultRole(agentRole || "assistant"),
  );
  const [recastCustomRoleInput, setRecastCustomRoleInput] = useState("");
  const [recastConflict, setRecastConflict] = useState<RecastConflictState | null>(null);
  const [recasting, setRecasting] = useState(false);
  const [recastError, setRecastError] = useState<string | null>(null);
  const [recastStatusBanner, setRecastStatusBanner] = useState<string | null>(null);
  const [postureOverride, setPostureOverride] =
    useState<Partial<AgentMcpPostureInput> | null>(null);
  const [postureActionStatus, setPostureActionStatus] = useState<string | null>(null);
  // Context window choice. `contextWindowPick` is the same idea as the model latch below: the
  // operator's pick stays on screen (also while the request is in flight and when a stale
  // `contextEstimate` arrives) until the hub reports a different `max_tokens`.
  const [contextWindowPick, setContextWindowPick] = useState<ContextWindowPick | null>(null);
  const [contextWindowSending, setContextWindowSending] = useState(false);
  const [contextWindowNotice, setContextWindowNotice] =
    useState<ContextWindowNotice | null>(null);
  const [pendingAgyModel, setPendingAgyModel] = useState<string | null>(null);
  const contextWindowSelectRef = useRef<HTMLSelectElement | null>(null);
  const contextWindowRequestRef = useRef(0);
  const contextWindowRefocusRef = useRef(false);
  const reportedMaxTokens = contextEstimate.maxTokens;

  useEffect(() => {
    // A new target or a reopen drops the pick; a request still in flight is ignored on return.
    contextWindowRequestRef.current += 1;
    setContextWindowPick(null);
    setContextWindowSending(false);
    setContextWindowNotice(null);
    setPendingAgyModel(null);
  }, [isOpen, sessionId, agentName, selectedModel, selectedThinkingLevel]);
  useEffect(() => {
    // The hub reported a different window: follow the server from here on.
    setContextWindowPick((prev) =>
      prev && reportedMaxTokens !== prev.reportedAtPick ? null : prev,
    );
    // A "still reports X" message is about the old window.
    setContextWindowNotice((prev) => (prev?.kind === "timed-out" ? null : prev));
  }, [reportedMaxTokens]);
  const contextWindowAwaitingReport =
    contextWindowPick !== null &&
    contextWindowNotice?.kind === "ok" &&
    contextWindowNotice.method === "operator-prompt";
  useEffect(() => {
    // Bound the wait: if Grok refuses silently or the paste is lost, `max_tokens` never changes.
    if (!contextWindowAwaitingReport) return;
    const timer = setTimeout(() => {
      setContextWindowPick(null);
      setContextWindowNotice({ kind: "timed-out" });
    }, CONTEXT_WINDOW_PICK_TIMEOUT_MS);
    return () => clearTimeout(timer);
  }, [contextWindowAwaitingReport, contextWindowPick]);
  useEffect(() => {
    // A disabled select loses focus; give it back once the call is over, unless the operator
    // has moved to another field meanwhile.
    if (!contextWindowSending && contextWindowRefocusRef.current) {
      contextWindowRefocusRef.current = false;
      const select = contextWindowSelectRef.current;
      const active = document.activeElement;
      if (select && (active === null || active === document.body || active === select)) {
        select.focus();
      }
    }
  }, [contextWindowSending]);

  useEffect(() => {
    setRecastProject(agentProject || "Workspace");
    setRecastRole(canonicalizeDefaultRole(agentRole || "assistant"));
    setRecastConflict(null);
    setRecastError(null);
    setRecastStatusBanner(null);
    setRecastModalOpen(false);
    setPostureOverride(null);
    setPostureActionStatus(null);
    setActiveTab(initialTab);
  }, [isOpen, agentName, agentProject, agentRole, initialTab]);
  // Session metadata read from the hub every time the modal opens. The hub may
  // change a session's model, effort or mode behind the UI's back (an operator
  // override can be superseded by what the vendor reports), so the session list
  // is the source of truth and the props are only the initial values. `props`
  // records the prop values at the moment the request was issued. A field is
  // latched as overridden the first time its prop differs from that snapshot
  // (the operator switched it, even while the request was in flight) or the
  // operator changes it in this modal; it then keeps showing the prop until the
  // modal reopens, the target changes or a new fetch is issued.
  const [fetchedSession, setFetchedSession] = useState<FetchedSessionState | null>(null);
  const [overridden, setOverridden] = useState<OverrideLatch>(NO_OVERRIDES);
  const latestPropsRef = useRef<SessionPropSnapshot & { apiClient?: HubApiClient }>({
    adapter,
    model: selectedModel,
    thinkingLevel: selectedThinkingLevel,
    executionMode,
    quotaUsage,
  });
  latestPropsRef.current = {
    adapter,
    model: selectedModel,
    thinkingLevel: selectedThinkingLevel,
    executionMode,
    quotaUsage,
    apiClient,
  };
  const requestSnapshotRef = useRef<SessionPropSnapshot>({
    adapter,
    model: selectedModel,
    thinkingLevel: selectedThinkingLevel,
    executionMode,
    quotaUsage,
  });
  const canFetchSessions =
    Boolean(apiClient) && typeof apiClient?.getSessions === "function";
  const quotaSourceRef = useRef<{
    poolKey: string;
    quotaUsage: SessionQuotaUsage | null | undefined;
    staleAcrossPoolSwitch: boolean;
  }>({
    poolKey: "",
    quotaUsage: undefined,
    staleAcrossPoolSwitch: false,
  });
  useEffect(() => {
    setFetchedSession(null);
    const { apiClient: client, ...propsAtRequest } = latestPropsRef.current;
    requestSnapshotRef.current = propsAtRequest;
    quotaSourceRef.current = {
      poolKey: "",
      quotaUsage: undefined,
      staleAcrossPoolSwitch: false,
    };
    setOverridden((prev) => (prev === NO_OVERRIDES ? prev : NO_OVERRIDES));
    if (!isOpen || !canFetchSessions || !client || (!sessionId && !agentName)) {
      return;
    }
    let cancelled = false;
    Promise.resolve()
      .then(() => client.getSessions())
      .then((res) => {
        if (cancelled) return;
        const sessions = res?.sessions ?? [];
        const wanted = agentName?.trim().toLowerCase();
        const match =
          (sessionId ? sessions.find((s) => s.id === sessionId) : undefined) ??
          (wanted ? sessions.find((s) => s.persona?.trim().toLowerCase() === wanted) : undefined);
        setFetchedSession({
          executionMode: match?.execution_mode ?? null,
          adapter: match?.adapter ?? null,
          model: match?.model ?? null,
          thinkingLevel: match?.thinking_level ?? null,
          availableModels: match?.available_models ?? null,
          quotaUsage: match?.quota_usage ?? null,
          status: match?.status ?? null,
          props: propsAtRequest,
        });
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [isOpen, canFetchSessions, sessionId, agentName]);
  // Declared after the fetch effect so that a new fetch resets the snapshot
  // before this compares against it.
  useEffect(() => {
    if (!isOpen) return;
    const snap = requestSnapshotRef.current;
    const changed: OverrideLatch = {
      model: selectedModel !== snap.model,
      thinkingLevel: selectedThinkingLevel !== snap.thinkingLevel,
      executionMode: executionMode !== snap.executionMode,
      adapter: adapter !== snap.adapter,
      quotaUsage: !sameModalQuotaUsage(quotaUsage, snap.quotaUsage),
    };
    setOverridden((prev) => {
      const next = {
        model: prev.model || changed.model,
        thinkingLevel: prev.thinkingLevel || changed.thinkingLevel,
        executionMode: prev.executionMode || changed.executionMode,
        adapter: prev.adapter || changed.adapter,
        quotaUsage: prev.quotaUsage || changed.quotaUsage,
      };
      return next.model === prev.model &&
        next.thinkingLevel === prev.thinkingLevel &&
        next.executionMode === prev.executionMode &&
        next.adapter === prev.adapter &&
        next.quotaUsage === prev.quotaUsage
        ? prev
        : next;
    });
  }, [isOpen, adapter, selectedModel, selectedThinkingLevel, executionMode, quotaUsage]);
  useEffect(() => {
    if (!isOpen) return;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        if (recastModalOpen) {
          if (!recasting) {
            setRecastModalOpen(false);
          }
          return;
        }
        onClose();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [isOpen, recastModalOpen, recasting, onClose]);

  // A field is overridden once latched, and also in the render where its prop
  // first differs from the request snapshot (before the latch effect has run).
  const isOverridden = <K extends keyof OverrideLatch>(
    key: K,
    current: SessionPropSnapshot[K],
  ): boolean =>
    overridden[key] ||
    (fetchedSession !== null && current !== fetchedSession.props[key]);
  const fetchedFor = <K extends keyof OverrideLatch>(
    key: K,
    current: SessionPropSnapshot[K],
    fetchedValue: string | null | undefined,
  ): string | null => {
    const value = (fetchedValue ?? "").trim();
    if (!fetchedSession || !value) return null;
    return isOverridden(key, current) ? null : value;
  };
  const rawSelectedModel =
    fetchedFor("model", selectedModel, fetchedSession?.model) ?? (selectedModel ?? "").trim();
  const rawPropAdapter = (adapter ?? "").trim();
  const rawFetchedAdapter = fetchedSession?.adapter?.trim() ?? "";
  const chosenAdapter =
    rawFetchedAdapter &&
    rawFetchedAdapter.toLowerCase() !== "unknown" &&
    !isOverridden("adapter", adapter)
      ? rawFetchedAdapter
      : rawPropAdapter;
  const knownAdapter = resolveSessionVendor(chosenAdapter, rawSelectedModel);
  const effectiveAdapter = knownAdapter || chosenAdapter || null;
  const effectiveAvailableModels =
    fetchedSession?.availableModels && fetchedSession.availableModels.length > 0
      ? fetchedSession.availableModels
      : availableModels;
  const candidateModel = rawSelectedModel;
  const isStaleAntigravityModel =
    !isAntigravityAdapter(knownAdapter) &&
    candidateModel.toLowerCase().startsWith("gemini-") &&
    (!Array.isArray(effectiveAvailableModels) ||
      !effectiveAvailableModels.includes(candidateModel));
  const sanitizedCurrentModel = isStaleAntigravityModel ? "" : candidateModel;
  const modelOptions = getVendorModelOptions(
    knownAdapter || effectiveAdapter,
    sanitizedCurrentModel,
    effectiveAvailableModels,
  );
  const effectiveModel = sanitizedCurrentModel || modelOptions[0] || "";
  const shownModel = pendingAgyModel || effectiveModel;
  const isQuotaOverridden =
    overridden.quotaUsage ||
    !sameModalQuotaUsage(
      quotaUsage,
      fetchedSession !== null ? fetchedSession.props.quotaUsage : requestSnapshotRef.current.quotaUsage,
    );
  const isAdapterOverridden =
    isOverridden("adapter", adapter) ||
    adapter !== requestSnapshotRef.current.adapter;
  const sessionVendor = canonicalAdapter(
    (fetchedSession?.adapter && fetchedSession.adapter.trim().toLowerCase() !== "unknown"
      ? fetchedSession.adapter
      : requestSnapshotRef.current.adapter) ?? "",
  );
  const effectiveVendor = canonicalAdapter(effectiveAdapter ?? "");
  const currentPoolKey = resolveQuotaPoolKey(effectiveAdapter, effectiveModel);
  const candidateQuotaUsage = (() => {
    const fetchedQuota = fetchedSession?.quotaUsage ?? null;
    const propQuota = quotaUsage ?? null;
    if (propQuota && fetchedQuota) {
      const propTs = propQuota.observed_at_ms;
      const fetchedTs = fetchedQuota.observed_at_ms;
      if (
        typeof propTs === "number" &&
        Number.isFinite(propTs) &&
        typeof fetchedTs === "number" &&
        Number.isFinite(fetchedTs) &&
        propTs !== fetchedTs
      ) {
        return propTs > fetchedTs ? propQuota : fetchedQuota;
      }
      return isQuotaOverridden ? propQuota : fetchedQuota;
    }
    if (fetchedSession !== null && !isQuotaOverridden) {
      return fetchedQuota ?? propQuota;
    }
    return propQuota ?? fetchedQuota;
  })();

  useEffect(() => {
    if (!isOpen) return;
    const prev = quotaSourceRef.current;
    if (!prev.poolKey) {
      quotaSourceRef.current = {
        poolKey: currentPoolKey,
        quotaUsage: candidateQuotaUsage,
        staleAcrossPoolSwitch: false,
      };
      return;
    }
    const sameQuota = sameModalQuotaUsage(candidateQuotaUsage, prev.quotaUsage);
    if (currentPoolKey !== prev.poolKey) {
      quotaSourceRef.current = {
        poolKey: currentPoolKey,
        quotaUsage: candidateQuotaUsage,
        staleAcrossPoolSwitch: sameQuota,
      };
    } else if (!sameQuota) {
      quotaSourceRef.current = {
        poolKey: currentPoolKey,
        quotaUsage: candidateQuotaUsage,
        staleAcrossPoolSwitch: false,
      };
    }
  }, [isOpen, currentPoolKey, candidateQuotaUsage]);

  if (!isOpen) {
    return null;
  }

  const displayTarget = agentDisplayName || agentName || sessionId || "Agent";
  const agentKey = (agentName || displayTarget).trim().toLowerCase();
  const targetSessionOrAgent = sessionId || agentName || "";
  const effectiveThinkingLevel =
    fetchedFor("thinkingLevel", selectedThinkingLevel, fetchedSession?.thinkingLevel) ??
    selectedThinkingLevel;
  const effectiveThinking = (effectiveThinkingLevel ?? "").trim().toLowerCase();
  const isAgySession = isAntigravityAdapter(knownAdapter || effectiveAdapter);
  const agyThinkingRequired =
    isAgySession &&
    !effectiveThinking &&
    agyModelRequiresThinkingLevel(shownModel, effectiveAvailableModels);
  const contextVendor = canonicalAdapter(knownAdapter);
  const offeredContextWindows = contextEstimate.offeredWindows ?? [];
  const contextWindowEditable =
    contextVendor === "grok" &&
    authority?.tier === "owner" &&
    Boolean(sessionId) &&
    offeredContextWindows.length >= 2;
  // The pick stands while the hub still reports the window it reported at pick time.
  const activeContextWindowPick =
    contextWindowPick && reportedMaxTokens === contextWindowPick.reportedAtPick
      ? contextWindowPick
      : null;
  const shownContextWindow = activeContextWindowPick?.value ?? reportedMaxTokens;
  const contextWindowSizeText =
    reportedMaxTokens === null
      ? "not reported"
      : formatContextMax(reportedMaxTokens, contextEstimate.source);
  const contextWindowWaiting =
    contextWindowNotice?.kind === "ok" &&
    contextWindowNotice.method === "operator-prompt" &&
    activeContextWindowPick !== null &&
    activeContextWindowPick.value !== reportedMaxTokens;
  const contextWindowStatusText =
    contextWindowNotice?.kind === "timed-out"
      ? reportedMaxTokens === null
        ? "The session has not reported a window. The change may not have been applied."
        : `The session still reports ${contextWindowSizeText}. The change may not have been applied.`
      : contextWindowNotice?.kind !== "ok"
      ? ""
      : contextWindowNotice.method === "stored"
        ? "Saved for this agent. It is not applied to the running session (its terminal is not open)."
        : `Requested ${formatTokenCount(contextWindowNotice.requested)}. Sent to the session; it reports the window it actually uses.${
            contextWindowNotice.persisted ? "" : " Not saved: it will not survive a restart."
          }`;

  const handleRecastSubmit = async (confirmOverrides = false) => {
    if (recasting || !agentKey) return;
    const effectiveRole = normalizeRecastRole(recastRole, recastCustomRoleInput);
    setRecasting(true);
    setRecastError(null);
    setRecastStatusBanner(null);
    try {
      const client = apiClient ?? new HubApiClient();
      // Send what the select shows: the slug of the matched project, else the raw value.
      const res = await client.recastAgent(agentKey, {
        project: projectChoices(projects, recastProject).value,
        role: effectiveRole,
        replace_lead: confirmOverrides
          ? (recastConflict ? recastConflict.replaceLead : true)
          : false,
        create_area: confirmOverrides
          ? (recastConflict ? recastConflict.createArea : true)
          : false,
      });
      if (
        (res?.retires_lead ||
          (Array.isArray(res?.creates_areas) && res.creates_areas.length > 0)) &&
        !confirmOverrides
      ) {
        setRecastConflict(extractRecastConflict(res));
        return;
      }
      setRecastConflict(null);
      setRecastModalOpen(false);
      setRecastStatusBanner(
        `Re-cast ${displayTarget} to ${recastProject} as ${roleLabel(effectiveRole, roleCatalog.roles)}.${res?.guidance_requires_reforge ? " Run an explicit Reforge to apply current role guidance." : ""}`,
      );
      onAgentsUpdated?.();
    } catch (err) {
      if (
        err instanceof HubApiError &&
        (err.status === 409 || err.status === 400) &&
        !confirmOverrides
      ) {
        setRecastConflict(extractRecastConflict(err));
      } else {
        setRecastError(
          err instanceof Error ? err.message : `Failed to re-cast ${displayTarget}`,
        );
      }
    } finally {
      setRecasting(false);
    }
  };
  const handleContextWindowChange = async (event: React.ChangeEvent<HTMLSelectElement>) => {
    const next = Number(event.target.value);
    if (!sessionId || contextWindowSending || !Number.isInteger(next) || next <= 0) return;
    const token = ++contextWindowRequestRef.current;
    setContextWindowPick({ value: next, reportedAtPick: reportedMaxTokens });
    setContextWindowNotice(null);
    setContextWindowSending(true);
    try {
      const client = apiClient ?? new HubApiClient();
      const res = await client.setSessionContextWindow(sessionId, next);
      if (token !== contextWindowRequestRef.current) return;
      // A stored window is not applied to the running session: show what it really uses.
      if (res.method === "stored") setContextWindowPick(null);
      setContextWindowNotice({
        kind: "ok",
        requested: next,
        method: res.method,
        persisted: res.persisted,
      });
    } catch (err) {
      if (token !== contextWindowRequestRef.current) return;
      // Roll back to what the session reports.
      setContextWindowPick(null);
      setContextWindowNotice({
        kind: "error",
        text: err instanceof Error && err.message ? err.message : "Could not set the context window",
      });
    } finally {
      if (token === contextWindowRequestRef.current) {
        contextWindowRefocusRef.current = true;
        setContextWindowSending(false);
      }
    }
  };
  const baseThinkingOptions: ReadonlyArray<{ value: string; label: string }> =
    isAgySession
      ? THINKING_LEVEL_OPTIONS.map((opt) =>
          opt.value === "high" ? { value: "high", label: "High" } : opt,
        )
      : THINKING_LEVEL_OPTIONS;
  const thinkingOptions: ReadonlyArray<{ value: string; label: string }> =
    !effectiveThinking
      ? [{ value: "", label: "Select a thinking level" }, ...baseThinkingOptions]
      : !baseThinkingOptions.some((opt) => opt.value === effectiveThinking)
        ? [{ value: effectiveThinking, label: effectiveThinking }, ...baseThinkingOptions]
        : baseThinkingOptions;
  const effectiveExecutionMode =
    fetchedFor("executionMode", executionMode, fetchedSession?.executionMode) ??
    (executionMode !== undefined ? executionMode : null);
  const executionModeKey = (effectiveExecutionMode ?? "").trim().toLowerCase();
  const executionModeIsYolo = executionModeKey === "yolo";
  const sameAsTrackedQuota = sameModalQuotaUsage(
    candidateQuotaUsage,
    quotaSourceRef.current.quotaUsage,
  );
  const isQuotaFromPreviousPool = Boolean(
    quotaSourceRef.current.poolKey &&
      currentPoolKey &&
      ((currentPoolKey !== quotaSourceRef.current.poolKey && sameAsTrackedQuota) ||
        (currentPoolKey === quotaSourceRef.current.poolKey &&
          quotaSourceRef.current.staleAcrossPoolSwitch &&
          sameAsTrackedQuota)),
  );
  const adapterChangedAcrossVendors =
    isAdapterOverridden &&
    Boolean(sessionVendor && effectiveVendor && sessionVendor !== effectiveVendor) &&
    (!isQuotaOverridden || isQuotaFromPreviousPool);
  const effectiveQuotaUsage =
    adapterChangedAcrossVendors || isQuotaFromPreviousPool ? null : candidateQuotaUsage;
  const effectiveLivenessState: "alive" | "stale" | "dead" =
    livenessState ??
    (fetchedSession?.status
      ? isLiveSessionStatus(fetchedSession.status)
        ? "alive"
        : "dead"
      : sessionId
        ? "alive"
        : "dead");
  const quotaEval = evaluateQuotaStatus({
    quotaUsage: effectiveQuotaUsage,
    vendor: effectiveAdapter,
    model: effectiveModel,
  });
  const livenessLabel =
    effectiveLivenessState === "alive"
      ? "Online (alive)"
      : effectiveLivenessState === "stale"
        ? "Stale"
        : "Offline (dead)";

  return (
    <div className="fd-modal-backdrop" onClick={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        aria-label={`Agent Settings — ${displayTarget}`}
        data-testid="agent-settings-modal"
        className="fd-modal"
        onClick={(e) => e.stopPropagation()}
        style={{
          width: "min(560px, calc(100vw - 24px))",
          maxWidth: "calc(100vw - 24px)",
          minWidth: 0,
          height: "760px",
          maxHeight: "calc(100dvh - 32px)",
          minHeight: 0,
          boxSizing: "border-box",
          display: "flex",
          flexDirection: "column",
          overflow: "hidden",
          gap: "0",
        }}
      >
        {/* Pinned Header + 3-Tab Navigation Bar */}
        <div
          className="fd-modal-header"
          data-testid="agent-settings-header"
          style={{
            display: "flex",
            flexDirection: "column",
            gap: "10px",
            flexShrink: 0,
            paddingBottom: "10px",
            borderBottom: "1px solid var(--border-subtle)",
          }}
        >
          {returnToSettingsLabel && onReturnToSettings && (
            <div style={{ display: "flex", alignItems: "center" }}>
              <button
                type="button"
                className="fd-btn"
                data-testid="agent-settings-back-to-origin-btn"
                onClick={onReturnToSettings}
                style={{
                  fontSize: "11.5px",
                  padding: "3px 8px",
                  fontWeight: 600,
                }}
              >
                {returnToSettingsLabel}
              </button>
            </div>
          )}
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: "8px",
            }}
          >
            <div style={{ flex: 1, minWidth: 0 }}>
              <h2 style={{ margin: 0, fontSize: "16px", fontWeight: 700 }}>
                Agent Settings — {displayTarget}
              </h2>
              <p
                style={{
                  margin: "3px 0 0 0",
                  fontSize: "12px",
                  color: "var(--text-secondary)",
                }}
              >
                Configure live model &amp; thinking level, inspect context &amp; execution mode, and
                manage session lifecycle.
              </p>
            </div>
            <div style={{ display: "flex", alignItems: "center", gap: "6px", flexShrink: 0 }}>
              <button
                type="button"
                data-testid="agent-pin-toggle-btn"
                aria-pressed={pinnedAgents.has((agentName || displayTarget).trim().toLowerCase())}
                title={
                  pinnedAgents.has((agentName || displayTarget).trim().toLowerCase())
                    ? "Unpin Agent"
                    : "Pin Agent"
                }
                aria-label={
                  pinnedAgents.has((agentName || displayTarget).trim().toLowerCase())
                    ? "Unpin Agent"
                    : "Pin Agent"
                }
                className={`fd-btn${pinnedAgents.has((agentName || displayTarget).trim().toLowerCase()) ? " fd-btn-active" : ""}`}
                style={{
                  padding: "4px 6px",
                  display: "inline-flex",
                  alignItems: "center",
                  justifyContent: "center",
                  lineHeight: 1,
                }}
                onClick={() => togglePinnedAgent(agentName || displayTarget)}
              >
                <PinIcon
                  pinned={pinnedAgents.has((agentName || displayTarget).trim().toLowerCase())}
                  circled={true}
                  size={16}
                />
              </button>
              <button
                type="button"
                className="fd-btn"
                data-testid="close-agent-settings-btn"
                aria-label="Close Agent Settings"
                onClick={onClose}
              >
                ✕
              </button>
            </div>
          </div>

          <div
            role="tablist"
            aria-label="Agent Settings Tabs"
            data-testid="agent-settings-tabs"
            style={{
              width: "100%",
              minWidth: 0,
            }}
          >
            <select
              aria-label="Agent Settings Section"
              data-testid="agent-settings-tab-select"
              className="fd-select"
              value={activeTab}
              onChange={(e) =>
                setActiveTab(
                  e.target.value as "overview" | "permissions" | "runtime",
                )
              }
              style={{
                width: "100%",
                minWidth: 0,
                maxWidth: "100%",
                boxSizing: "border-box",
                fontSize: "12.5px",
                padding: "6px 10px",
              }}
            >
              <option value="overview" data-testid="agent-settings-tab-option-overview">
                Overview &amp; Model
              </option>
              <option
                value="permissions"
                data-testid="agent-settings-tab-option-permissions"
              >
                Permissions &amp; Sandbox
              </option>
              <option value="runtime" data-testid="agent-settings-tab-option-runtime">
                MCP &amp; Runtime
              </option>
            </select>
            <div
              className="sr-only"
              style={{
                position: "absolute",
                width: "1px",
                height: "1px",
                padding: 0,
                margin: "-1px",
                overflow: "hidden",
                clip: "rect(0, 0, 0, 0)",
                whiteSpace: "nowrap",
                border: 0,
              }}
            >
              {(
                [
                  { id: "overview", visibleLabel: "Overview", srSuffix: " & Model" },
                  { id: "permissions", visibleLabel: "Permissions", srSuffix: " & Sandbox" },
                  { id: "runtime", visibleLabel: "MCP & Runtime", srSuffix: "" },
                ] as const
              ).map((tab) => {
                const isSelected = activeTab === tab.id;
                return (
                  <button
                    key={tab.id}
                    id={`agent-settings-tab-${tab.id}`}
                    type="button"
                    role="tab"
                    aria-selected={isSelected}
                    aria-controls={`agent-settings-panel-${tab.id}`}
                    data-testid={`agent-settings-tab-${tab.id}`}
                    className={`fd-btn${isSelected ? " fd-btn-primary" : ""}`}
                    onClick={() => setActiveTab(tab.id)}
                  >
                    {tab.visibleLabel}
                    {tab.srSuffix && (
                      <span
                        className="sr-only"
                        style={{
                          position: "absolute",
                          width: "1px",
                          height: "1px",
                          padding: 0,
                          margin: "-1px",
                          overflow: "hidden",
                          clip: "rect(0, 0, 0, 0)",
                          whiteSpace: "nowrap",
                          border: 0,
                        }}
                      >
                        {tab.srSuffix}
                      </span>
                    )}
                  </button>
                );
              })}
            </div>
          </div>
        </div>

        {/* Scrollable Middle Body */}
        <div
          className="fd-scroll"
          data-testid="agent-settings-scroll-body"
          style={{
            flex: 1,
            minHeight: 0,
            overflowY: "auto",
            overflowX: "hidden",
            display: "flex",
            flexDirection: "column",
            gap: "12px",
            padding: "12px 2px 12px 0",
          }}
        >
          {/* Tab Panel 1: Overview & Model */}
          <div
            id="agent-settings-panel-overview"
            role="tabpanel"
            aria-labelledby="agent-settings-tab-overview"
            data-testid="agent-settings-panel-overview"
            data-active={activeTab === "overview" ? "true" : "false"}
            style={{
              display: activeTab === "overview" ? "flex" : "none",
              flexDirection: "column",
              gap: "12px",
            }}
          >
            {/* Agent Identity Summary */}
            <div
              data-testid="agent-settings-identity"
              className="fd-card"
              style={{
                padding: "10px 12px",
                display: "grid",
                gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
                gap: "8px",
                backgroundColor: "var(--bg-elevated)",
                fontSize: "12px",
              }}
            >
              <div>
                <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                  Persona
                </div>
                <div className="fd-mono" style={{ fontWeight: 600 }}>
                  {displayTarget} {agentName ? `(@${agentName})` : ""}
                </div>
              </div>
              {agentProject && (
                <div>
                  <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                    Project
                  </div>
                  <div style={{ fontWeight: 600 }}>{agentProject}</div>
                </div>
              )}
              {agentRole && (
                <div>
                  <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                    Role
                  </div>
                  <div className="fd-mono">{roleLabel(canonicalizeDefaultRole(agentRole), roleCatalog.roles)}</div>
                </div>
              )}
              <div>
                <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                  Harness / Adapter
                </div>
                <div className="fd-mono">{effectiveAdapter || "unknown"}</div>
              </div>
              <div>
                <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                  Session ID
                </div>
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: "6px",
                    minWidth: 0,
                  }}
                >
                  {sessionId && (
                    <span
                      className="fd-badge fd-mono"
                      data-testid="session-id-pill"
                      title={sessionId}
                      style={{ flexShrink: 0 }}
                    >
                      #{sessionId.slice(-8)}
                    </span>
                  )}
                  <span
                    className="fd-mono"
                    title={sessionId || targetSessionOrAgent || "none"}
                    style={{
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                      whiteSpace: "nowrap",
                    }}
                  >
                    {sessionId || targetSessionOrAgent || "unrouted"}
                  </span>
                </div>
              </div>
              <div style={{ minWidth: 0 }}>
                <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                  Active Model
                </div>
                <span
                  className="fd-badge fd-mono"
                  data-testid="chat-active-model-badge"
                  style={{
                    maxWidth: "100%",
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                    whiteSpace: "nowrap",
                    display: "inline-block",
                  }}
                >
                  {effectiveAdapter && effectiveAdapter.toLowerCase() !== "unknown"
                    ? `${effectiveAdapter} · `
                    : ""}
                  {shownModel || "default"}
                  {effectiveThinking ? ` (${effectiveThinking})` : " (unknown)"}
                </span>
              </div>
              {typeof turnCount === "number" && (
                <div>
                  <div style={{ fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase" }}>
                    Transcript Turns
                  </div>
                  <span className="fd-badge" data-testid="turn-count-badge">
                    {turnCount} {turnCount === 1 ? "turn" : "turns"}
                  </span>
                </div>
              )}
            </div>

            {/* Model & Thinking Level Selectors */}
            <div
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))",
                gap: "10px",
              }}
            >
              <div style={{ minWidth: 0 }}>
                <label
                  htmlFor="fd-agent-settings-model"
                  style={{
                    display: "block",
                    fontSize: "11px",
                    fontWeight: 600,
                    color: "var(--text-muted)",
                    marginBottom: "4px",
                    textTransform: "uppercase",
                  }}
                >
                  Session Model
                </label>
                <select
                  id="fd-agent-settings-model"
                  aria-label="Session Model"
                  data-testid="session-model-select"
                  className="fd-select fd-mono"
                  disabled={!targetSessionOrAgent || switchingModel}
                  value={shownModel}
                  onChange={(e) => {
                    const nextModel = e.target.value;
                    setOverridden((prev) => (prev.model ? prev : { ...prev, model: true }));
                    if (
                      isAgySession &&
                      !effectiveThinking &&
                      agyModelRequiresThinkingLevel(nextModel, effectiveAvailableModels)
                    ) {
                      setPendingAgyModel(nextModel);
                      return;
                    }
                    setPendingAgyModel(null);
                    onChangeModelAndThinking(nextModel, effectiveThinking);
                  }}
                  style={{
                    width: "100%",
                    minWidth: 0,
                    maxWidth: "100%",
                    boxSizing: "border-box",
                  }}
                >
                  {modelOptions.length === 0 && (
                    <option value="">Default (runtime default)</option>
                  )}
                  {modelOptions.map((opt) => (
                    <option key={opt} value={opt}>
                      {formatModelOptionLabel(opt)}
                    </option>
                  ))}
                </select>
              </div>

              <div style={{ minWidth: 0 }}>
                <label
                  htmlFor="fd-agent-settings-thinking"
                  style={{
                    display: "block",
                    fontSize: "11px",
                    fontWeight: 600,
                    color: "var(--text-muted)",
                    marginBottom: "4px",
                    textTransform: "uppercase",
                  }}
                >
                  Thinking Level
                </label>
                <select
                  id="fd-agent-settings-thinking"
                  aria-label="Thinking Level"
                  data-testid="session-thinking-select"
                  className="fd-select fd-mono"
                  disabled={!targetSessionOrAgent || switchingModel}
                  value={effectiveThinking}
                  onChange={(e) => {
                    const nextThinking = e.target.value;
                    if (!nextThinking) {
                      return;
                    }
                    setOverridden((prev) =>
                      prev.thinkingLevel ? prev : { ...prev, thinkingLevel: true },
                    );
                    const modelToApply = pendingAgyModel || effectiveModel;
                    setPendingAgyModel(null);
                    onChangeModelAndThinking(modelToApply, nextThinking);
                  }}
                  style={{
                    width: "100%",
                    minWidth: 0,
                    maxWidth: "100%",
                    boxSizing: "border-box",
                  }}
                >
                  {thinkingOptions.map((opt) => (
                    <option key={opt.value} value={opt.value} disabled={opt.value === ""}>
                      {opt.label}
                    </option>
                  ))}
                </select>
              </div>
              {agyThinkingRequired && (
                <div
                  data-testid="agy-thinking-required-hint"
                  style={{
                    gridColumn: "1 / -1",
                    fontSize: "11.5px",
                    color: "var(--text-secondary)",
                  }}
                >
                  Choose a thinking level to switch this Antigravity model.
                </div>
              )}

              <div
                data-testid="context-window-row"
                style={{ minWidth: 0 }}
              >
                {contextWindowEditable ? (
                  <>
                    <label
                      htmlFor="fd-agent-settings-context-window"
                      style={{
                        display: "block",
                        fontSize: "11px",
                        fontWeight: 600,
                        color: "var(--text-muted)",
                        marginBottom: "4px",
                        textTransform: "uppercase",
                      }}
                    >
                      Context window
                    </label>
                    <select
                      id="fd-agent-settings-context-window"
                      ref={contextWindowSelectRef}
                      data-testid="context-window-select"
                      className="fd-select fd-mono"
                      disabled={contextWindowSending}
                      aria-busy={contextWindowSending}
                      aria-describedby={
                        contextWindowNotice?.kind === "error"
                          ? `${CONTEXT_WINDOW_STATUS_ID} ${CONTEXT_WINDOW_ERROR_ID}`
                          : CONTEXT_WINDOW_STATUS_ID
                      }
                      value={
                        shownContextWindow !== null &&
                        offeredContextWindows.includes(shownContextWindow)
                          ? String(shownContextWindow)
                          : ""
                      }
                      onChange={(e) => void handleContextWindowChange(e)}
                      style={{
                        width: "100%",
                        minWidth: 0,
                        maxWidth: "100%",
                        boxSizing: "border-box",
                      }}
                    >
                      {(shownContextWindow === null ||
                        !offeredContextWindows.includes(shownContextWindow)) && (
                        <option value="" disabled>
                          Choose a size
                        </option>
                      )}
                      {offeredContextWindows.map((size) => (
                        <option key={size} value={String(size)}>
                          {formatTokenCount(size)}
                        </option>
                      ))}
                    </select>
                    <div
                      className="fd-mono"
                      style={{ fontSize: "11px", color: "var(--text-muted)", marginTop: "4px" }}
                    >
                      Effective:{" "}
                      <span
                        data-testid="context-window-effective"
                        data-source={contextEstimate.source ?? "unknown"}
                        title={contextWindowSourceTitle(contextEstimate.source)}
                        style={
                          contextEstimate.source === "assumed"
                            ? { fontStyle: "italic", cursor: "help" }
                            : undefined
                        }
                      >
                        {contextWindowSizeText}
                      </span>
                      {contextWindowWaiting && (
                        <span data-testid="context-window-waiting">
                          {" "}
                          (waiting for the session to report it)
                        </span>
                      )}
                    </div>
                    <div
                      id={CONTEXT_WINDOW_STATUS_ID}
                      role="status"
                      data-testid="context-window-status"
                      style={{ fontSize: "11px", color: "var(--text-muted)", marginTop: "4px" }}
                    >
                      {contextWindowStatusText}
                    </div>
                    {contextWindowNotice?.kind === "error" && (
                      <div
                        id={CONTEXT_WINDOW_ERROR_ID}
                        role="alert"
                        data-testid="context-window-error"
                        style={{
                          fontSize: "11px",
                          color: "var(--status-danger)",
                          marginTop: "4px",
                        }}
                      >
                        {contextWindowNotice.text}
                      </div>
                    )}
                  </>
                ) : (
                  <div
                    data-testid="context-window-readonly"
                    className="fd-mono"
                    style={{ fontSize: "12px", color: "var(--text-muted)" }}
                  >
                    {contextVendor === "grok"
                      ? offeredContextWindows.length >= 2
                        ? authority?.tier !== "owner"
                          ? `Context window: ${contextWindowSizeText}. Only an owner can change it.`
                          : `Context window: ${contextWindowSizeText}.`
                        : `Context window: ${contextWindowSizeText} (no other sizes offered)`
                      : contextVendor
                        ? `Context window: ${contextWindowSizeText}. ${vendorDisplayName(contextVendor)}'s window cannot be selected from FerroDeck.`
                        : `Context window: ${contextWindowSizeText}. This session's window cannot be selected from FerroDeck.`}
                  </div>
                )}
              </div>
            </div>

            {/* Context Meter & Execution Mode */}
            <div
              className="fd-card"
              style={{
                padding: "10px 12px",
                display: "flex",
                flexDirection: "column",
                gap: "8px",
                backgroundColor: "var(--bg-elevated)",
              }}
            >
              <div
                data-testid="context-level-meter"
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
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
                  <span
                    style={{
                      fontSize: "11px",
                      fontWeight: 600,
                      textTransform: "uppercase",
                      color: "var(--text-muted)",
                    }}
                  >
                    Context Window Usage
                  </span>
                  <span
                    data-testid="context-level-text"
                    className="fd-mono"
                    style={{ fontSize: "12px", color: "var(--text-primary)" }}
                  >
                    {contextEstimate.maxTokens === null ? (
                      <>
                        Context:{" "}
                        {contextEstimate.usedTokens === null
                          ? "no data"
                          : `${contextEstimate.estimated ? "about " : ""}${formatTokenCount(contextEstimate.usedTokens)} used`}
                        {contextEstimate.usedTokens !== null ? ", window not reported" : ""}
                      </>
                    ) : (
                      <>
                        Context: {formatTokenCount(contextEstimate.usedTokens ?? 0)} /{" "}
                        <span
                          data-testid="context-max"
                          data-source={contextEstimate.source ?? "unknown"}
                          title={contextWindowSourceTitle(contextEstimate.source)}
                          style={
                            contextEstimate.source === "assumed"
                              ? { fontStyle: "italic", cursor: "help" }
                              : undefined
                          }
                        >
                          {formatContextMax(contextEstimate.maxTokens, contextEstimate.source)}
                        </span>{" "}
                        ({contextEstimate.percent}%)
                      </>
                    )}
                  </span>
                </div>
                {contextEstimate.percent !== null && (
                  <div
                    role="progressbar"
                    aria-label="Context level"
                    aria-valuemin={0}
                    aria-valuemax={100}
                    aria-valuenow={contextEstimate.percent}
                    aria-valuetext={`${contextEstimate.percent}%${
                      contextEstimate.source === "assumed"
                        ? " against an assumed window"
                        : contextEstimate.source === "reported" ||
                            contextEstimate.source === "catalog"
                          ? ` of a ${contextEstimate.source} window`
                          : ""
                    }`}
                    style={{
                      width: "100%",
                      height: "8px",
                      borderRadius: "4px",
                      backgroundColor: "var(--bg-input)",
                      border: "1px solid var(--border-subtle)",
                      overflow: "hidden",
                    }}
                  >
                    <div
                      style={{
                        width: `${contextEstimate.percent}%`,
                        height: "100%",
                        backgroundColor:
                          contextEstimate.percent >= 80
                            ? "var(--status-danger)"
                            : contextEstimate.percent >= 50
                              ? "var(--status-stale)"
                              : "var(--status-alive)",
                      }}
                    />
                  </div>
                )}
                {(contextEstimate.offeredWindows?.length ?? 0) > 0 && (
                  <div
                    data-testid="context-offered-windows"
                    className="fd-mono"
                    style={{ fontSize: "11px", color: "var(--text-muted)" }}
                  >
                    Offered:{" "}
                    {(contextEstimate.offeredWindows ?? []).map((size, index) => {
                      const effective = size === contextEstimate.maxTokens;
                      return (
                        <React.Fragment key={size}>
                          {index > 0 ? ", " : ""}
                          <span
                            data-testid={`context-offered-${size}`}
                            data-effective={effective ? "true" : undefined}
                            aria-current={effective ? "true" : undefined}
                            style={
                              effective
                                ? { fontWeight: 700, color: "var(--text-primary)" }
                                : undefined
                            }
                          >
                            {formatTokenCount(size)}
                          </span>
                        </React.Fragment>
                      );
                    })}
                  </div>
                )}
              </div>

              <div
                data-testid="agent-settings-execution-mode"
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
                  paddingTop: "4px",
                  borderTop: "1px solid var(--border-subtle)",
                }}
              >
                <span
                  style={{
                    fontSize: "11px",
                    fontWeight: 600,
                    textTransform: "uppercase",
                    color: "var(--text-muted)",
                  }}
                >
                  Execution Mode
                </span>
                <span
                  data-testid="execution-mode-badge"
                  data-mode={executionModeKey || "unknown"}
                  data-warning={executionModeIsYolo ? "true" : undefined}
                  className="fd-badge"
                  title={
                    executionModeIsYolo
                      ? "Yolo: this session runs tools without asking for approval"
                      : "Reported by the agent runtime"
                  }
                  style={{
                    alignSelf: "flex-start",
                    padding: "4px 8px",
                    fontWeight: 600,
                    borderColor: executionModeIsYolo
                      ? "var(--status-stale, #F59E0B)"
                      : "var(--border-subtle)",
                    backgroundColor: executionModeIsYolo
                      ? "rgba(245, 158, 11, 0.14)"
                      : "var(--bg-surface)",
                    color: executionModeIsYolo
                      ? "var(--status-stale, #F59E0B)"
                      : executionModeKey
                        ? "var(--text-secondary)"
                        : "var(--text-muted)",
                  }}
                >
                  {formatExecutionMode(effectiveExecutionMode)}
                </span>
                <span
                  data-testid="execution-mode-note"
                  style={{ fontSize: "11px", color: "var(--text-muted)" }}
                >
                  Read-only: how this session handles tool approvals. Change it in the agent&apos;s own
                  CLI.
                </span>
              </div>

              <div
                data-testid="agent-settings-status-detail"
                data-liveness-state={effectiveLivenessState}
                data-quota-state={quotaEval.status}
                data-quota-reason={quotaEval.reasonKind}
                data-quota-pool={quotaEval.poolLabel ?? undefined}
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: "6px",
                  paddingTop: "4px",
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
                  <span
                    style={{
                      fontSize: "11px",
                      fontWeight: 600,
                      textTransform: "uppercase",
                      color: "var(--text-muted)",
                    }}
                  >
                    Status &amp; Allowance
                  </span>
                  {quotaEval.poolLabel && (
                    <span
                      data-testid="agent-settings-allowance-pool"
                      className="fd-badge fd-mono"
                      style={{ fontSize: "10.5px", padding: "2px 6px" }}
                    >
                      Pool: {quotaEval.poolLabel}
                    </span>
                  )}
                </div>
                <div
                  data-testid="agent-settings-liveness-detail"
                  style={{ fontSize: "12px", color: "var(--text-secondary)" }}
                >
                  Process: <strong style={{ color: "var(--text-primary)" }}>{livenessLabel}</strong>
                </div>
                <div
                  data-testid="agent-settings-allowance-detail"
                  style={{
                    fontSize: "12px",
                    color:
                      quotaEval.status === "red"
                        ? "var(--status-danger)"
                        : quotaEval.status === "orange"
                          ? "#F97316"
                          : quotaEval.status === "yellow"
                            ? "var(--status-stale)"
                            : "var(--text-secondary)",
                  }}
                >
                  Allowance: {quotaEval.detailText}
                </div>
                {effectiveLivenessState === "dead" && quotaEval.status !== "unknown" && (
                  <div
                    data-testid="agent-settings-offline-quota-note"
                    style={{ fontSize: "11px", color: "var(--text-muted)" }}
                  >
                    Process is offline; usage allowance is reported separately (
                    {quotaEval.reasonKind === "exhausted"
                      ? "quota exhausted"
                      : `${quotaEval.status} state`}
                    ).
                  </div>
                )}
              </div>
            </div>
          </div>

          {/* Tab Panel 2: Permissions & Sandbox */}
          <div
            id="agent-settings-panel-permissions"
            role="tabpanel"
            aria-labelledby="agent-settings-tab-permissions"
            data-testid="agent-settings-panel-permissions"
            data-active={activeTab === "permissions" ? "true" : "false"}
            style={{
              display: activeTab === "permissions" ? "flex" : "none",
              flexDirection: "column",
              gap: "12px",
            }}
          >
            {/* Record #103: Universal Agent Permission & Auto-Accept Management (3-Layer + Keyed Change Classes) */}
            {(() => {
              const hostTelemetryForPerms = resolveAgentHostSupervisorTelemetry(
                effectiveAdapter,
                agentName || sessionId,
              );
              const postureChangeClass = resolvePermissionChangeClass(
                effectiveAdapter,
                hostTelemetryForPerms.transportMode,
                "posture",
                agentPosture,
              );
              const sandboxChangeClass = resolvePermissionChangeClass(
                effectiveAdapter,
                hostTelemetryForPerms.transportMode,
                "sandbox",
              );
              const compileReceipt = compileUniversalPermissions(
                effectiveAdapter,
                agentPosture,
                agentSandbox,
                agentPermissionMatrix,
                {
                  requireFsEnforcement: agentRequireFsEnforcement,
                  unenforcedDefaults: agentUnenforcedDefaults,
                },
              );
              return (
                <div
                  data-testid="agent-permissions-section"
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
                    <span style={{ fontWeight: 700, fontSize: "11px" }}>
                      Permission Posture, Sandbox &amp; Rules
                    </span>
                    <code
                      className="fd-mono"
                      data-testid="agent-permission-vendor-preview"
                      style={{
                        fontSize: "10.5px",
                        color: "var(--accent-primary)",
                        overflowWrap: "anywhere",
                        wordBreak: "break-word",
                        minWidth: 0,
                      }}
                    >
                      {resolveVendorPermissionPreview(
                        effectiveAdapter,
                        agentPermissionMode,
                      )}
                    </code>
                  </div>

                  {/* 3-Layer Controls with Keyed Change-Class Badges */}
                  <div
                    style={{
                      display: "grid",
                      gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))",
                      gap: "6px",
                    }}
                  >
                    <div style={{ minWidth: 0 }}>
                      <div
                        style={{
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "space-between",
                          gap: "4px",
                          marginBottom: "2px",
                          flexWrap: "wrap",
                        }}
                      >
                        <span
                          style={{
                            fontSize: "10px",
                            fontWeight: 600,
                            color: "var(--text-muted)",
                            textTransform: "uppercase",
                          }}
                        >
                          1. Posture
                        </span>
                        <span
                          className="fd-badge fd-mono"
                          data-testid="agent-change-class-posture"
                          style={{ fontSize: "9.5px" }}
                        >
                          {postureChangeClass}
                        </span>
                      </div>
                      <select
                        aria-label="Agent Approval Posture"
                        data-testid="agent-posture-select"
                        className="fd-select fd-mono"
                        value={agentPosture}
                        onChange={(e) => {
                          const nextPosture = e.target
                            .value as UniversalPermissionPosture;
                          setAgentPosture(nextPosture);
                          if (nextPosture === "bypass") {
                            setAgentPermissionMode("auto-accept");
                            setAgentRequireFsEnforcement(false);
                          } else if (nextPosture === "accept-edits") {
                            setAgentPermissionMode("workspace-write");
                          }
                        }}
                        style={{
                          width: "100%",
                          minWidth: 0,
                          maxWidth: "100%",
                          boxSizing: "border-box",
                          fontSize: "10.5px",
                        }}
                      >
                        <option value="ask">ask (default)</option>
                        <option value="accept-edits">accept-edits</option>
                        <option value="auto">auto</option>
                        <option value="plan">plan</option>
                        <option value="bypass">bypass (skip prompts)</option>
                      </select>
                    </div>

                    <div style={{ minWidth: 0 }}>
                      <div
                        style={{
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "space-between",
                          gap: "4px",
                          marginBottom: "2px",
                          flexWrap: "wrap",
                        }}
                      >
                        <span
                          style={{
                            fontSize: "10px",
                            fontWeight: 600,
                            color: "var(--text-muted)",
                            textTransform: "uppercase",
                          }}
                        >
                          2. Sandbox
                        </span>
                        <span
                          className="fd-badge fd-mono"
                          data-testid="agent-change-class-sandbox"
                          style={{ fontSize: "9.5px" }}
                        >
                          {sandboxChangeClass}
                        </span>
                      </div>
                      <select
                        aria-label="Agent OS Sandbox Mode"
                        data-testid="agent-sandbox-select"
                        className="fd-select fd-mono"
                        value={agentSandbox}
                        onChange={(e) => {
                          const nextSandbox = e.target
                            .value as UniversalSandboxMode;
                          setAgentSandbox(nextSandbox);
                          if (nextSandbox === "off") {
                            setAgentRequireFsEnforcement(false);
                          }
                        }}
                        style={{
                          width: "100%",
                          minWidth: 0,
                          maxWidth: "100%",
                          boxSizing: "border-box",
                          fontSize: "10.5px",
                        }}
                      >
                        <option value="vendor-default">vendor-default</option>
                        <option value="off">off (no OS sandbox)</option>
                      </select>
                    </div>

                    <div style={{ minWidth: 0 }}>
                      <div
                        style={{
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "space-between",
                          gap: "4px",
                          marginBottom: "2px",
                          flexWrap: "wrap",
                        }}
                      >
                        <span
                          style={{
                            fontSize: "10px",
                            fontWeight: 600,
                            color: "var(--text-muted)",
                            textTransform: "uppercase",
                          }}
                        >
                          3. Rule Preset
                        </span>
                      </div>
                      <select
                        aria-label="Agent Capability Rule Preset"
                        data-testid="agent-rule-preset-select"
                        className="fd-select fd-mono"
                        value={agentRulePreset}
                        onChange={(e) => {
                          const nextPreset = e.target
                            .value as UniversalRulePreset;
                          setAgentRulePreset(nextPreset);
                          setAgentPermissionMatrix({
                            ...DEFAULT_RULE_PRESET_MATRICES[nextPreset],
                          });
                        }}
                        style={{
                          width: "100%",
                          minWidth: 0,
                          maxWidth: "100%",
                          boxSizing: "border-box",
                          fontSize: "10.5px",
                        }}
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
                      flexDirection: "column",
                      alignItems: "stretch",
                      gap: "6px",
                      width: "100%",
                      minWidth: 0,
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
                        minWidth: 0,
                        flexWrap: "wrap",
                      }}
                    >
                      <input
                        type="checkbox"
                        data-testid="agent-require-fs-enforcement-checkbox"
                        checked={agentRequireFsEnforcement}
                        disabled={
                          agentSandbox === "off" || agentPosture === "bypass"
                        }
                        onChange={(e) =>
                          setAgentRequireFsEnforcement(e.target.checked)
                        }
                      />
                      <span style={{ overflowWrap: "anywhere" }}>
                        require_fs_enforcement (OS boundary for
                        fs_write_protected)
                      </span>
                    </label>
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: "6px",
                        width: "100%",
                        minWidth: 0,
                        flexWrap: "wrap",
                      }}
                    >
                      <label
                        htmlFor="fd-agent-unenforced-defaults"
                        style={{
                          fontSize: "10px",
                          color: "var(--text-muted)",
                          fontWeight: 600,
                          flexShrink: 0,
                        }}
                      >
                        unenforced_defaults:
                      </label>
                      <select
                        id="fd-agent-unenforced-defaults"
                        aria-label="Agent Unenforced Defaults Policy"
                        data-testid="agent-unenforced-defaults-select"
                        className="fd-select fd-mono"
                        value={agentUnenforcedDefaults}
                        onChange={(e) =>
                          setAgentUnenforcedDefaults(
                            e.target.value as UnenforcedDefaultsMode,
                          )
                        }
                        style={{
                          fontSize: "10.5px",
                          padding: "2px 6px",
                          flex: 1,
                          minWidth: 0,
                          maxWidth: "100%",
                          boxSizing: "border-box",
                        }}
                      >
                        <option value="refuse">refuse (default)</option>
                        <option value="acknowledge">acknowledge</option>
                      </select>
                    </div>
                  </div>

                  <select
                    aria-label="Agent Permission & Approval Mode"
                    data-testid="agent-permission-mode-select"
                    className="fd-select fd-mono"
                    value={agentPermissionMode}
                    onChange={(e) => {
                      const nextMode = e.target.value as UniversalPermissionMode;
                      setAgentPermissionMode(nextMode);
                      setAgentPermissionMatrix({
                        ...DEFAULT_UNIVERSAL_PERMISSION_MATRICES[nextMode],
                      });
                      if (nextMode === "auto-accept") {
                        setAgentPosture("bypass");
                        setAgentRequireFsEnforcement(false);
                      } else if (nextMode === "workspace-write") {
                        setAgentPosture("accept-edits");
                        setAgentRulePreset("workspace");
                      } else {
                        setAgentRulePreset("custom");
                      }
                    }}
                    style={{
                      width: "100%",
                      minWidth: 0,
                      maxWidth: "100%",
                      boxSizing: "border-box",
                      fontSize: "11px",
                    }}
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
                    data-testid="agent-permission-matrix"
                    style={{
                      display: "grid",
                      gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))",
                      gap: "4px",
                    }}
                  >
                    {UNIVERSAL_PERMISSION_CAPABILITIES.map((cap) => {
                      const capChangeClass = resolvePermissionChangeClass(
                        effectiveAdapter,
                        hostTelemetryForPerms.transportMode,
                        cap,
                      );
                      return (
                        <div
                          key={cap}
                          data-testid={`agent-perm-cap-${cap}`}
                          style={{
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "space-between",
                            gap: "4px",
                            padding: "3px 6px",
                            borderRadius: "4px",
                            border: "1px solid var(--border-subtle)",
                            fontSize: "10.5px",
                            minWidth: 0,
                            flexWrap: "wrap",
                          }}
                        >
                          <div
                            style={{
                              display: "flex",
                              alignItems: "center",
                              gap: "4px",
                              minWidth: 0,
                            }}
                          >
                            <code className="fd-mono">{cap}</code>
                            <span
                              className="fd-badge fd-mono"
                              data-testid={`agent-perm-change-class-${cap}`}
                              style={{ fontSize: "9px", padding: "0 4px" }}
                            >
                              {capChangeClass}
                            </span>
                          </div>
                          <select
                            aria-label={`Agent permission for ${cap}`}
                            data-testid={`agent-perm-select-${cap}`}
                            className="fd-select fd-mono"
                            style={{
                              fontSize: "10px",
                              padding: "1px 4px",
                              minWidth: 0,
                              maxWidth: "100%",
                            }}
                            value={agentPermissionMatrix[cap]}
                            onChange={(e) => {
                              const nextAction = e.target
                                .value as PermissionRuleAction;
                              setAgentPermissionMode("granular");
                              setAgentRulePreset("custom");
                              setAgentPermissionMatrix((prev) => ({
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
                      );
                    })}
                  </div>

                  {/* Compile Receipt & Downgrade Preview */}
                  <div
                    data-testid="agent-compile-receipt"
                    style={{
                      padding: "6px 8px",
                      borderRadius: "4px",
                      backgroundColor: "var(--bg-surface)",
                      border: "1px solid var(--border-subtle)",
                      display: "flex",
                      flexDirection: "column",
                      gap: "4px",
                      fontSize: "10.5px",
                      minWidth: 0,
                      overflowWrap: "anywhere",
                      wordBreak: "break-word",
                    }}
                  >
                    <div
                      className="fd-mono"
                      data-testid="agent-compile-native-flags"
                      style={{ color: "var(--accent-primary)" }}
                    >
                      Compiled ({compileReceipt.vendor}):{" "}
                      {compileReceipt.nativeFlagsOrConfig}
                    </div>
                    <div
                      className="fd-mono"
                      data-testid="agent-canonical-policy-preview"
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
                        data-testid="agent-compile-downgrades"
                        style={{ color: "#FBBF24" }}
                      >
                        Downgrade: {compileReceipt.downgrades.join(" ")}
                      </div>
                    )}
                    {compileReceipt.emulationNotices.length > 0 && (
                      <div
                        data-testid="agent-compile-emulation"
                        style={{ color: "var(--text-secondary)" }}
                      >
                        Emulated: {compileReceipt.emulationNotices.join(" ")}
                      </div>
                    )}
                    {compileReceipt.refused.length > 0 && (
                      <div
                        data-testid="agent-compile-refused"
                        style={{ color: "#F87171", fontWeight: 600 }}
                      >
                        Refused: {compileReceipt.refused.join(" ")}
                      </div>
                    )}
                  </div>

                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: "8px",
                      flexWrap: "wrap",
                    }}
                  >
                    <button
                      type="button"
                      className="fd-btn fd-btn-primary"
                      data-testid="agent-save-permissions-btn"
                      style={{ fontSize: "11px", padding: "4px 8px" }}
                      disabled={compileReceipt.refused.length > 0}
                      onClick={() => {
                        setAgentPermissionSavedStatus(
                          `Saved PermissionPolicy v${compileReceipt.policy.schema_version} (posture=${compileReceipt.policy.posture}, sandbox=${compileReceipt.policy.sandbox}, rules=${compileReceipt.policy.rules.length}) for ${displayTarget}.`,
                        );
                      }}
                    >
                      Save Permission Policy
                    </button>
                    {agentPermissionSavedStatus && (
                      <span
                        role="status"
                        data-testid="agent-permissions-saved-banner"
                        className="fd-mono"
                        style={{ fontSize: "10.5px", color: "#34D399" }}
                      >
                        {agentPermissionSavedStatus}
                      </span>
                    )}
                  </div>
                </div>
              );
            })()}
          </div>

          {/* Tab Panel 3: MCP & Runtime */}
          <div
            id="agent-settings-panel-runtime"
            role="tabpanel"
            aria-labelledby="agent-settings-tab-runtime"
            data-testid="agent-settings-panel-runtime"
            data-active={activeTab === "runtime" ? "true" : "false"}
            style={{
              display: activeTab === "runtime" ? "flex" : "none",
              flexDirection: "column",
              gap: "12px",
            }}
          >
            {/* 3-Tier MCP & Agent Posture Status (Record #95) */}
            {(() => {
              const mergedPostureInput: AgentMcpPostureInput = {
                agentName: agentName ?? displayTarget,
                project: agentProject,
                role: agentRole,
                adapter: effectiveAdapter,
                sessionId,
                ...postureInput,
                ...(postureOverride ?? {}),
              };
              const posture = evaluateAgentMcpPosture(mergedPostureInput);
              const isSynced = posture.driftKind === "synced";
              const badgeStyle: React.CSSProperties = isSynced
                ? {
                    backgroundColor: "rgba(16, 185, 129, 0.14)",
                    color: "#34D399",
                    borderColor: "rgba(16, 185, 129, 0.4)",
                    fontWeight: 700,
                  }
                : {
                    backgroundColor: "rgba(245, 158, 11, 0.16)",
                    color: "#FBBF24",
                    borderColor: "rgba(245, 158, 11, 0.45)",
                    fontWeight: 700,
                  };

              return (
                <div
                  data-testid="agent-posture-section"
                  className="fd-card"
                  style={{
                    padding: "10px 12px",
                    display: "flex",
                    flexDirection: "column",
                    gap: "8px",
                    backgroundColor: "var(--bg-elevated)",
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
                    <span
                      style={{
                        fontSize: "11px",
                        fontWeight: 600,
                        textTransform: "uppercase",
                        color: "var(--text-muted)",
                      }}
                    >
                      3-Tier MCP &amp; Agent Posture
                    </span>
                    <span
                      className="fd-badge"
                      data-testid="agent-posture-status-badge"
                      data-posture={posture.driftKind}
                      style={badgeStyle}
                    >
                      {posture.badgeLabel}
                    </span>
                  </div>

                  <div
                    data-testid="agent-posture-summary"
                    style={{
                      fontSize: "11.5px",
                      color: "var(--text-secondary)",
                      lineHeight: 1.4,
                    }}
                  >
                    {posture.summary}
                  </div>

                  <div
                    data-testid="agent-posture-tiers"
                    style={{
                      display: "grid",
                      gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
                      gap: "6px",
                      fontSize: "11px",
                    }}
                  >
                    <div
                      data-testid="posture-tier-desired"
                      style={{
                        padding: "6px 8px",
                        borderRadius: "4px",
                        backgroundColor: "var(--bg-surface)",
                        border: "1px solid var(--border-subtle)",
                        minWidth: 0,
                        overflowWrap: "anywhere",
                        wordBreak: "break-word",
                      }}
                    >
                      <div style={{ fontSize: "10px", color: "var(--text-muted)", fontWeight: 600 }}>
                        1. Desired Catalog
                      </div>
                      <div className="fd-mono" style={{ marginTop: "2px" }}>
                        {posture.desiredServers.join(", ")}
                      </div>
                    </div>

                    <div
                      data-testid="posture-tier-applied"
                      style={{
                        padding: "6px 8px",
                        borderRadius: "4px",
                        backgroundColor: "var(--bg-surface)",
                        border:
                          posture.missingInApplied.length > 0
                            ? "1px solid rgba(245, 158, 11, 0.5)"
                            : "1px solid var(--border-subtle)",
                        minWidth: 0,
                        overflowWrap: "anywhere",
                        wordBreak: "break-word",
                      }}
                    >
                      <div style={{ fontSize: "10px", color: "var(--text-muted)", fontWeight: 600 }}>
                        2. Applied ({posture.vendorConfigPath})
                      </div>
                      <div className="fd-mono" style={{ marginTop: "2px" }}>
                        {posture.appliedServers.join(", ") || "none"}
                      </div>
                    </div>

                    <div
                      data-testid="posture-tier-live"
                      style={{
                        padding: "6px 8px",
                        borderRadius: "4px",
                        backgroundColor: "var(--bg-surface)",
                        border:
                          posture.driftKind === "stale_session"
                            ? "1px solid rgba(245, 158, 11, 0.5)"
                            : "1px solid var(--border-subtle)",
                        minWidth: 0,
                        overflowWrap: "anywhere",
                        wordBreak: "break-word",
                      }}
                    >
                      <div style={{ fontSize: "10px", color: "var(--text-muted)", fontWeight: 600 }}>
                        3. Live-Attested
                      </div>
                      <div className="fd-mono" style={{ marginTop: "2px" }}>
                        {posture.liveServers.join(", ") || "none"}
                      </div>
                    </div>
                  </div>

                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: "6px",
                      flexWrap: "wrap",
                    }}
                  >
                    {posture.driftKind === "unapplied_catalog" && (
                      <button
                        type="button"
                        className="fd-btn fd-btn-primary"
                        data-testid="posture-remediate-sync-btn"
                        style={{ fontSize: "11px", padding: "4px 8px" }}
                        onClick={() => {
                          setPostureOverride((prev) => ({
                            ...(prev ?? {}),
                            appliedMcpServers: [...posture.desiredServers],
                          }));
                          setPostureActionStatus(
                            `Synced Desired catalog [${posture.desiredServers.join(", ")}] to ${posture.vendorConfigPath}.`,
                          );
                          onRemediatePosture?.("sync_mcp", posture);
                        }}
                      >
                        Sync MCP Config
                      </button>
                    )}

                    {posture.driftKind === "stale_session" && (
                      <button
                        type="button"
                        className="fd-btn fd-btn-primary"
                        data-testid="posture-remediate-reforge-btn"
                        style={{ fontSize: "11px", padding: "4px 8px" }}
                        onClick={() => {
                          setPostureOverride((prev) => ({
                            ...(prev ?? {}),
                            liveMcpServers: [...posture.appliedServers],
                            sessionStartedBeforeSync: false,
                            sessionRole: agentRole ?? undefined,
                            sessionProject: agentProject ?? undefined,
                          }));
                          setPostureActionStatus(
                            `Reforged live session for ${displayTarget} with latest MCP & role posture.`,
                          );
                          onRemediatePosture?.("reforge_session", posture);
                        }}
                      >
                        Reforge / Restart Session
                      </button>
                    )}

                    {posture.driftKind === "extra_server" && (
                      <button
                        type="button"
                        className="fd-btn fd-btn-primary"
                        data-testid="posture-remediate-prune-btn"
                        style={{ fontSize: "11px", padding: "4px 8px" }}
                        onClick={() => {
                          setPostureOverride((prev) => ({
                            ...(prev ?? {}),
                            appliedMcpServers: [...posture.desiredServers],
                            liveMcpServers: [...posture.desiredServers],
                          }));
                          setPostureActionStatus(
                            `Pruned unregistered MCP server(s) [${posture.extraInAppliedOrLive.join(", ")}] from ${posture.vendorConfigPath}.`,
                          );
                          onRemediatePosture?.("prune_extra", posture);
                        }}
                      >
                        Prune Extra Servers
                      </button>
                    )}
                  </div>

                  {postureActionStatus && (
                    <div
                      role="status"
                      data-testid="posture-remediate-status"
                      style={{
                        fontSize: "11px",
                        color: "#34D399",
                        padding: "4px 8px",
                        borderRadius: "4px",
                        backgroundColor: "rgba(16, 185, 129, 0.12)",
                      }}
                    >
                      {postureActionStatus}
                    </div>
                  )}
                </div>
              );
            })()}

            {/* Record #87: Worktree Policy, Bootstrap (.fl/worktree.toml) & Conflict-Free Port Allocation */}
            {(() => {
              const wtPreview = computeWorktreeBootstrapPreview(
                agentKey,
                agentProject || "FerroLoop",
                agentWorktreePolicy,
              );
              return (
                <div
                  data-testid="agent-worktree-policy-section"
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
                    <span style={{ fontWeight: 700, fontSize: "11px" }}>
                      Worktree Policy &amp; Dev-Server Ports
                    </span>
                    <span
                      className="fd-badge fd-mono"
                      data-testid="agent-allocated-ports-badge"
                      style={{
                        backgroundColor: "rgba(56, 189, 248, 0.14)",
                        borderColor: "rgba(56, 189, 248, 0.4)",
                        color: "var(--accent-primary)",
                        fontSize: "10.5px",
                      }}
                    >
                      PORT={wtPreview.allocatedPort} · VITE_PORT={wtPreview.allocatedVitePort}
                    </span>
                  </div>
                  <select
                    aria-label="Agent Worktree Policy"
                    data-testid="agent-worktree-policy-select"
                    className="fd-select fd-mono"
                    value={agentWorktreePolicy}
                    onChange={(e) =>
                      setAgentWorktreePolicy(e.target.value as WorktreePolicyMode)
                    }
                    style={{ width: "100%", fontSize: "11.5px" }}
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
                    data-testid="agent-bootstrap-hooks-preview"
                    style={{
                      fontSize: "10.5px",
                      color: "var(--text-muted)",
                      overflowWrap: "anywhere",
                      wordBreak: "break-word",
                    }}
                  >
                    {wtPreview.bootstrapConfigPath}: {wtPreview.bootstrapHooks.join(" && ")}
                  </div>
                </div>
              );
            })()}

            {/* Record #82 (Issue #217): Native fl host Supervisor Telemetry */}
            {(() => {
              const hostTelemetry = resolveAgentHostSupervisorTelemetry(
                adapter,
                agentName || sessionId,
              );
              return (
                <div
                  data-testid="agent-host-supervisor-telemetry"
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
                    <span style={{ fontWeight: 700, fontSize: "11px" }}>
                      Native `fl host` Supervisor Telemetry
                    </span>
                    <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                      <span
                        className="fd-badge fd-mono"
                        data-testid="agent-host-transport-mode-badge"
                        data-mode={hostTelemetry.transportMode}
                        style={{
                          backgroundColor: "rgba(52, 211, 153, 0.14)",
                          color: "#34d399",
                          fontSize: "10.5px",
                        }}
                      >
                        Transport: {hostTelemetry.transportMode}
                      </span>
                      <span
                        className="fd-badge fd-mono"
                        data-testid="agent-host-route-authority-badge"
                        style={{
                          backgroundColor: "rgba(56, 189, 248, 0.14)",
                          color: "#38bdf8",
                          fontSize: "10.5px",
                        }}
                      >
                        Route Authority: {hostTelemetry.routeAuthority}
                      </span>
                    </div>
                  </div>
                  <div
                    className="fd-mono"
                    data-testid="agent-host-ring-buffer-usage"
                    style={{
                      fontSize: "10.5px",
                      color: "var(--text-muted)",
                      overflowWrap: "anywhere",
                      wordBreak: "break-word",
                    }}
                  >
                    {hostTelemetry.supervisorUnit} · Ring Buffer:{" "}
                    {Math.round(hostTelemetry.ringBufferUsedBytes / 1024)} KB /{" "}
                    {Math.round(hostTelemetry.ringBufferCapacityBytes / 1024)} KB (
                    {Math.round(
                      (hostTelemetry.ringBufferUsedBytes /
                        hostTelemetry.ringBufferCapacityBytes) *
                        100,
                    )}
                    %)
                  </div>
                </div>
              );
            })()}
          </div>
        </div>

        {/* Pinned Bottom Lifecycle Footer */}
        <div
          data-testid="agent-settings-footer"
          style={{
            flexShrink: 0,
            paddingTop: "8px",
            borderTop: "1px solid var(--border-subtle)",
            backgroundColor: "var(--bg-surface)",
            display: "flex",
            flexDirection: "column",
            gap: "6px",
          }}
        >
          <div
            style={{
              fontSize: "11px",
              fontWeight: 600,
              color: "var(--text-muted)",
              textTransform: "uppercase",
            }}
          >
            Session Lifecycle Actions
          </div>
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
              className="fd-btn fd-btn-primary"
              data-testid={`recast-agent-btn-${agentKey}`}
              title="Reassign agent project or role in the registry without clearing the live session"
              onClick={() => {
                setRecastProject(agentProject || "Workspace");
                setRecastRole(canonicalizeDefaultRole(agentRole || "assistant"));
                setRecastConflict(null);
                setRecastError(null);
                setRecastModalOpen(true);
              }}
            >
              Re-Cast
            </button>

            <button
              type="button"
              className={`fd-btn${canCompact ? " fd-btn-primary" : ""}`}
              data-testid="compact-context-btn"
              disabled={!canCompact}
              title={
                contextEstimate.percent === null
                  ? "Context window unknown \u2014 compact anyway"
                  : contextAtCompactThreshold(contextEstimate)
                    ? "Snapshot a FerroState checkpoint, then send /compact to the session"
                    : "Available when context reaches 50%"
              }
              onClick={onCompactContext}
            >
              {compacting ? "Compacting..." : "Compact"}
            </button>

            <button
              type="button"
              className="fd-btn"
              data-testid="open-clear-reforge-modal-btn"
              disabled={!canClear}
              title="Clear session context or reforge with registered agent identity"
              onClick={() => {
                onClose();
                onOpenClearModal();
              }}
            >
              {clearing ? "Clearing..." : "Clear / Reforge"}
            </button>

            <button
              type="button"
              className="fd-btn fd-btn-danger"
              data-testid="open-decommission-modal-btn"
              disabled={!canDecommission}
              title="Prepare graceful decommission handshake or decommission the session immediately"
              onClick={() => {
                onClose();
                onOpenDecommissionModal();
              }}
            >
              {decommissioning ? "Decommissioning..." : "Decommission"}
            </button>
          </div>

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
        </div>

        {recastModalOpen && (() => {
          // The hub takes a project slug; the dropdown shows the display name.
          const { choices: projectOptions, value: recastProjectValue } =
            projectChoices(projects, recastProject);

          return (
            <div
              className="fd-modal-backdrop"
              onClick={() => {
                if (!recasting) {
                  setRecastModalOpen(false);
                }
              }}
            >
              <div
                role="dialog"
                aria-modal="true"
                aria-label={`Re-Cast Agent — ${displayTarget}`}
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
                    Re-Cast Agent — {displayTarget}
                  </h3>
                  <button
                    type="button"
                    className="fd-btn"
                    style={{ flexShrink: 0 }}
                    aria-label="Close Re-Cast Modal"
                    disabled={recasting}
                    onClick={() => setRecastModalOpen(false)}
                  >
                    ✕
                  </button>
                </div>

                {recastError && (
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
                    {recastError}
                  </div>
                )}

                <div style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
                  <div>
                    <label
                      htmlFor={`recast-project-select-${agentKey}`}
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
                      id={`recast-project-select-${agentKey}`}
                      aria-label={`Project for ${displayTarget}`}
                      data-testid={`recast-project-select-${agentKey}`}
                      className="fd-select fd-mono"
                      style={{ width: "100%", fontSize: "12px", padding: "6px 8px" }}
                      value={recastProjectValue}
                      onChange={(e) => {
                        setRecastProject(e.target.value);
                        setRecastConflict(null);
                      }}
                    >
                      {projectOptions.map((proj) => (
                        <option key={proj.value} value={proj.value}>
                          {proj.label}
                        </option>
                      ))}
                    </select>
                  </div>

                  <div>
                    <label
                      htmlFor={`recast-role-select-${agentKey}`}
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
                      id={`recast-role-select-${agentKey}`}
                      aria-label={`Role for ${displayTarget}`}
                      data-testid={`recast-role-select-${agentKey}`}
                      className="fd-select fd-mono"
                      style={{ width: "100%", fontSize: "12px", padding: "6px 8px" }}
                      value={recastRole}
                      onChange={(e) => {
                        setRecastRole(e.target.value);
                        setRecastConflict(null);
                      }}
                    >
                      {selectedRoleOptions(recastRoleOptions, recastRole, roleCatalog.roles).map((opt) => (
                        <option key={opt.value} value={opt.value}>
                          {opt.label}
                        </option>
                      ))}
                      <option value={CREATE_CUSTOM_ROLE_VALUE}>
                        + Create User Role...
                      </option>
                    </select>
                  </div>

                  {roleCatalog.loading && <div role="status">Loading roles…</div>}
                  {roleCatalog.error && <div role="status">Role catalog: {roleCatalog.error}. Existing roles remain available.</div>}
                  {recastRole === CREATE_CUSTOM_ROLE_VALUE && (
                    <div>
                      <label
                        htmlFor={`recast-custom-role-input-${agentKey}`}
                        style={{
                          display: "block",
                          fontSize: "11px",
                          fontWeight: 600,
                          color: "var(--text-muted)",
                          marginBottom: "4px",
                          textTransform: "uppercase",
                        }}
                      >
                        User Role Name
                      </label>
                      <input
                        id={`recast-custom-role-input-${agentKey}`}
                        type="text"
                        aria-label={`User role for ${displayTarget}`}
                        data-testid={`recast-custom-role-input-${agentKey}`}
                        className="fd-input fd-mono"
                        placeholder="e.g. reviewer or custom:reviewer"
                        style={{ width: "100%", fontSize: "12px", padding: "6px 8px" }}
                        value={recastCustomRoleInput}
                        onChange={(e) => setRecastCustomRoleInput(e.target.value)}
                      />
                    </div>
                  )}
                </div>

                {recastConflict && (
                  <div
                    data-testid={`recast-lead-warning-${agentKey}`}
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
                      {recastConflict.replaceLead
                        ? `Re-casting ${displayTarget} as Lead will replace the active Lead (${recastConflict.message}).`
                        : `Re-casting ${displayTarget} requires creating a new coverage area (${recastConflict.message}).`}
                    </span>
                    <button
                      type="button"
                      data-testid={`recast-confirm-lead-btn-${agentKey}`}
                      className="fd-btn fd-btn-danger"
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                      disabled={recasting}
                      onClick={() => void handleRecastSubmit(true)}
                    >
                      {recastConflict.replaceLead
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
                    data-testid={`recast-cancel-btn-${agentKey}`}
                    className="fd-btn"
                    disabled={recasting}
                    onClick={() => setRecastModalOpen(false)}
                  >
                    Cancel
                  </button>
                  <button
                    type="button"
                    data-testid={`recast-submit-btn-${agentKey}`}
                    className="fd-btn fd-btn-primary"
                    disabled={recasting}
                    onClick={() => void handleRecastSubmit(false)}
                  >
                    {recasting ? "Re-Casting..." : "Confirm Re-Cast"}
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
