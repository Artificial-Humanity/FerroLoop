import React, { useCallback, useEffect, useState } from "react";
import type { HubApiClient } from "../api/client";
import { formatTimestamp, useActiveTimeZone } from "../lib/datetime";
import type {
  DeleteGateSecretResponse,
  GateBridgeSummary,
  GateConnectorSummary,
  GateSecretResponse,
  HubAuthority,
  SetVaultSecretInput,
  VaultSecretMetadata,
} from "../types/api";

export interface VaultGatePaneProps {
  secrets?: VaultSecretMetadata[];
  connectors?: GateConnectorSummary[];
  accessBundles?: GateAccessBundle[];
  admissionSlices?: GateAdmissionSlice[];
  failoverRoutes?: GateFailoverRoute[];
  outputPolicies?: GateOutputPolicyStatus[];
  authority?: HubAuthority | null;
  loading?: boolean;
  error?: string | null;
  onRefresh?: () => void;
  onSetSecret?: (input: SetVaultSecretInput) => Promise<GateSecretResponse> | void;
  onDeleteSecret?: (name: string) => Promise<DeleteGateSecretResponse> | void;
  onRevokeBundle?: (bundleId: string) => Promise<void> | void;
  apiClient?: HubApiClient;
}

// Record #99: Session-Scoped Revocable Access Bundles
export interface GateAccessBundle {
  bundle_id: string;
  session_id: string;
  persona: string;
  project: string;
  warrant_id: string;
  secrets: string[];
  issued_at: string;
  expires_at: string;
  status: "active" | "revoked";
}

export const DEFAULT_GATE_ACCESS_BUNDLES: GateAccessBundle[] = [
  {
    bundle_id: "bnd-sess-ferris-01",
    session_id: "sess-ferris-lead",
    persona: "ferris",
    project: "FerroLoop",
    warrant_id: "wrt-fl-8901",
    secrets: ["GITHUB_APP_TOKEN", "DISCORD_BOT_TOKEN"],
    issued_at: "2026-11-03T04:00:00Z",
    expires_at: "2026-11-03T08:00:00Z",
    status: "active",
  },
  {
    bundle_id: "bnd-sess-felix-02",
    session_id: "sess-felix-ui",
    persona: "felix",
    project: "FerroLoop",
    warrant_id: "wrt-fl-8902",
    secrets: ["GITHUB_APP_TOKEN"],
    issued_at: "2026-11-03T04:15:00Z",
    expires_at: "2026-11-03T08:15:00Z",
    status: "active",
  },
];

// Record #97: Shared Gateway Rate/Budget Admission & Concurrency
export interface GateAdmissionSlice {
  id: string;
  principal: string;
  project: string;
  endpoint: string;
  in_flight: number;
  max_concurrency: number;
  req_per_min: number;
  rpm_limit: number;
  cost_usd_used: number;
  cost_usd_limit: number;
}

export const DEFAULT_GATE_ADMISSION_SLICES: GateAdmissionSlice[] = [
  {
    id: "adm-ferroloop-github",
    principal: "ferris",
    project: "FerroLoop",
    endpoint: "api.github.com",
    in_flight: 2,
    max_concurrency: 8,
    req_per_min: 18,
    rpm_limit: 60,
    cost_usd_used: 0.42,
    cost_usd_limit: 5.0,
  },
  {
    id: "adm-ferroloop-llm",
    principal: "felix",
    project: "FerroLoop",
    endpoint: "api.anthropic.com",
    in_flight: 1,
    max_concurrency: 4,
    req_per_min: 9,
    rpm_limit: 30,
    cost_usd_used: 1.85,
    cost_usd_limit: 10.0,
  },
];

// Record #98: Bounded Endpoint Failover Health & Deadline Status
export interface GateFailoverRoute {
  route_id: string;
  connector_id: string;
  primary_endpoint: string;
  fallback_endpoint: string;
  health_status: "healthy" | "degraded-failover" | "circuit-open";
  deadline_ms: number;
  max_retries: number;
  active_target: "primary" | "fallback";
}

export const DEFAULT_GATE_FAILOVER_ROUTES: GateFailoverRoute[] = [
  {
    route_id: "route-github-api",
    connector_id: "github",
    primary_endpoint: "https://api.github.com",
    fallback_endpoint: "https://uploads.github.com",
    health_status: "healthy",
    deadline_ms: 4500,
    max_retries: 2,
    active_target: "primary",
  },
  {
    route_id: "route-llm-gateway",
    connector_id: "llm-proxy",
    primary_endpoint: "https://api.anthropic.com/v1",
    fallback_endpoint: "https://openrouter.ai/api/v1",
    health_status: "degraded-failover",
    deadline_ms: 12000,
    max_retries: 1,
    active_target: "fallback",
  },
];

// Record #96: Strict Output Schema Validation & Byte/Token Cap Status
export interface GateOutputPolicyStatus {
  connector_id: string;
  tool_name: string;
  schema_mode: "strict-json-schema" | "passthrough";
  max_response_bytes: number;
  max_response_tokens: number;
  truncation_policy: "reject-on-overflow" | "truncate-with-receipt";
  validated_calls: number;
  rejected_overflows: number;
}

export const DEFAULT_GATE_OUTPUT_POLICIES: GateOutputPolicyStatus[] = [
  {
    connector_id: "github",
    tool_name: "list_pull_requests",
    schema_mode: "strict-json-schema",
    max_response_bytes: 262144,
    max_response_tokens: 8192,
    truncation_policy: "truncate-with-receipt",
    validated_calls: 142,
    rejected_overflows: 0,
  },
  {
    connector_id: "discord",
    tool_name: "send_message",
    schema_mode: "strict-json-schema",
    max_response_bytes: 65536,
    max_response_tokens: 2048,
    truncation_policy: "reject-on-overflow",
    validated_calls: 64,
    rejected_overflows: 1,
  },
];

// Record #81 (Gate G5): Committed .fl/mcp.toml Workspace Catalog & Vendor Session MCP Deduplication
export type McpCatalogRepoStatus =
  | "synced"
  | "drift"
  | "missing"
  | "error"
  | "not-checked";

export interface McpCatalogChange {
  vendor: string;
  server: string;
  action: "add" | "update" | "remove";
}

export interface McpCatalogRepoRow {
  repo: string;
  root?: string;
  catalog_path: string;
  status: McpCatalogRepoStatus;
  deduplicated_daemons: string[];
  active_vendor_sessions: number | null;
  spawned_mcp_processes: number | null;
  enabled_servers?: string[] | null;
  vendors?: Record<string, "synced" | "drift" | "refused"> | null;
  changes?: McpCatalogChange[] | null;
  catalog_digest?: string | null;
  error?: string | null;
  refusals?: string[];
}

export const DEFAULT_MCP_CATALOG_REPOS: McpCatalogRepoRow[] = [
  {
    repo: "FerroLoop",
    catalog_path: "FerroLoop/.fl/mcp.toml",
    status: "not-checked",
    deduplicated_daemons: [],
    active_vendor_sessions: null,
    spawned_mcp_processes: null,
    enabled_servers: null,
    changes: null,
    catalog_digest: null,
    error: null,
    refusals: [],
  },
  {
    repo: "AI-Lab-AMD",
    catalog_path: "AI-Lab-AMD/.fl/mcp.toml",
    status: "not-checked",
    deduplicated_daemons: [],
    active_vendor_sessions: null,
    spawned_mcp_processes: null,
    enabled_servers: null,
    changes: null,
    catalog_digest: null,
    error: null,
    refusals: [],
  },
  {
    repo: "Sonora",
    catalog_path: "Sonora/.fl/mcp.toml",
    status: "not-checked",
    deduplicated_daemons: [],
    active_vendor_sessions: null,
    spawned_mcp_processes: null,
    enabled_servers: null,
    changes: null,
    catalog_digest: null,
    error: null,
    refusals: [],
  },
];

