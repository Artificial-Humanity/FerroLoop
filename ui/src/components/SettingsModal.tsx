import React, { useEffect, useMemo, useState } from "react";
import { HubApiClient, HubApiError, generatePasskeyCredentialId } from "../api/client";
import { TIMEZONE_OPTIONS, setActiveTimeZone } from "../lib/datetime";
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
  type TotpEnrollmentResponse,
} from "../types/api";

export type SettingsTabId = "agents" | "system" | "auth";

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
  onOpenSpinUpModal?: () => void;
  onOpenPairingModal?: () => void;
  onLogout?: () => void;
  onAgentsUpdated?: () => void;
  onDecommissionAgent?: (
    targetId: string,
    mode: DecommissionMode,
  ) => Promise<unknown>;
  onBulkSessionAction?: (
    action: BulkSessionAction,
  ) => Promise<BulkSessionActionResponse>;
  onSettingsUpdated?: (settings: HubSettings) => void;
  apiClient?: HubApiClient;
}

const CANONICAL_PROJECTS = [
  "AI-Lab-AMD",
  "FerroLoop",
  "Kaggle-Gemma4",
  "Lucida",
  "Prosodia",
  "Sonora",
  "Website",
  "Workspace",
];

const RECAST_ROLE_OPTIONS = [
  { value: "lead", label: "lead" },
  { value: "assistant", label: "assistant" },
  { value: "lab_manager", label: "lab_manager" },
  { value: "custom:ui-developer", label: "custom:ui-developer" },
  { value: "custom:scribe", label: "custom:scribe" },
];

const ACTION_LABELS: Record<BulkSessionAction, string> = {
  compact: "Compact All",
  clear: "Clear All",
  reforge: "Reforge All",
};

const ACTION_DESCRIPTIONS: Record<BulkSessionAction, string> = {
  compact:
    "Send /compact to every active session across the workspace fleet.",
  clear:
    "Clear conversation context across all active sessions, leaving each session blank.",
  reforge:
    "Clear conversation context and immediately re-prime every active session with its registered identity, project, and role prompt.",
};

