import React, { useEffect, useState } from "react";
import { HubApiClient } from "../api/client";
import { getVendorModelOptions } from "./ChatPane";
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
  nodes?: EnrolledNode[];
  onSpinUp?: (req: SpinUpSessionRequest) => Promise<SpinUpSessionResponse>;
  onSpunUp?: (res: SpinUpSessionResponse) => void;
  apiClient?: HubApiClient;
}

export const SpinUpAgentModal: React.FC<SpinUpAgentModalProps> = ({
  isOpen,
  onClose,
  nodes = [],
  onSpinUp,
  onSpunUp,
  apiClient,
}) => {
  const [persona, setPersona] = useState("");
  const [project, setProject] = useState<string>("ferroloop");
  const [role, setRole] = useState<string>("assistant");
  const [vendor, setVendor] = useState<string>("claude");
  const [model, setModel] = useState("");
  const [nodeId, setNodeId] = useState("ai-lab-0");
  const [hosted, setHosted] = useState(true);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

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

    const payload: SpinUpSessionRequest = {
      persona: trimmedPersona,
      project,
      role,
      vendor,
      ...(model.trim() ? { model: model.trim() } : {}),
      node_id: nodeId.trim() || "ai-lab-0",
      hosted,
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
      setError(err instanceof Error ? err.message : "Failed to spin up agent session");
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
        aria-label="Spin Up Agent Session"
        data-testid="spin-up-agent-modal"
        className="fd-modal"
        onClick={(e) => e.stopPropagation()}
        style={{ maxWidth: "480px" }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            marginBottom: "14px",
          }}
        >
          <h2 style={{ margin: 0, fontSize: "16px", fontWeight: 700 }}>
            Spin Up Agent Session
          </h2>
          <button
            type="button"
            className="fd-btn"
            onClick={onClose}
            disabled={submitting}
            aria-label="Close Spin Up Modal"
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
                onChange={(e) => setVendor(e.target.value)}
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
                placeholder="e.g. gemini-4-argon, claude-opus-5-5"
                value={model}
                onChange={(e) => setModel(e.target.value)}
                disabled={submitting}
                style={{ width: "100%" }}
              />
              <datalist id="fd-spin-up-model-options">
                {getVendorModelOptions(vendor).map((opt) => (
                  <option key={opt} value={opt} />
                ))}
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
              {submitting ? "Spinning Up..." : "Spin Up Agent"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
};