// Record #86 (Gate G6): FerroGate Vault Consumer Cutover & Signing Broker Attestation
export type VaultConsumerCutoverStatus =
  | "cutover-complete"
  | "pending-cutover"
  | "unverified";

export interface VaultConsumerCutoverRow {
  id: string;
  consumer_label: string;
  source_module: string;
  binding: string;
  plaintext_secrets: number | null;
  host_allowlist: string[];
  status: VaultConsumerCutoverStatus;
}

export const DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS: VaultConsumerCutoverRow[] = [
  {
    id: "consumer-config-tracker",
    consumer_label: "config.toml Tracker Credentials",
    source_module: "crates/core/src/config.rs",
    binding: "vault://GITHUB_APP_TOKEN",
    plaintext_secrets: null,
    host_allowlist: ["api.github.com"],
    status: "pending-cutover",
  },
  {
    id: "consumer-github-signing-broker",
    consumer_label: "GitHub App Key Signing Broker",
    source_module: "crates/github/src/creds.rs",
    binding: "vault://GITHUB_APP_PRIVATE_KEY (in-enclave JWT signing)",
    plaintext_secrets: null,
    host_allowlist: ["api.github.com", "uploads.github.com"],
    status: "pending-cutover",
  },
  {
    id: "consumer-discord-bridge",
    consumer_label: "FerroWire Discord Bridge Consumer",
    source_module: "crates/gate/src/connectors.rs",
    binding: "vault://DISCORD_BOT_TOKEN",
    plaintext_secrets: null,
    host_allowlist: ["discord.com"],
    status: "pending-cutover",
  },
];

/** Mirrors `normalize_secret_name` in `crates/gate/src/vault.rs` (server uppercases). */
const SECRET_NAME_PATTERN = /^[A-Z0-9_]{1,64}$/;

function parseCommaList(raw: string): string[] {
  return raw
    .split(",")
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

function errorMessage(err: unknown, fallback: string): string {
  return err instanceof Error ? err.message : fallback;
}

const inputStyle: React.CSSProperties = {
  width: "100%",
  minWidth: 0,
  maxWidth: "100%",
  boxSizing: "border-box",
  // 16px stops iPadOS Safari from zooming the viewport on focus.
  fontSize: "16px",
  minHeight: "40px",
};

const labelStyle: React.CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: "4px",
  fontSize: "12px",
  fontWeight: 600,
  color: "var(--text-secondary)",
  minWidth: 0,
  maxWidth: "100%",
};

function Badges({ items, emptyLabel }: { items: string[]; emptyLabel: string }) {
  if (items.length === 0) {
    return <span style={{ color: "var(--text-dim)", fontSize: "12px" }}>{emptyLabel}</span>;
  }
  return (
    <>
      {items.map((item) => (
        <span key={item} className="fd-badge">
          {item}
        </span>
      ))}
    </>
  );
}

function channelRouteEntries(bridge: GateBridgeSummary): Array<[string, string]> {
  if (!bridge.channel_routes || typeof bridge.channel_routes !== "object") {
    return [];
  }
  return Object.entries(bridge.channel_routes);
}