export const SettingsModal: React.FC<SettingsModalProps> = ({
  isOpen,
  onClose,
  initialTab = "agents",
  initialProject = null,
  agents: propAgents,
  authority,
  wsConnected = false,
  onlineNodesCount = 0,
  activeCockpitTab,
  onSelectCockpitTab,
  onOpenNodesModal,
  onOpenSpinUpModal,
  onOpenPairingModal,
  onLogout,
  onAgentsUpdated,
  onDecommissionAgent,
  onBulkSessionAction,
  onSettingsUpdated,
  apiClient,
}) => {
  const [activeTab, setActiveTab] = useState<SettingsTabId>(initialTab);
  const [selectedProjectFilter, setSelectedProjectFilter] = useState<string>(
    initialProject || "all",
  );
  const [selectedAgentKeys, setSelectedAgentKeys] = useState<Set<string>>(
    () => new Set(),
  );
  const [settings, setSettings] = useState<HubSettings>(DEFAULT_HUB_SETTINGS);
  const [savingSettings, setSavingSettings] = useState(false);
  const [settingsSaved, setSettingsSaved] = useState(false);

  // Agents Fleet & Re-Cast state
  const [loadedAgents, setLoadedAgents] = useState<AgentRecord[]>(
    propAgents && propAgents.length > 0 ? propAgents : DEFAULT_WORKSPACE_AGENTS,
  );
  const [recastDrafts, setRecastDrafts] = useState<
    Record<string, { project: string; role: string }>
  >({});
  const [recastReplaceLeadArmed, setRecastReplaceLeadArmed] = useState<
    Record<string, string>
  >({});
  const [recastingAgentKey, setRecastingAgentKey] = useState<string | null>(
    null,
  );
  const [recastStatusBanner, setRecastStatusBanner] = useState<string | null>(
    null,
  );
  const [decommissioningKey, setDecommissioningKey] = useState<string | null>(
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
    setSelectedProjectFilter(initialProject || "all");
    if (initialProject) {
      setActiveTab("agents");
    }
  }, [initialProject]);

  useEffect(() => {
    setArmedAction(null);
    setError(null);
    setLastResult(null);
    setSettingsSaved(false);
    setCloudAdaptersSaved(false);
    setRecastStatusBanner(null);
    setRecastReplaceLeadArmed({});
    setSelectedAgentKeys(new Set());
    setSelectedProjectFilter(initialProject || "all");
    if (initialProject) {
      setActiveTab("agents");
    }
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

    return () => {
      cancelled = true;
    };
  }, [isOpen, apiClient]);

  useEffect(() => {
    if (!isOpen) return;
    const handleKeyDownEscape = (event: KeyboardEvent) => {
      if (
        event.key === "Escape" &&
        !submitting &&
        !savingSettings &&
        !savingCloudAdapters
      ) {
        onClose();
      }
    };
    window.addEventListener("keydown", handleKeyDownEscape);
    return () => window.removeEventListener("keydown", handleKeyDownEscape);
  }, [isOpen, submitting, savingSettings, savingCloudAdapters, onClose]);

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

  const filteredAgents = useMemo<AgentRecord[]>(() => {
    if (!selectedProjectFilter || selectedProjectFilter === "all") {
      return effectiveAgents;
    }
    const target = selectedProjectFilter.toLowerCase();
    return effectiveAgents.filter(
      (ag) => (ag.project || "Workspace").toLowerCase() === target,
    );
  }, [effectiveAgents, selectedProjectFilter]);

  const allFilteredSelected = useMemo(() => {
    if (filteredAgents.length === 0) return false;
    return filteredAgents.every((ag) =>
      selectedAgentKeys.has(ag.name_key.toLowerCase()),
    );
  }, [filteredAgents, selectedAgentKeys]);

  const projectSummaryLead = useMemo(() => {
    if (!selectedProjectFilter || selectedProjectFilter === "all") return null;
    const lead = filteredAgents.find(
      (ag) => ag.role === "lead" || ag.role === "lab_manager",
    );
    return lead ? lead.display_name : "Unassigned";
  }, [filteredAgents, selectedProjectFilter]);

  const authorityText = useMemo(() => {
    if (!authority) return "Unauthenticated";
    if (authority.tier === "owner") {
      return `Owner: ${authority.login}`;
    }
    return authority.agent ? `Peer: ${authority.agent}` : "Peer";
  }, [authority]);

  if (!isOpen) {
    return null;
  }

  const getDraftForAgent = (ag: AgentRecord) => {
    const key = ag.name_key.toLowerCase();
    return (
      recastDrafts[key] ?? {
        project: ag.project || "Workspace",
        role: ag.role || "assistant",
      }
    );
  };

  const handleRecastAgent = async (
    ag: AgentRecord,
    confirmOverrides = false,
  ) => {
    const key = ag.name_key.toLowerCase();
    const draft = getDraftForAgent(ag);
    if (recastingAgentKey) return;
    setRecastingAgentKey(key);
    setError(null);
    setRecastStatusBanner(null);
    try {
      const client = apiClient ?? new HubApiClient();
      const res = await client.recastAgent(key, {
        project: draft.project,
        role: draft.role,
        replace_lead: confirmOverrides,
        create_area: confirmOverrides,
      });
      if (
        (res?.retires_lead ||
          (Array.isArray(res?.creates_areas) && res.creates_areas.length > 0)) &&
        !confirmOverrides
      ) {
        const warningParts: string[] = [];
        if (res.retires_lead) {
          warningParts.push(`retires active Lead (${res.retires_lead})`);
        }
        if (Array.isArray(res.creates_areas) && res.creates_areas.length > 0) {
          warningParts.push(`creates area (${res.creates_areas.join(", ")})`);
        }
        setRecastReplaceLeadArmed((prev) => ({
          ...prev,
          [key]: warningParts.join("; "),
        }));
        return;
      }
      const updatedAgent: AgentRecord = res?.agent ??
        res?.after ?? {
          ...ag,
          project: draft.project,
          role: draft.role,
        };
      setLoadedAgents((prev) =>
        prev.map((item) =>
          item.name_key.toLowerCase() === key ? updatedAgent : item,
        ),
      );
      setRecastReplaceLeadArmed((prev) => {
        const next = { ...prev };
        delete next[key];
        return next;
      });
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
        setRecastReplaceLeadArmed((prev) => ({
          ...prev,
          [key]: err.message || "existing Lead",
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

  const handleDecommissionForAgent = async (
    ag: AgentRecord,
    mode: DecommissionMode,
  ) => {
    const key = ag.name_key.toLowerCase();
    if (decommissioningKey) return;
    setDecommissioningKey(`${key}:${mode}`);
    setError(null);
    setRecastStatusBanner(null);
    try {
      if (onDecommissionAgent) {
        await onDecommissionAgent(key, mode);
      } else {
        const client = apiClient ?? new HubApiClient();
        await client.decommissionSession(key, mode);
      }
      setRecastStatusBanner(
        mode === "prepare"
          ? `Dispatched decommission preparation to ${ag.display_name}.`
          : `Completed session teardown for ${ag.display_name}.`,
      );
      onAgentsUpdated?.();
    } catch (err) {
      setError(
        err instanceof Error
          ? err.message
          : `Failed to ${mode} session for ${ag.display_name}`,
      );
    } finally {
      setDecommissioningKey(null);
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
    try {
      let res: BulkSessionActionResponse;
      if (onBulkSessionAction) {
        res = await onBulkSessionAction(armedAction);
      } else if (apiClient) {
        res = await apiClient.bulkSessionAction(armedAction);
      } else {
        const client = new HubApiClient();
        res = await client.bulkSessionAction(armedAction);
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

  const systemViewTabs: Array<{ id: CockpitTab; label: string; testId: string }> = [
    { id: "terminal", label: "Terminal", testId: "settings-open-terminal-btn" },
    { id: "hub", label: "Hub & Arch", testId: "settings-open-hub-btn" },
    { id: "gate", label: "Vault & Gate", testId: "settings-open-gate-btn" },
    { id: "state", label: "FerroState", testId: "settings-open-state-btn" },
  ];

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
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: "12px",
          }}
        >
          <h2 style={{ margin: 0, fontSize: "16px", fontWeight: 700 }}>
            Settings &amp; Fleet Operations
          </h2>
          <button
            type="button"
            className="fd-btn"
            onClick={onClose}
            disabled={submitting || savingSettings || savingCloudAdapters}
            aria-label="Close Settings Modal"
          >
            ✕
          </button>
        </div>

        {/* 3-Tab Navigation Header */}
        <div
          role="tablist"
          aria-label="Settings Categories"
          data-testid="settings-tab-bar"
          style={{
            display: "flex",
            alignItems: "center",
            gap: "6px",
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
            {effectiveAgents.length} Agents
          </button>
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
            aria-selected={activeTab === "auth"}
            data-testid="settings-tab-auth"
            className={`fd-btn${activeTab === "auth" ? " fd-btn-active" : ""}`}
            onClick={() => setActiveTab("auth")}
          >
            Auth
          </button>
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
                  {effectiveAgents.length} Agents
                </h3>
                <p
                  style={{
                    margin: 0,
                    fontSize: "12px",
                    color: "var(--text-secondary)",
                  }}
                >
                  Spin up new sessions, re-cast an agent&apos;s project or role, or
                  decommission sessions across the fleet.
                </p>
              </div>
              <button
                type="button"
                data-testid="settings-spin-up-btn"
                className="fd-btn fd-btn-primary"
                onClick={() => {
                  onOpenSpinUpModal?.();
                  onClose();
                }}
              >
                + Spin Up Agent
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
                  value={selectedProjectFilter}
                  onChange={(e) => setSelectedProjectFilter(e.target.value)}
                >
                  <option value="all">All Projects ({effectiveAgents.length})</option>
                  {CANONICAL_PROJECTS.map((proj) => (
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
            {selectedProjectFilter !== "all" && (
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
                      {selectedProjectFilter}
                    </strong>
                    <span className="fd-badge fd-mono" data-testid="settings-project-summary-lead">
                      Lead: {projectSummaryLead}
                    </span>
                    <span className="fd-badge fd-mono">
                      {filteredAgents.length} {filteredAgents.length === 1 ? "agent" : "agents"}
                    </span>
                  </div>

                  <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
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
                    <button
                      type="button"
                      data-testid="settings-project-spin-up-btn"
                      className="fd-btn fd-btn-primary"
                      style={{ fontSize: "11px", padding: "3px 8px" }}
                      onClick={() => {
                        onOpenSpinUpModal?.();
                        onClose();
                      }}
                    >
                      + Spin Up in {selectedProjectFilter}
                    </button>
                  </div>
                </div>
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
                const draft = getDraftForAgent(ag);
                const armedLeadWarning = recastReplaceLeadArmed[key];
                const projectOptions = CANONICAL_PROJECTS.includes(draft.project)
                  ? CANONICAL_PROJECTS
                  : [draft.project, ...CANONICAL_PROJECTS];
                const hasRoleOption = RECAST_ROLE_OPTIONS.some(
                  (opt) => opt.value === draft.role,
                );

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
                          {ag.role}
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
                        <select
                          aria-label={`Project for ${ag.display_name}`}
                          data-testid={`recast-project-select-${key}`}
                          className="fd-select fd-mono"
                          style={{ fontSize: "11.5px", padding: "3px 6px" }}
                          value={draft.project}
                          onChange={(e) => {
                            const nextProject = e.target.value;
                            setRecastDrafts((prev) => ({
                              ...prev,
                              [key]: {
                                ...getDraftForAgent(ag),
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

                        <select
                          aria-label={`Role for ${ag.display_name}`}
                          data-testid={`recast-role-select-${key}`}
                          className="fd-select fd-mono"
                          style={{ fontSize: "11.5px", padding: "3px 6px" }}
                          value={draft.role}
                          onChange={(e) => {
                            const nextRole = e.target.value;
                            setRecastDrafts((prev) => ({
                              ...prev,
                              [key]: {
                                ...getDraftForAgent(ag),
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
                        </select>

                        <button
                          type="button"
                          data-testid={`recast-agent-btn-${key}`}
                          className="fd-btn fd-btn-primary"
                          style={{ fontSize: "11px", padding: "3px 8px" }}
                          disabled={recastingAgentKey === key}
                          onClick={() => void handleRecastAgent(ag, false)}
                        >
                          {recastingAgentKey === key ? "Re-Casting..." : "Re-Cast"}
                        </button>

                        <button
                          type="button"
                          data-testid={`settings-decommission-prepare-${key}`}
                          className="fd-btn"
                          style={{ fontSize: "11px", padding: "3px 8px" }}
                          disabled={decommissioningKey === `${key}:prepare`}
                          onClick={() =>
                            void handleDecommissionForAgent(ag, "prepare")
                          }
                          title={`Prepare ${ag.display_name} for decommission`}
                        >
                          Prepare
                        </button>

                        <button
                          type="button"
                          data-testid={`settings-decommission-teardown-${key}`}
                          className="fd-btn fd-btn-danger"
                          style={{ fontSize: "11px", padding: "3px 8px" }}
                          disabled={decommissioningKey === `${key}:teardown`}
                          onClick={() =>
                            void handleDecommissionForAgent(ag, "teardown")
                          }
                          title={`Tear down ${ag.display_name} session`}
                        >
                          Tear Down
                        </button>
                      </div>
                    </div>

                    {armedLeadWarning && (
                      <div
                        data-testid={`recast-lead-warning-${key}`}
                        style={{
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "space-between",
                          gap: "8px",
                          padding: "6px 8px",
                          borderRadius: "4px",
                          backgroundColor: "rgba(245, 158, 11, 0.14)",
                          border: "1px solid rgba(245, 158, 11, 0.4)",
                          fontSize: "11.5px",
                        }}
                      >
                        <span>
                          Re-casting {ag.display_name} as Lead will replace the
                          active Lead ({armedLeadWarning}).
                        </span>
                        <button
                          type="button"
                          data-testid={`recast-confirm-lead-btn-${key}`}
                          className="fd-btn fd-btn-danger"
                          style={{ fontSize: "11px", padding: "2px 8px" }}
                          onClick={() => void handleRecastAgent(ag, true)}
                        >
                          Confirm Replace Lead
                        </button>
                      </div>
                    )}
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
                  <strong>Confirm {ACTION_LABELS[armedAction]}:</strong>{" "}
                  {ACTION_DESCRIPTIONS[armedAction]}
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
                      armedAction === "clear"
                        ? "fd-btn fd-btn-danger"
                        : "fd-btn fd-btn-primary"
                    }
                    data-testid="bulk-confirm-btn"
                    disabled={submitting}
                    onClick={() => void handleConfirmBulkAction()}
                  >
                    {submitting
                      ? "Running..."
                      : `Confirm ${ACTION_LABELS[armedAction]}`}
                  </button>
                </div>
              </div>
            )}
          </div>
        </div>

        {/* TAB 2: System */}
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
          {/* System Views & Fleet Nodes Launcher (moved from ContextHeader SYSTEM row) */}
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
                  System Consoles &amp; Fleet Nodes
                </h3>
                <span style={{ fontSize: "11.5px", color: "var(--text-muted)" }}>
                  Open system-level FerroLoop consoles or inspect enrolled compute
                  nodes.
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
              {systemViewTabs.map((item) => (
                <button
                  key={item.id}
                  type="button"
                  data-testid={item.testId}
                  className={`fd-btn${activeCockpitTab === item.id ? " fd-btn-active" : ""}`}
                  onClick={() => {
                    onSelectCockpitTab?.(item.id);
                    onClose();
                  }}
                >
                  {item.label}
                </button>
              ))}
              <button
                type="button"
                data-testid="settings-open-nodes-btn"
                className="fd-btn"
                onClick={() => {
                  onOpenNodesModal?.();
                  onClose();
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

            <hr
              style={{
                border: "none",
                borderTop: "1px solid var(--border-subtle)",
                margin: 0,
              }}
            />

            {/* Subsystem Toggles */}
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
                FerroLoop Installed Products &amp; Subsystem Toggles
              </h3>
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
                    data-testid="toggle-ferrogate"
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
                  <span>FerroGate Vault &amp; Proxy</span>
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
                    data-testid="toggle-ferrostate"
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
                  <span>FerroState Memory &amp; Index</span>
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
                  <span>FerroStep Workflows</span>
                </label>
              </div>
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
      </div>
    </div>
  );
};
