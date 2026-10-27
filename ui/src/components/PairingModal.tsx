import React, { useState } from "react";

export interface PairingModalProps {
  isOpen: boolean;
  onClose: () => void;
  onPairWithCode: (code: string) => Promise<void>;
  onPairWithToken: (token: string) => Promise<void>;
  error?: string | null;
}

export const PairingModal: React.FC<PairingModalProps> = ({
  isOpen,
  onClose,
  onPairWithCode,
  onPairWithToken,
  error,
}) => {
  const [code, setCode] = useState("");
  const [token, setToken] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);

  if (!isOpen) {
    return null;
  }

  const displayError = localError || error;

  const handleCodeSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    const trimmed = code.trim();
    if (!trimmed) {
      setLocalError("Please enter a pairing code.");
      return;
    }
    setLocalError(null);
    setSubmitting(true);
    try {
      await onPairWithCode(trimmed);
      setCode("");
    } catch (err) {
      setLocalError(err instanceof Error ? err.message : "Pairing failed");
    } finally {
      setSubmitting(false);
    }
  };

  const handleTokenSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    const trimmed = token.trim();
    if (!trimmed) {
      setLocalError("Please enter a bearer token.");
      return;
    }
    setLocalError(null);
    setSubmitting(true);
    try {
      await onPairWithToken(trimmed);
      setToken("");
    } catch (err) {
      setLocalError(err instanceof Error ? err.message : "Token authentication failed");
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="fd-modal-backdrop" role="dialog" aria-modal="true" aria-label="Pair with FerroLoop Hub">
      <div className="fd-modal">
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            marginBottom: "14px",
          }}
        >
          <h2 style={{ margin: 0, fontSize: "16px", fontWeight: 700 }}>
            Authenticate with FerroLoop Hub
          </h2>
          <button
            type="button"
            className="fd-btn"
            onClick={onClose}
            aria-label="Close Pairing Modal"
          >
            ✕
          </button>
        </div>

        <p style={{ margin: "0 0 14px 0", fontSize: "12px", color: "var(--text-secondary)" }}>
          Run <code className="fd-mono">fl hub pair</code> on the host to generate a 10-minute
          pairing code, or paste an Owner/Peer bearer token directly.
        </p>

        {displayError && (
          <div
            role="alert"
            style={{
              marginBottom: "14px",
              padding: "8px 10px",
              borderRadius: "var(--radius-md)",
              backgroundColor: "rgba(239, 68, 68, 0.14)",
              border: "1px solid rgba(239, 68, 68, 0.45)",
              color: "#FCA5A5",
              fontSize: "12px",
            }}
          >
            {displayError}
          </div>
        )}

        <form onSubmit={handleCodeSubmit} style={{ marginBottom: "16px" }}>
          <label
            htmlFor="fd-pair-code-input"
            style={{
              display: "block",
              fontSize: "11px",
              fontWeight: 600,
              color: "var(--text-muted)",
              marginBottom: "6px",
              textTransform: "uppercase",
            }}
          >
            10-Minute Pairing Code
          </label>
          <div style={{ display: "flex", gap: "8px" }}>
            <input
              id="fd-pair-code-input"
              type="text"
              name="fd-pair-code"
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              data-1p-ignore="true"
              data-lpignore="true"
              data-form-type="other"
              className="fd-input fd-mono"
              placeholder="Enter pairing code (e.g. FL-123456)"
              value={code}
              onChange={(e) => setCode(e.target.value)}
              disabled={submitting}
              style={{ flex: 1 }}
            />
            <button
              type="submit"
              className="fd-btn fd-btn-primary"
              disabled={submitting}
            >
              Pair with Code
            </button>
          </div>
        </form>

        <form onSubmit={handleTokenSubmit}>
          <label
            htmlFor="fd-pair-token-input"
            style={{
              display: "block",
              fontSize: "11px",
              fontWeight: 600,
              color: "var(--text-muted)",
              marginBottom: "6px",
              textTransform: "uppercase",
            }}
          >
            Owner / Peer Bearer Token
          </label>
          <div style={{ display: "flex", gap: "8px" }}>
            <input
              id="fd-pair-token-input"
              type="password"
              name="fd-pair-token"
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              data-1p-ignore="true"
              data-lpignore="true"
              data-form-type="other"
              className="fd-input fd-mono"
              placeholder="Paste bearer token"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              disabled={submitting}
              style={{ flex: 1 }}
            />
            <button type="submit" className="fd-btn" disabled={submitting}>
              Save Token
            </button>
          </div>
        </form>
      </div>
    </div>
  );
};