export const VaultGatePane: React.FC<VaultGatePaneProps> = ({
  secrets,
  connectors,
  accessBundles: propBundles,
  admissionSlices: propAdmissionSlices,
  failoverRoutes: propFailoverRoutes,
  outputPolicies: propOutputPolicies,
  authority = null,
  loading = false,
  error = null,
  onRefresh,
  onSetSecret,
  onDeleteSecret,
  onRevokeBundle,
  apiClient,
}) => {
  const isOwner = authority?.tier === "owner";
  const activeTimeZone = useActiveTimeZone();

  const [bundlesState, setBundlesState] = useState<GateAccessBundle[]>(
    propBundles ?? DEFAULT_GATE_ACCESS_BUNDLES,
  );
  const [bundleStatusBanner, setBundleStatusBanner] = useState<string | null>(
    null,
  );
  useEffect(() => {
    if (propBundles) {
      setBundlesState(propBundles);
    }
  }, [propBundles]);

  const effectiveAdmissionSlices =
    propAdmissionSlices ?? DEFAULT_GATE_ADMISSION_SLICES;
  const effectiveFailoverRoutes =
    propFailoverRoutes ?? DEFAULT_GATE_FAILOVER_ROUTES;
  const effectiveOutputPolicies =
    propOutputPolicies ?? DEFAULT_GATE_OUTPUT_POLICIES;

  // Record #81 (Gate G5): .fl/mcp.toml Workspace Catalog & Vendor Session MCP Deduplication state
  const [mcpCatalogRepos, setMcpCatalogRepos] = useState<McpCatalogRepoRow[]>(
    DEFAULT_MCP_CATALOG_REPOS,
  );
  const [mcpCatalogVerified, setMcpCatalogVerified] = useState(false);
  const [mcpCatalogBanner, setMcpCatalogBanner] = useState<string | null>(null);
  const [mcpCatalogBusy, setMcpCatalogBusy] = useState(false);

  // Record #86 (Gate G6): FerroGate Vault Consumer Cutover & Signing Broker Attestation state
  const [vaultConsumerRows, setVaultConsumerRows] = useState<
    VaultConsumerCutoverRow[]
  >(DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS);
  const [vaultConsumersVerified, setVaultConsumersVerified] = useState(false);
  const [vaultConsumersBanner, setVaultConsumersBanner] = useState<
    string | null
  >(null);
  const [vaultConsumersBusy, setVaultConsumersBusy] = useState(false);

  const normalizeMcpCatalogRow = (
    raw: Partial<McpCatalogRepoRow> & { repo: string; catalog_path: string },
  ): McpCatalogRepoRow => ({
    repo: raw.repo,
    root: raw.root,
    catalog_path: raw.catalog_path,
    status: raw.status ?? "not-checked",
    deduplicated_daemons: Array.isArray(raw.deduplicated_daemons)
      ? raw.deduplicated_daemons
      : [],
    active_vendor_sessions:
      typeof raw.active_vendor_sessions === "number"
        ? raw.active_vendor_sessions
        : null,
    spawned_mcp_processes:
      typeof raw.spawned_mcp_processes === "number"
        ? raw.spawned_mcp_processes
        : null,
    enabled_servers: Array.isArray(raw.enabled_servers)
      ? raw.enabled_servers
      : null,
    vendors:
      raw.vendors && typeof raw.vendors === "object" ? raw.vendors : null,
    changes: Array.isArray(raw.changes) ? raw.changes : null,
    catalog_digest:
      typeof raw.catalog_digest === "string" &&
      raw.catalog_digest.trim().length > 0
        ? raw.catalog_digest.trim()
        : null,
    error: typeof raw.error === "string" ? raw.error : null,
    refusals: Array.isArray(raw.refusals) ? raw.refusals : [],
  });

  const fetchMcpCatalogFromHub = async (): Promise<boolean> => {
    if (typeof globalThis.fetch !== "function") {
      return false;
    }
    const res = await globalThis.fetch("/api/v1/mcp/catalog");
    if (!res || !res.ok) {
      return false;
    }
    const json = (await res.json()) as {
      repos?: Array<
        Partial<McpCatalogRepoRow> & { repo: string; catalog_path: string }
      >;
    };
    if (json && Array.isArray(json.repos) && json.repos.length > 0) {
      setMcpCatalogRepos(json.repos.map(normalizeMcpCatalogRow));
    }
    setMcpCatalogVerified(true);
    return true;
  };

  const handleCheckMcpCatalog = async () => {
    setMcpCatalogBusy(true);
    try {
      const ok = await fetchMcpCatalogFromHub();
      if (ok) {
        setMcpCatalogBanner(
          "Checked committed .fl/mcp.toml catalogs across workspace repositories (fl mcp check).",
        );
        return;
      }
      setMcpCatalogBanner(
        "Catalog check failed: backend /api/v1/mcp/catalog unavailable (showing unverified state).",
      );
    } catch {
      setMcpCatalogBanner(
        "Catalog check failed: backend /api/v1/mcp/catalog unavailable (showing unverified state).",
      );
    } finally {
      setMcpCatalogBusy(false);
    }
  };

  const syncableMcpRepos = mcpCatalogRepos.filter(
    (
      row,
    ): row is McpCatalogRepoRow & {
      catalog_digest: string;
    } =>
      typeof row.catalog_digest === "string" &&
      row.catalog_digest.trim().length > 0,
  );

  const handleSyncMcpCatalog = async (targetRepo?: string) => {
    const targets = targetRepo
      ? syncableMcpRepos.filter((row) => row.repo === targetRepo)
      : syncableMcpRepos;
    if (targets.length === 0) {
      setMcpCatalogBanner(
        "Sync refused: run Check Catalog (fl mcp check) first to obtain a catalog_digest before syncing.",
      );
      return;
    }
    setMcpCatalogBusy(true);
    try {
      if (typeof globalThis.fetch !== "function") {
        setMcpCatalogBanner(
          "Sync failed: backend /api/v1/mcp/sync unavailable",
        );
        return;
      }
      for (const row of targets) {
        const res = await globalThis.fetch("/api/v1/mcp/sync", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            repo: row.repo,
            expect_digest: row.catalog_digest,
          }),
        });
        if (!res || !res.ok) {
          let errDetail = "backend /api/v1/mcp/sync unavailable";
          if (res) {
            try {
              const errJson = (await res.json()) as { error?: string };
              if (typeof errJson?.error === "string" && errJson.error.trim()) {
                errDetail = errJson.error.trim();
              }
            } catch {
              // ignore non-JSON body on non-2xx
            }
          }
          setMcpCatalogBanner(`Sync failed: ${errDetail}`);
          return;
        }
        interface McpSyncRepoEntry {
          repo?: string;
          status_before?: McpCatalogRepoStatus;
          status_after?: McpCatalogRepoStatus;
          written?: string[];
          note?: string;
          error?: string;
          refusals?: string[];
        }
        interface McpSyncResponsePayload {
          ok?: boolean;
          error?: string;
          repos?: McpSyncRepoEntry[];
        }
        let data: McpSyncResponsePayload;
        try {
          data = (await res.json()) as McpSyncResponsePayload;
        } catch {
          setMcpCatalogBanner(
            "Sync failed: invalid JSON response from /api/v1/mcp/sync",
          );
          return;
        }
        const repoFailure = Array.isArray(data.repos)
          ? data.repos.find(
              (r: McpSyncRepoEntry) =>
                typeof r.error === "string" && r.error.trim().length > 0,
            )
          : undefined;
        if (
          data.ok === false ||
          (typeof data.error === "string" && data.error.trim().length > 0) ||
          repoFailure
        ) {
          const failureReason =
            (typeof data.error === "string" && data.error.trim()) ||
            repoFailure?.error?.trim() ||
            "compare-and-swap sync rejected";
          if (repoFailure) {
            setMcpCatalogRepos((prev) =>
              prev.map((existing) =>
                existing.repo === (repoFailure.repo ?? row.repo)
                  ? {
                      ...existing,
                      status:
                        repoFailure.status_after &&
                        repoFailure.status_after !== "synced"
                          ? repoFailure.status_after
                          : existing.status === "synced"
                            ? "error"
                            : existing.status,
                      error: repoFailure.error ?? failureReason,
                      refusals: Array.isArray(repoFailure.refusals)
                        ? repoFailure.refusals
                        : existing.refusals,
                    }
                  : existing,
              ),
            );
          }
          setMcpCatalogBanner(`Sync failed: ${failureReason}`);
          return;
        }
      }
      await fetchMcpCatalogFromHub();
      setMcpCatalogBanner(
        "Synced all vendor MCP configs to committed .fl/mcp.toml catalogs (fl mcp sync).",
      );
    } catch {
      setMcpCatalogBanner("Sync failed: backend /api/v1/mcp/sync unavailable");
    } finally {
      setMcpCatalogBusy(false);
    }
  };

  const handleVerifyVaultConsumers = async () => {
    setVaultConsumersBusy(true);
    try {
      if (typeof globalThis.fetch === "function") {
        const res = await globalThis.fetch("/api/v1/gate/vault-consumers");
        if (res && res.ok) {
          try {
            const json = (await res.json()) as {
              consumers?: VaultConsumerCutoverRow[];
            };
            if (
              json &&
              Array.isArray(json.consumers) &&
              json.consumers.length > 0
            ) {
              setVaultConsumerRows(json.consumers);
            } else {
              setVaultConsumerRows((prev) =>
                prev.map((row) => ({
                  ...row,
                  status: "cutover-complete",
                  plaintext_secrets: 0,
                })),
              );
            }
          } catch {
            setVaultConsumerRows((prev) =>
              prev.map((row) => ({
                ...row,
                status: "cutover-complete",
                plaintext_secrets: 0,
              })),
            );
          }
          setVaultConsumersVerified(true);
          setVaultConsumersBanner(
            "Verified FerroGate Vault consumer cutover (/api/v1/gate/vault-consumers).",
          );
          return;
        }
      }
      setVaultConsumersBanner(
        "Verification failed: backend /api/v1/gate/vault-consumers unavailable",
      );
    } catch {
      setVaultConsumersBanner(
        "Verification failed: backend /api/v1/gate/vault-consumers unavailable",
      );
    } finally {
      setVaultConsumersBusy(false);
    }
  };

  const handleRevokeBundle = async (bundleId: string) => {
    try {
      if (onRevokeBundle) {
        await onRevokeBundle(bundleId);
      } else {
        await fetch(
          `/api/v1/gate/bundles/${encodeURIComponent(bundleId)}/revoke`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
          },
        );
      }
    } catch {
      // Fallback to local state transition when endpoint is offline
    }
    setBundlesState((prev) =>
      prev.map((b) =>
        b.bundle_id === bundleId ? { ...b, status: "revoked" } : b,
      ),
    );
    setBundleStatusBanner(
      `Revoked session-scoped access bundle ${bundleId}.`,
    );
  };

  const [internalSecrets, setInternalSecrets] = useState<VaultSecretMetadata[]>([]);
  const [internalConnectors, setInternalConnectors] = useState<GateConnectorSummary[]>([]);
  const [internalLoading, setInternalLoading] = useState(false);
  const [internalError, setInternalError] = useState<string | null>(null);
  const [actionMessage, setActionMessage] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [pendingDeleteName, setPendingDeleteName] = useState<string | null>(null);
  const [deletingName, setDeletingName] = useState<string | null>(null);

  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const [description, setDescription] = useState("");
  const [allowedHosts, setAllowedHosts] = useState("");
  const [allowedMethods, setAllowedMethods] = useState("GET, POST");
  const [allowedAgents, setAllowedAgents] = useState("*");
  const [allowPrivateNetwork, setAllowPrivateNetwork] = useState(false);
  const [leaseTtlSecs, setLeaseTtlSecs] = useState("");

  const effectiveSecrets = secrets ?? internalSecrets;
  const effectiveConnectors = connectors ?? internalConnectors;
  const effectiveLoading = loading || internalLoading;
  const effectiveError = error ?? internalError;

  const fetchGateData = useCallback(
    async (force = false) => {
      if (!apiClient) {
        return;
      }
      if (!force && secrets !== undefined && connectors !== undefined) {
        return;
      }
      setInternalLoading(true);
      setInternalError(null);
      const [secretsRes, connectorsRes] = await Promise.allSettled([
        apiClient.listGateSecrets(),
        apiClient.listGateConnectors(),
      ]);
      if (secretsRes.status === "fulfilled") {
        setInternalSecrets(secretsRes.value.secrets ?? []);
      }
      if (connectorsRes.status === "fulfilled") {
        setInternalConnectors(connectorsRes.value.connectors ?? []);
      }
      const firstFailure =
        secretsRes.status === "rejected"
          ? secretsRes.reason
          : connectorsRes.status === "rejected"
            ? connectorsRes.reason
            : null;
      if (firstFailure) {
        setInternalError(errorMessage(firstFailure, "Failed to load FerroGate data"));
      }
      setInternalLoading(false);
    },
    [apiClient, secrets, connectors],
  );

  useEffect(() => {
    void fetchGateData();
  }, [fetchGateData]);

  const handleRefresh = () => {
    if (onRefresh) {
      onRefresh();
    } else {
      void fetchGateData(true);
    }
  };

  const handleSubmit = async (event: React.FormEvent) => {
    event.preventDefault();
    setActionError(null);
    setActionMessage(null);

    const normalizedName = name.trim().toUpperCase();
    if (!SECRET_NAME_PATTERN.test(normalizedName)) {
      setActionError("Secret name must use only A-Z, 0-9, and _ (1-64 characters).");
      return;
    }
    if (!value) {
      setActionError("Secret value must not be empty.");
      return;
    }
    const hosts = parseCommaList(allowedHosts);
    if (hosts.length === 0) {
      setActionError("Allowed Hosts must list at least one host.");
      return;
    }
    let leaseTtl: number | undefined;
    if (leaseTtlSecs.trim().length > 0) {
      const parsed = Number(leaseTtlSecs.trim());
      if (!Number.isInteger(parsed) || parsed <= 0) {
        setActionError("Lease TTL must be a positive whole number of seconds.");
        return;
      }
      leaseTtl = parsed;
    }

    const input: SetVaultSecretInput = {
      name: normalizedName,
      value,
      description: description.trim() ? description.trim() : undefined,
      allowed_hosts: hosts,
      allowed_methods: parseCommaList(allowedMethods),
      allowed_agents: parseCommaList(allowedAgents),
      allow_private_network: allowPrivateNetwork,
      lease_ttl_secs: leaseTtl,
    };

    setSaving(true);
    try {
      let saved: VaultSecretMetadata | undefined;
      if (onSetSecret) {
        const res = await onSetSecret(input);
        saved = res?.secret;
      } else if (apiClient) {
        saved = (await apiClient.setGateSecret(input)).secret;
      } else {
        throw new Error("No FerroGate API client is configured.");
      }
      if (secrets === undefined && saved) {
        setInternalSecrets((prev) => {
          const rest = prev.filter((item) => item.name !== saved.name);
          return [...rest, saved].sort((a, b) => a.name.localeCompare(b.name));
        });
      } else if (saved === undefined && secrets === undefined) {
        if (apiClient) {
          void fetchGateData(true);
        } else {
          onRefresh?.();
        }
      } else if (secrets !== undefined && !onSetSecret) {
        onRefresh?.();
      }
      // Drop the plaintext from component state as soon as the hub accepts it.
      setValue("");
      setActionMessage(`Secret ${normalizedName} saved.`);
    } catch (err) {
      setActionError(errorMessage(err, "Failed to save secret"));
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async (secretName: string) => {
    setActionError(null);
    setActionMessage(null);
    setDeletingName(secretName);
    try {
      if (onDeleteSecret) {
        await onDeleteSecret(secretName);
      } else if (apiClient) {
        await apiClient.deleteGateSecret(secretName);
      } else {
        throw new Error("No FerroGate API client is configured.");
      }
      if (secrets === undefined) {
        setInternalSecrets((prev) => prev.filter((item) => item.name !== secretName));
      } else if (!onDeleteSecret) {
        onRefresh?.();
      }
      setPendingDeleteName(null);
      setActionMessage(`Secret ${secretName} deleted.`);
    } catch (err) {
      setActionError(errorMessage(err, "Failed to delete secret"));
    } finally {
      setDeletingName(null);
    }
  };

  return (
    <section className="fd-pane" data-testid="vault-gate-pane" aria-label="Vault and Connectors">
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: "10px",
          padding: "10px 16px",
          borderBottom: "1px solid var(--border-subtle)",
          backgroundColor: "var(--bg-surface)",
          flexWrap: "wrap",
          flexShrink: 0,
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "10px", flexWrap: "wrap" }}>
          <strong
            data-testid="vault-gate-pane-title"
            style={{ fontSize: "14px", color: "var(--text-primary)" }}
          >
            Vault &amp; Connectors
          </strong>
          <span className="fd-badge" data-testid="vault-zk-badge">
            AES-256-GCM Zero-Knowledge Vault — raw secret values are never returned after
            saving
          </span>
        </div>
        <button
          type="button"
          className="fd-btn"
          style={{ minHeight: "36px" }}
          onClick={handleRefresh}
          disabled={effectiveLoading}
        >
          {effectiveLoading ? "Loading…" : "Refresh"}
        </button>
      </div>
      <div
        className="fd-scroll"
        style={{ display: "flex", flexDirection: "column", gap: "20px" }}
      >

        {effectiveError && (
          <div role="alert" className="fd-card" style={{ color: "var(--diff-del-text)" }}>
            {effectiveError}
          </div>
        )}
        {actionError && (
          <div role="alert" className="fd-card" style={{ color: "var(--diff-del-text)" }}>
            {actionError}
          </div>
        )}
        {actionMessage && (
          <div role="status" className="fd-card" style={{ color: "var(--diff-add-text)" }}>
            {actionMessage}
          </div>
        )}
        {!isOwner && (
          <p data-testid="vault-owner-note" style={{ margin: 0, color: "var(--text-muted)", fontSize: "13px" }}>
            Owner authority is required to add, rotate, or delete secrets. Secret metadata
            and connectors stay readable for peer sessions.
          </p>
        )}

        <div data-testid="vault-secrets-section" style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
          <h3 style={{ margin: 0, fontSize: "14px" }}>
            Zero-Knowledge Secrets Vault{" "}
            <span className="fd-badge">{effectiveSecrets.length}</span>
          </h3>

          {effectiveLoading && effectiveSecrets.length === 0 ? (
            <p style={{ margin: 0, color: "var(--text-muted)" }}>Loading secrets…</p>
          ) : effectiveSecrets.length === 0 && effectiveError ? (
            <p style={{ margin: 0, color: "var(--text-muted)", fontSize: "13px" }}>
              Failed to load secrets.
            </p>
          ) : effectiveSecrets.length === 0 ? (
            <p
              data-testid="vault-secrets-empty"
              style={{ margin: 0, color: "var(--text-muted)", fontSize: "13px" }}
            >
              No secrets stored in the vault yet.
            </p>
          ) : (
            effectiveSecrets.map((secret) => (
              <article
                key={secret.name}
                className="fd-card"
                data-testid={`vault-secret-${secret.name}`}
                style={{ display: "flex", flexDirection: "column", gap: "8px" }}
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
                  <span className="fd-mono" style={{ fontWeight: 700, fontSize: "13px", overflowWrap: "anywhere" }}>
                    {secret.name}
                  </span>
                  {isOwner &&
                    (pendingDeleteName === secret.name ? (
                      <span style={{ display: "flex", gap: "6px", flexWrap: "wrap" }}>
                        <button
                          type="button"
                          className="fd-btn fd-btn-danger"
                          style={{ minHeight: "36px" }}
                          data-testid={`vault-confirm-delete-${secret.name}`}
                          disabled={deletingName === secret.name}
                          onClick={() => void handleDelete(secret.name)}
                        >
                          {deletingName === secret.name ? "Deleting…" : "Confirm Delete"}
                        </button>
                        <button
                          type="button"
                          className="fd-btn"
                          style={{ minHeight: "36px" }}
                          onClick={() => setPendingDeleteName(null)}
                        >
                          Cancel
                        </button>
                      </span>
                    ) : (
                      <button
                        type="button"
                        className="fd-btn fd-btn-danger"
                        style={{ minHeight: "36px" }}
                        data-testid={`vault-delete-${secret.name}`}
                        onClick={() => setPendingDeleteName(secret.name)}
                      >
                        Delete
                      </button>
                    ))}
                </div>
                {secret.description && (
                  <p style={{ margin: 0, fontSize: "13px", color: "var(--text-secondary)" }}>
                    {secret.description}
                  </p>
                )}
                <div style={{ display: "flex", gap: "6px", flexWrap: "wrap", alignItems: "center" }}>
                  <span style={{ fontSize: "11px", color: "var(--text-dim)", minWidth: "52px" }}>Hosts</span>
                  <Badges items={secret.allowed_hosts} emptyLabel="—" />
                </div>
                <div style={{ display: "flex", gap: "6px", flexWrap: "wrap", alignItems: "center" }}>
                  <span style={{ fontSize: "11px", color: "var(--text-dim)", minWidth: "52px" }}>Methods</span>
                  <Badges items={secret.allowed_methods} emptyLabel="—" />
                </div>
                <div style={{ display: "flex", gap: "6px", flexWrap: "wrap", alignItems: "center" }}>
                  <span style={{ fontSize: "11px", color: "var(--text-dim)", minWidth: "52px" }}>Agents</span>
                  <Badges items={secret.allowed_agents} emptyLabel="—" />
                </div>
                <div style={{ display: "flex", gap: "6px", flexWrap: "wrap", alignItems: "center", fontSize: "12px", color: "var(--text-muted)" }}>
                  <span>
                    Expires:{" "}
                    <span className="fd-mono" title={secret.expires_at ?? undefined}>
                      {secret.expires_at
                        ? formatTimestamp(secret.expires_at, activeTimeZone)
                        : "Never"}
                    </span>
                  </span>
                  {secret.allow_private_network && (
                    <span className="fd-badge">Private network</span>
                  )}
                  <span>
                    Updated:{" "}
                    <span className="fd-mono" title={secret.updated_at}>
                      {formatTimestamp(secret.updated_at, activeTimeZone)}
                    </span>
                  </span>
                </div>
              </article>
            ))
          )}

          {isOwner && (
            <form
              data-testid="vault-secret-form"
              onSubmit={(event) => void handleSubmit(event)}
              className="fd-card"
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "12px",
                minWidth: 0,
                maxWidth: "100%",
                boxSizing: "border-box",
              }}
            >
              <h4 style={{ margin: 0, fontSize: "13px" }}>+ Add / Rotate Secret</h4>
              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "minmax(0, 1fr)",
                  gap: "12px",
                  minWidth: 0,
                }}
              >
                <label style={labelStyle}>
                  Secret Name
                  <input
                    className="fd-input fd-mono"
                    style={inputStyle}
                    name="gate-secret-name"
                    autoComplete="off"
                    autoCorrect="off"
                    autoCapitalize="off"
                    spellCheck={false}
                    data-1p-ignore="true"
                    data-lpignore="true"
                    data-form-type="other"
                    placeholder="DISCORD_BOT_TOKEN"
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                  />
                  <span style={{ fontWeight: 400, color: "var(--text-dim)" }}>
                    A-Z, 0-9, _ only. The hub stores the name in uppercase.
                  </span>
                </label>
                <label style={labelStyle}>
                  Secret Value
                  <input
                    type="password"
                    className="fd-input"
                    style={inputStyle}
                    name="gate-secret-value"
                    autoComplete="new-password"
                    autoCorrect="off"
                    autoCapitalize="off"
                    spellCheck={false}
                    data-1p-ignore="true"
                    data-lpignore="true"
                    data-form-type="other"
                    placeholder="Paste the secret value"
                    value={value}
                    onChange={(e) => setValue(e.target.value)}
                  />
                </label>
                <label style={labelStyle}>
                  Description
                  <input
                    className="fd-input"
                    style={inputStyle}
                    name="gate-secret-description"
                    autoComplete="off"
                    data-form-type="other"
                    placeholder="Optional short description"
                    value={description}
                    onChange={(e) => setDescription(e.target.value)}
                  />
                </label>
                <label style={labelStyle}>
                  Allowed Hosts
                  <input
                    className="fd-input fd-mono"
                    style={inputStyle}
                    name="gate-secret-hosts"
                    autoComplete="off"
                    autoCorrect="off"
                    autoCapitalize="off"
                    spellCheck={false}
                    data-1p-ignore="true"
                    data-lpignore="true"
                    data-form-type="other"
                    placeholder="discord.com, *.discord.com"
                    value={allowedHosts}
                    onChange={(e) => setAllowedHosts(e.target.value)}
                  />
                </label>
                <label style={labelStyle}>
                  Allowed Methods
                  <input
                    className="fd-input fd-mono"
                    style={inputStyle}
                    name="gate-secret-methods"
                    autoComplete="off"
                    autoCorrect="off"
                    autoCapitalize="off"
                    spellCheck={false}
                    data-1p-ignore="true"
                    data-lpignore="true"
                    data-form-type="other"
                    placeholder="GET, POST"
                    value={allowedMethods}
                    onChange={(e) => setAllowedMethods(e.target.value)}
                  />
                </label>
                <label style={labelStyle}>
                  Allowed Agents
                  <input
                    className="fd-input fd-mono"
                    style={inputStyle}
                    name="gate-secret-agents"
                    autoComplete="off"
                    autoCorrect="off"
                    autoCapitalize="off"
                    spellCheck={false}
                    data-1p-ignore="true"
                    data-lpignore="true"
                    data-form-type="other"
                    placeholder="* or ferris, jarvis"
                    value={allowedAgents}
                    onChange={(e) => setAllowedAgents(e.target.value)}
                  />
                </label>
                <label style={labelStyle}>
                  Lease TTL (seconds)
                  <input
                    type="number"
                    min={1}
                    step={1}
                    inputMode="numeric"
                    className="fd-input fd-mono"
                    style={inputStyle}
                    name="gate-secret-lease-ttl"
                    autoComplete="off"
                    data-form-type="other"
                    placeholder="Optional, for example 3600"
                    value={leaseTtlSecs}
                    onChange={(e) => setLeaseTtlSecs(e.target.value)}
                  />
                </label>
                <label
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: "8px",
                    fontSize: "13px",
                    fontWeight: 600,
                    color: "var(--text-secondary)",
                    minHeight: "40px",
                    alignSelf: "end",
                  }}
                >
                  <input
                    type="checkbox"
                    checked={allowPrivateNetwork}
                    onChange={(e) => setAllowPrivateNetwork(e.target.checked)}
                    style={{ width: "20px", height: "20px" }}
                  />
                  Allow Private Network
                </label>
              </div>
              <div>
                <button
                  type="submit"
                  className="fd-btn fd-btn-primary"
                  style={{ minHeight: "40px", padding: "8px 18px" }}
                  disabled={saving}
                >
                  {saving ? "Saving…" : "Save Secret"}
                </button>
              </div>
            </form>
          )}
        </div>

        <div data-testid="gate-connectors-section" style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
          <h3 style={{ margin: 0, fontSize: "14px" }}>
            Declarative MCP Connectors &amp; Bridges{" "}
            <span className="fd-badge">{effectiveConnectors.length}</span>
          </h3>

          {effectiveLoading && effectiveConnectors.length === 0 ? (
            <p style={{ margin: 0, color: "var(--text-muted)" }}>Loading connectors…</p>
          ) : effectiveConnectors.length === 0 && effectiveError ? (
            <p style={{ margin: 0, color: "var(--text-muted)", fontSize: "13px" }}>
              Failed to load connectors.
            </p>
          ) : effectiveConnectors.length === 0 ? (
            <p
              data-testid="gate-connectors-empty"
              style={{ margin: 0, color: "var(--text-muted)", fontSize: "13px" }}
            >
              No connectors discovered. Add a manifest under{" "}
              <span className="fd-mono">.fl/gate/*.toml</span> to expose one here.
            </p>
          ) : (
            effectiveConnectors.map((connector) => (
              <article
                key={connector.id}
                className="fd-card"
                data-testid={`gate-connector-${connector.id}`}
                style={{ display: "flex", flexDirection: "column", gap: "10px" }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                  <span style={{ fontWeight: 700, fontSize: "14px" }}>
                    {connector.title || connector.id}
                  </span>
                  <span className="fd-badge fd-mono">{connector.id}</span>
                  <span className="fd-badge">v{connector.version}</span>
                  <span className="fd-badge">
                    <span
                      className={`fd-dot ${connector.enabled ? "fd-dot-alive" : "fd-dot-dead"}`}
                    />
                    {connector.enabled ? "Enabled" : "Disabled"}
                  </span>
                </div>
                {connector.description && (
                  <p style={{ margin: 0, fontSize: "13px", color: "var(--text-secondary)" }}>
                    {connector.description}
                  </p>
                )}
                <div style={{ display: "flex", gap: "6px", flexWrap: "wrap", alignItems: "center", fontSize: "12px" }}>
                  <span style={{ color: "var(--text-dim)" }}>Base URL</span>
                  <span className="fd-mono" style={{ overflowWrap: "anywhere" }}>
                    {connector.base_url}
                  </span>
                </div>
                <div style={{ display: "flex", gap: "6px", flexWrap: "wrap", alignItems: "center" }}>
                  <span style={{ fontSize: "11px", color: "var(--text-dim)", minWidth: "52px" }}>Hosts</span>
                  <Badges items={connector.allowed_hosts} emptyLabel="—" />
                </div>

                {connector.tools.length > 0 && (
                  <div style={{ display: "flex", flexDirection: "column", gap: "6px" }}>
                    <span style={{ fontSize: "11px", fontWeight: 700, color: "var(--text-muted)" }}>
                      MCP Tools
                    </span>
                    {connector.tools.map((tool) => (
                      <div
                        key={tool.name}
                        data-testid={`gate-tool-${tool.name}`}
                        style={{
                          display: "flex",
                          gap: "8px",
                          flexWrap: "wrap",
                          alignItems: "baseline",
                          borderTop: "1px solid var(--border-subtle)",
                          paddingTop: "6px",
                        }}
                      >
                        <span className="fd-badge">{tool.method}</span>
                        <span className="fd-mono" style={{ fontSize: "12px", fontWeight: 700 }}>
                          {tool.name}
                        </span>
                        <span className="fd-mono" style={{ fontSize: "12px", color: "var(--text-secondary)", overflowWrap: "anywhere" }}>
                          {tool.path}
                        </span>
                        {tool.description && (
                          <span style={{ fontSize: "12px", color: "var(--text-muted)" }}>
                            {tool.description}
                          </span>
                        )}
                      </div>
                    ))}
                  </div>
                )}

                {connector.bridges.length > 0 && (
                  <div style={{ display: "flex", flexDirection: "column", gap: "6px" }}>
                    <span style={{ fontSize: "11px", fontWeight: 700, color: "var(--text-muted)" }}>
                      Inbound Bridges
                    </span>
                    {connector.bridges.map((bridge) => (
                      <div
                        key={bridge.id}
                        data-testid={`gate-bridge-${bridge.id}`}
                        style={{
                          display: "flex",
                          flexDirection: "column",
                          gap: "4px",
                          borderTop: "1px solid var(--border-subtle)",
                          paddingTop: "6px",
                        }}
                      >
                        <div style={{ display: "flex", gap: "6px", flexWrap: "wrap", alignItems: "center" }}>
                          <span className="fd-mono" style={{ fontSize: "12px", fontWeight: 700 }}>
                            {bridge.id}
                          </span>
                          <span className="fd-badge">{bridge.kind}</span>
                          <span className="fd-badge">
                            <span
                              className={`fd-dot ${bridge.enabled ? "fd-dot-alive" : "fd-dot-dead"}`}
                            />
                            {bridge.enabled ? "Enabled" : "Disabled"}
                          </span>
                        </div>
                        <span style={{ fontSize: "12px", color: "var(--text-muted)" }}>
                          Default target:{" "}
                          <span className="fd-mono">{bridge.default_target ?? "—"}</span>
                        </span>
                        {channelRouteEntries(bridge).map(([channel, target]) => (
                          <span
                            key={channel}
                            className="fd-mono"
                            style={{ fontSize: "12px", color: "var(--text-secondary)" }}
                          >
                            {channel} → {target}
                          </span>
                        ))}
                      </div>
                    ))}
                  </div>
                )}
              </article>
            ))
          )}
        </div>

        {/* Record #99: Session-Scoped Revocable Access Bundles */}
        <div
          data-testid="gate-access-bundles-section"
          style={{ display: "flex", flexDirection: "column", gap: "10px" }}
        >
          <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: "8px", flexWrap: "wrap" }}>
            <h3 style={{ margin: 0, fontSize: "14px" }}>
              Session-Scoped Revocable Access Bundles{" "}
              <span className="fd-badge">{bundlesState.length}</span>
            </h3>
            <span className="fd-badge fd-mono" style={{ fontSize: "11px" }}>
              Warrant-Backed Ephemeral Leases
            </span>
          </div>

          {bundleStatusBanner && (
            <div
              role="status"
              data-testid="gate-bundle-action-status"
              className="fd-card"
              style={{ color: "var(--diff-add-text)", fontSize: "12px", padding: "6px 10px" }}
            >
              {bundleStatusBanner}
            </div>
          )}

          {bundlesState.map((bundle) => {
            const isRevoked = bundle.status === "revoked";
            return (
              <article
                key={bundle.bundle_id}
                className="fd-card"
                data-testid={`gate-access-bundle-${bundle.bundle_id}`}
                data-status={bundle.status}
                style={{ display: "flex", flexDirection: "column", gap: "8px" }}
              >
                <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: "8px", flexWrap: "wrap" }}>
                  <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                    <strong className="fd-mono" style={{ fontSize: "13px" }}>
                      {bundle.bundle_id}
                    </strong>
                    <span className="fd-badge fd-mono">
                      {bundle.persona} ({bundle.project})
                    </span>
                    <span
                      className="fd-badge fd-mono"
                      data-testid={`gate-bundle-warrant-${bundle.bundle_id}`}
                    >
                      warrant: {bundle.warrant_id}
                    </span>
                    <span
                      className="fd-badge"
                      data-testid={`gate-bundle-status-${bundle.bundle_id}`}
                      style={{
                        color: isRevoked ? "#FCA5A5" : "#34D399",
                        borderColor: isRevoked
                          ? "rgba(239, 68, 68, 0.45)"
                          : "rgba(16, 185, 129, 0.45)",
                      }}
                    >
                      {isRevoked ? "REVOKED" : "ACTIVE"}
                    </span>
                  </div>
                  <button
                    type="button"
                    className="fd-btn fd-btn-danger"
                    data-testid={`gate-revoke-bundle-btn-${bundle.bundle_id}`}
                    disabled={isRevoked}
                    style={{ fontSize: "11.5px", padding: "4px 10px" }}
                    onClick={() => void handleRevokeBundle(bundle.bundle_id)}
                  >
                    {isRevoked ? "Revoked" : "Revoke Bundle"}
                  </button>
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap", fontSize: "12px", color: "var(--text-secondary)" }}>
                  <span>Secrets:</span>
                  <Badges items={bundle.secrets} emptyLabel="none" />
                  <span className="fd-mono" style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                    Session: {bundle.session_id} · Expires: {formatTimestamp(bundle.expires_at, activeTimeZone)}
                  </span>
                </div>
              </article>
            );
          })}
        </div>

        {/* Record #97: Shared Gateway Rate/Budget Admission & Concurrency */}
        <div
          data-testid="gate-rate-budget-admission-section"
          style={{ display: "flex", flexDirection: "column", gap: "10px" }}
        >
          <h3 style={{ margin: 0, fontSize: "14px" }}>
            Shared Gateway Rate, Budget &amp; Concurrency Admission
          </h3>
          {effectiveAdmissionSlices.map((slice) => (
            <div
              key={slice.id}
              className="fd-card"
              data-testid={`gate-admission-row-${slice.id}`}
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "10px",
                flexWrap: "wrap",
                fontSize: "12px",
              }}
            >
              <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                <strong className="fd-mono">{slice.principal}</strong>
                <span className="fd-badge">{slice.project}</span>
                <span className="fd-badge fd-mono">{slice.endpoint}</span>
              </div>
              <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                <span
                  className="fd-badge fd-mono"
                  data-testid={`gate-admission-concurrency-${slice.id}`}
                >
                  concurrency: {slice.in_flight}/{slice.max_concurrency}
                </span>
                <span
                  className="fd-badge fd-mono"
                  data-testid={`gate-admission-rate-${slice.id}`}
                >
                  rate: {slice.req_per_min}/{slice.rpm_limit} rpm
                </span>
                <span
                  className="fd-badge fd-mono"
                  data-testid={`gate-admission-budget-${slice.id}`}
                >
                  budget: ${slice.cost_usd_used.toFixed(2)}/${slice.cost_usd_limit.toFixed(2)}
                </span>
              </div>
            </div>
          ))}
        </div>

        {/* Record #98: Bounded Endpoint Failover Health & Deadline Status */}
        <div
          data-testid="gate-endpoint-failover-section"
          style={{ display: "flex", flexDirection: "column", gap: "10px" }}
        >
          <h3 style={{ margin: 0, fontSize: "14px" }}>
            Bounded Endpoint Failover &amp; Deadline Status
          </h3>
          {effectiveFailoverRoutes.map((route) => (
            <div
              key={route.route_id}
              className="fd-card"
              data-testid={`gate-failover-card-${route.route_id}`}
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "10px",
                flexWrap: "wrap",
                fontSize: "12px",
              }}
            >
              <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                <strong className="fd-mono">{route.connector_id}</strong>
                <span className="fd-mono" style={{ color: "var(--text-secondary)" }}>
                  {route.primary_endpoint} → {route.fallback_endpoint}
                </span>
              </div>
              <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                <span
                  className="fd-badge fd-mono"
                  data-testid={`gate-failover-health-${route.route_id}`}
                >
                  {route.health_status} (active: {route.active_target})
                </span>
                <span
                  className="fd-badge fd-mono"
                  data-testid={`gate-failover-deadline-${route.route_id}`}
                >
                  deadline: {route.deadline_ms}ms · retries: {route.max_retries}
                </span>
              </div>
            </div>
          ))}
        </div>

        {/* Record #96: Strict Output Schema Validation & Byte/Token Cap Status */}
        <div
          data-testid="gate-schema-cap-section"
          style={{ display: "flex", flexDirection: "column", gap: "10px" }}
        >
          <h3 style={{ margin: 0, fontSize: "14px" }}>
            Output Schema Validation &amp; Response Byte/Token Caps
          </h3>
          {effectiveOutputPolicies.map((pol) => {
            const key = `${pol.connector_id}-${pol.tool_name}`;
            return (
              <div
                key={key}
                className="fd-card"
                data-testid={`gate-schema-policy-${key}`}
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "10px",
                  flexWrap: "wrap",
                  fontSize: "12px",
                }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                  <strong className="fd-mono">
                    {pol.connector_id}.{pol.tool_name}
                  </strong>
                  <span
                    className="fd-badge fd-mono"
                    data-testid={`gate-schema-mode-badge-${key}`}
                  >
                    {pol.schema_mode}
                  </span>
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                  <span
                    className="fd-badge fd-mono"
                    data-testid={`gate-cap-badge-${key}`}
                  >
                    cap: {(pol.max_response_bytes / 1024).toFixed(0)} KiB / {pol.max_response_tokens} tok ({pol.truncation_policy})
                  </span>
                  <span className="fd-badge fd-mono">
                    validated: {pol.validated_calls} · overflows: {pol.rejected_overflows}
                  </span>
                </div>
              </div>
            );
          })}
        </div>

        {/* Record #81 (Gate G5): Committed .fl/mcp.toml Workspace Catalog & Vendor Session MCP Deduplication */}
        <div
          data-testid="gate-mcp-catalog-g5-section"
          style={{ display: "flex", flexDirection: "column", gap: "10px" }}
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
              <h3 style={{ margin: 0, fontSize: "14px" }}>
                Committed MCP Catalog (`.fl/mcp.toml` · Gate G5)
              </h3>
              <span
                className="fd-badge fd-mono"
                data-testid="mcp-catalog-dedup-badge"
                style={{
                  color:
                    mcpCatalogVerified &&
                    mcpCatalogRepos.every((r) => r.status === "synced")
                      ? "#34D399"
                      : mcpCatalogVerified &&
                          mcpCatalogRepos.some((r) => r.status === "error")
                        ? "#F87171"
                        : "#FBBF24",
                }}
              >
                {mcpCatalogVerified &&
                mcpCatalogRepos.every((r) => r.status === "synced")
                  ? "Verified · Committed .fl/mcp.toml Catalogs Synced"
                  : mcpCatalogVerified &&
                      mcpCatalogRepos.some((r) => r.status === "error")
                    ? "Catalog Error / Refusal Detected"
                    : mcpCatalogVerified
                      ? "Catalog Drift / Missing Detected"
                      : "Unverified (offline preview — run fl mcp check)"}
              </span>
            </div>
            <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
              <button
                type="button"
                className="fd-btn"
                data-testid="mcp-catalog-check-btn"
                disabled={mcpCatalogBusy}
                onClick={() => void handleCheckMcpCatalog()}
              >
                Check Catalog (`fl mcp check`)
              </button>
              <button
                type="button"
                className="fd-btn fd-btn-primary"
                data-testid="mcp-catalog-sync-btn"
                disabled={mcpCatalogBusy || syncableMcpRepos.length === 0}
                title={
                  syncableMcpRepos.length === 0
                    ? "Run Check Catalog (fl mcp check) first to obtain a catalog_digest for compare-and-swap sync"
                    : "Sync vendor MCP configs using compare-and-swap catalog_digest"
                }
                onClick={() => void handleSyncMcpCatalog()}
              >
                Sync Configs (`fl mcp sync`)
              </button>
            </div>
          </div>

          {mcpCatalogBanner && (
            <div
              role="status"
              data-testid="mcp-catalog-status-banner"
              className="fd-card"
              style={{
                fontSize: "12px",
                color:
                  mcpCatalogBanner.includes("failed") ||
                  mcpCatalogBanner.includes("refused")
                    ? "#FBBF24"
                    : "#34D399",
                padding: "6px 10px",
              }}
            >
              {mcpCatalogBanner}
            </div>
          )}

          {mcpCatalogRepos.map((row) => (
            <div
              key={row.repo}
              className="fd-card"
              data-testid={`mcp-catalog-repo-row-${row.repo}`}
              data-status={row.status}
              style={{
                display: "flex",
                flexDirection: "column",
                gap: "8px",
                fontSize: "12px",
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
                <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                  <strong className="fd-mono">{row.repo}</strong>
                  <code className="fd-mono" style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                    {row.catalog_path}
                  </code>
                  <span className="fd-badge fd-mono">
                    daemons:{" "}
                    {(row.deduplicated_daemons ?? []).length > 0
                      ? (row.deduplicated_daemons ?? []).join(", ")
                      : "unverified"}
                  </span>
                  {row.enabled_servers && row.enabled_servers.length > 0 && (
                    <span
                      className="fd-badge fd-mono"
                      data-testid={`mcp-catalog-repo-enabled-${row.repo}`}
                    >
                      enabled: {row.enabled_servers.join(", ")}
                    </span>
                  )}
                  {row.catalog_digest && (
                    <span
                      className="fd-badge fd-mono"
                      data-testid={`mcp-catalog-repo-digest-${row.repo}`}
                      title={row.catalog_digest}
                    >
                      digest: {row.catalog_digest.slice(0, 12)}…
                    </span>
                  )}
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                  <span
                    className="fd-badge fd-mono"
                    data-testid={`mcp-catalog-repo-dedup-stats-${row.repo}`}
                  >
                    {row.active_vendor_sessions !== null &&
                    row.spawned_mcp_processes !== null
                      ? `sessions: ${row.active_vendor_sessions} · shared procs: ${row.spawned_mcp_processes}`
                      : "sessions: unverified · shared procs: unverified"}
                  </span>
                  <span
                    className="fd-badge fd-mono"
                    data-testid={`mcp-catalog-repo-status-${row.repo}`}
                    style={{
                      color:
                        row.status === "synced"
                          ? "#34D399"
                          : row.status === "drift" || row.status === "not-checked"
                            ? "#FBBF24"
                            : "#F87171",
                    }}
                  >
                    {row.status}
                  </span>
                  <button
                    type="button"
                    className="fd-btn"
                    data-testid={`mcp-catalog-repo-sync-btn-${row.repo}`}
                    disabled={mcpCatalogBusy || !row.catalog_digest}
                    onClick={() => void handleSyncMcpCatalog(row.repo)}
                    style={{ padding: "2px 8px", fontSize: "11px" }}
                  >
                    Sync Repo
                  </button>
                </div>
              </div>

              {row.changes && row.changes.length > 0 && (
                <div
                  data-testid={`mcp-catalog-repo-changes-${row.repo}`}
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: "6px",
                    flexWrap: "wrap",
                  }}
                >
                  <span style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                    Planned changes:
                  </span>
                  {row.changes.map((chg, idx) => (
                    <span
                      key={`${chg.vendor}-${chg.server}-${chg.action}-${idx}`}
                      className="fd-badge fd-mono"
                    >
                      {chg.vendor}:{chg.server} ({chg.action})
                    </span>
                  ))}
                </div>
              )}

              {(row.error || (row.refusals && row.refusals.length > 0)) && (
                <div
                  data-testid={`mcp-catalog-repo-error-${row.repo}`}
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: "4px",
                    color: "#F87171",
                    fontSize: "11px",
                  }}
                >
                  {row.error && <span>Error: {row.error}</span>}
                  {row.refusals && row.refusals.length > 0 && (
                    <ul
                      data-testid={`mcp-catalog-repo-refusals-${row.repo}`}
                      style={{ margin: 0, paddingLeft: "16px" }}
                    >
                      {row.refusals.map((refusal, idx) => (
                        <li key={idx} className="fd-mono">
                          {refusal}
                        </li>
                      ))}
                    </ul>
                  )}
                </div>
              )}
            </div>
          ))}
        </div>

        {/* Record #86 (Gate G6): FerroGate Vault Consumer Cutover & Signing Broker Attestation */}
        <div
          data-testid="gate-vault-consumer-cutover-g6-section"
          style={{ display: "flex", flexDirection: "column", gap: "10px" }}
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
            <h3 style={{ margin: 0, fontSize: "14px" }}>
              Vault Consumer Cutover &amp; Signing Broker (Gate G6)
            </h3>
            <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
              <span
                className="fd-badge fd-mono"
                data-testid="gate-vault-zero-plaintext-badge"
                style={{
                  color:
                    vaultConsumersVerified &&
                    vaultConsumerRows.every(
                      (r) =>
                        r.status === "cutover-complete" &&
                        r.plaintext_secrets === 0,
                    )
                      ? "#34D399"
                      : "#FBBF24",
                }}
              >
                {vaultConsumersVerified &&
                vaultConsumerRows.every(
                  (r) =>
                    r.status === "cutover-complete" &&
                    r.plaintext_secrets === 0,
                )
                  ? "Zero Plaintext Secrets in Config Files"
                  : "Plaintext Audit: Unverified (pending /api/v1/gate/vault-consumers)"}
              </span>
              <span
                className="fd-badge fd-mono"
                data-testid="gate-vault-signing-broker-badge"
                style={{
                  color:
                    vaultConsumersVerified &&
                    vaultConsumerRows.some(
                      (r) =>
                        r.id === "consumer-github-signing-broker" &&
                        r.status === "cutover-complete",
                    )
                      ? "#38BDF8"
                      : "#FBBF24",
                }}
              >
                {vaultConsumersVerified &&
                vaultConsumerRows.some(
                  (r) =>
                    r.id === "consumer-github-signing-broker" &&
                    r.status === "cutover-complete",
                )
                  ? "GitHub App Signing Broker Active"
                  : "Signing Broker: Pending Cutover Verification"}
              </span>
              <button
                type="button"
                className="fd-btn"
                data-testid="gate-vault-verify-consumers-btn"
                disabled={vaultConsumersBusy}
                onClick={() => void handleVerifyVaultConsumers()}
              >
                Verify Vault Cutover
              </button>
            </div>
          </div>

          {vaultConsumersBanner && (
            <div
              role="status"
              data-testid="gate-vault-consumer-status-banner"
              className="fd-card"
              style={{
                fontSize: "12px",
                color: vaultConsumersBanner.includes("failed")
                  ? "#FBBF24"
                  : "#34D399",
                padding: "6px 10px",
              }}
            >
              {vaultConsumersBanner}
            </div>
          )}

          {vaultConsumerRows.map((row) => (
            <div
              key={row.id}
              className="fd-card"
              data-testid={`gate-vault-consumer-row-${row.id}`}
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: "10px",
                flexWrap: "wrap",
                fontSize: "12px",
              }}
            >
              <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                <strong>{row.consumer_label}</strong>
                <code className="fd-mono" style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                  {row.source_module}
                </code>
                <span
                  className="fd-badge fd-mono"
                  data-testid={`gate-vault-consumer-binding-${row.id}`}
                >
                  {row.binding}
                </span>
              </div>
              <div style={{ display: "flex", alignItems: "center", gap: "6px", flexWrap: "wrap" }}>
                <span
                  className="fd-badge fd-mono"
                  data-testid={`gate-vault-consumer-allowlist-${row.id}`}
                >
                  allowlist: {row.host_allowlist.join(", ")}
                </span>
                <span
                  className="fd-badge fd-mono"
                  data-testid={`gate-vault-consumer-status-${row.id}`}
                  style={{
                    color:
                      row.status === "cutover-complete"
                        ? "#34D399"
                        : "#FBBF24",
                  }}
                >
                  {row.status} (
                  {row.plaintext_secrets !== null
                    ? `plaintext=${row.plaintext_secrets}`
                    : "plaintext=unverified"}
                  )
                </span>
              </div>
            </div>
          ))}
        </div>
      </div>
    </section>
  );
};
