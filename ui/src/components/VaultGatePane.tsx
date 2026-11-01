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
  authority?: HubAuthority | null;
  loading?: boolean;
  error?: string | null;
  onRefresh?: () => void;
  onSetSecret?: (input: SetVaultSecretInput) => Promise<GateSecretResponse> | void;
  onDeleteSecret?: (name: string) => Promise<DeleteGateSecretResponse> | void;
  apiClient?: HubApiClient;
}

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
  authority = null,
  loading = false,
  error = null,
  onRefresh,
  onSetSecret,
  onDeleteSecret,
  apiClient,
}) => {
  const isOwner = authority?.tier === "owner";
  const activeTimeZone = useActiveTimeZone();

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
        className="fd-scroll"
        style={{ display: "flex", flexDirection: "column", gap: "20px" }}
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
          <div style={{ display: "flex", alignItems: "center", gap: "10px", flexWrap: "wrap" }}>
            <h2 style={{ margin: 0, fontSize: "16px" }}>Vault &amp; Connectors</h2>
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
              style={{ display: "flex", flexDirection: "column", gap: "12px" }}
            >
              <h4 style={{ margin: 0, fontSize: "13px" }}>+ Add / Rotate Secret</h4>
              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))",
                  gap: "12px",
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
      </div>
    </section>
  );
};
