import React, { useEffect, useState } from "react";
import { HubApiClient } from "../api/client";
import {
  formatModelOptionLabel,
  getVendorModelOptions,
} from "./ChatPane";
import type {
  EnrolledNode,
  SpinUpSessionRequest,
  SpinUpSessionResponse,
} from "../types/api";

export const SPIN_UP_PROJECTS = [
  { value: "ferroloop", label: "ferroloop (FerroLoop)" },
  { value: "kaggle-gemma4", label: "kaggle-gemma4 (Kaggle-Gemma4)" },
  { value: "sonora", label: "sonora (Sonora)" },
  { value: "lucida", label: "lucida (Lucida)" },
  { value: "prosodia", label: "prosodia (Prosodia)" },
  { value: "website", label: "website (ArtificialHumanity.io)" },
  { value: "ai-lab-amd", label: "ai-lab-amd (AI-Lab-AMD)" },
] as const;

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
  nodes?: EnrolledNode[];
  onSpinUp?: (req: SpinUpSessionRequest) => Promise<SpinUpSessionResponse>;
  onSpunUp?: (res: SpinUpSessionResponse) => void;
  apiClient?: HubApiClient;
}

export function resolveSpinUpProject(rawProject?: string | null): string {
  const trimmed = (rawProject ?? "").trim().toLowerCase();
  if (!trimmed || trimmed === "all") {
    return "ferroloop";
  }
  const matched = SPIN_UP_PROJECTS.find(
    (p) => p.value === trimmed || p.label.toLowerCase().includes(trimmed),
  );
  return matched ? matched.value : "ferroloop";
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

export const SpinUpAgentModal: React.FC<SpinUpAgentModalProps> = ({
  isOpen,
  onClose,
  initialProject = null,
  nodes = [],
  onSpinUp,
  onSpunUp,
  apiClient,
}) => {
  const [persona, setPersona] = useState("");
  const [project, setProject] = useState<string>(() =>
    resolveSpinUpProject(initialProject),
  );
  const [role, setRole] = useState<string>("assistant");
  const [vendor, setVendor] = useState<string>("claude");
  const [model, setModel] = useState("");
  const [nodeId, setNodeId] = useState("ai-lab-0");
  const [hosted, setHosted] = useState(true);
  const [worktreePolicy, setWorktreePolicy] =
    useState<WorktreePolicyMode>("create");
  const [worktreePolicyTouched, setWorktreePolicyTouched] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (isOpen) {
      setProject(resolveSpinUpProject(initialProject));
      setWorktreePolicyTouched(false);
    }
  }, [initialProject, isOpen]);

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

  const nodeOptions = Array.from(
    new Set(["ai-lab-0", ...nodes.map((n) => n.node_id || n.display_name).filter(Boolean)]),
  );

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    const trimmedPersona = persona.trim().toLowerCase();
    if (!trimmedPersona) {
      setError("Please enter a persona name (e.g. kacey).");
      return;
    }

    setError(null);
    setSubmitting(true);

    const worktreePreview = computeWorktreeBootstrapPreview(
      trimmedPersona,
      project,
      worktreePolicy,
    );

    const payload: SpinUpSessionRequest = {
      persona: trimmedPersona,
      project,
      role,
      vendor,
      ...(model.trim() ? { model: model.trim() } : {}),
      node_id: nodeId.trim() || "ai-lab-0",
      hosted,
      ...(worktreePolicyTouched
        ? {
            worktree_policy: worktreePolicy,
            allocated_port: worktreePreview.allocatedPort,
            allocated_vite_port: worktreePreview.allocatedVitePort,
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
      onSpunUp?.(res);
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
                onChange={(e) => setProject(e.target.value)}
                disabled={submitting}
                style={{ width: "100%" }}
              >
                {SPIN_UP_PROJECTS.map((proj) => (
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
                {SPIN_UP_ROLES.map((r) => (
                  <option key={r.value} value={r.value}>
                    {r.label}
                  </option>
                ))}
              </select>
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
              onChange={(e) => setNodeId(e.target.value)}
              disabled={submitting}
              style={{ width: "100%" }}
            />
            <datalist id="fd-spin-up-node-options">
              {nodeOptions.map((n) => (
                <option key={n} value={n} />
              ))}
            </datalist>
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
