import React, { useState } from "react";
import { HubApiClient, getLastRegisteredPasskeyId } from "../api/client";
import type { HubAuthority } from "../types/api";

export type AuthTabMode = "token" | "password" | "passkey";

export interface PairingModalProps {
  isOpen: boolean;
  onClose: () => void;
  onPairWithCode: (code: string) => Promise<void>;
  onPairWithToken: (token: string) => Promise<void>;
  onLoginWithPasskey?: (
    credentialId: string,
    totpCode?: string,
  ) => Promise<{
    ok?: boolean;
    requires_totp?: boolean;
    authority?: HubAuthority;
  } | void>;
  onAuthenticated?: (authority?: HubAuthority) => void;
  apiClient?: HubApiClient;
  error?: string | null;
}

export const PairingModal: React.FC<PairingModalProps> = ({
  isOpen,
  onClose,
  onPairWithCode,
  onPairWithToken,
  onLoginWithPasskey,
  onAuthenticated,
  apiClient,
  error,
}) => {
  const [authMode, setAuthMode] = useState<AuthTabMode>("token");
  const [code, setCode] = useState("");
  const [token, setToken] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [totpCode, setTotpCode] = useState("");
  const [passkeyId, setPasskeyId] = useState("");
  const [passkeyTotpCode, setPasskeyTotpCode] = useState("");
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
      setLocalError(
        err instanceof Error ? err.message : "Token authentication failed",
      );
    } finally {
      setSubmitting(false);
    }
  };

  const handlePasswordSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    const trimmedUser = username.trim();
    if (!trimmedUser || !password) {
      setLocalError("Please enter both username and password.");
      return;
    }
    setLocalError(null);
    setSubmitting(true);
    try {
      const client = apiClient ?? new HubApiClient();
      const trimmedTotp = totpCode.trim();
      const res = await client.loginWithPassword({
        username: trimmedUser,
        password,
        ...(trimmedTotp ? { totp_code: trimmedTotp } : {}),
      });
      if (res?.requires_totp && !res.ok) {
        setLocalError(
          "Multi-factor authentication required. Please enter your 6-digit TOTP code.",
        );
        return;
      }
      setUsername("");
      setPassword("");
      setTotpCode("");
      onAuthenticated?.(res?.authority);
      onClose();
    } catch (err) {
      setLocalError(
        err instanceof Error ? err.message : "Password authentication failed",
      );
    } finally {
      setSubmitting(false);
    }
  };

  const handlePasskeySubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setLocalError(null);
    setSubmitting(true);
    try {
      const credentialId = passkeyId.trim() || getLastRegisteredPasskeyId();
      const trimmedTotp = passkeyTotpCode.trim() || undefined;
      const res = onLoginWithPasskey
        ? await onLoginWithPasskey(credentialId, trimmedTotp)
        : await (apiClient ?? new HubApiClient()).loginWithPasskey({
            credential_id: credentialId,
            ...(trimmedTotp ? { totp_code: trimmedTotp } : {}),
          });
      if (res && res.requires_totp && !res.ok) {
        setLocalError(
          "Multi-factor authentication required. Please enter your 6-digit MFA / TOTP code.",
        );
        return;
      }
      setPasskeyId("");
      setPasskeyTotpCode("");
      onAuthenticated?.(res ? res.authority : undefined);
      onClose();
    } catch (err) {
      const requiresTotp =
        (err &&
          typeof err === "object" &&
          (err as { requires_totp?: boolean }).requires_totp === true) ||
        (err instanceof Error &&
          /totp|mfa|multi-factor/i.test(err.message));
      if (requiresTotp) {
        setLocalError(
          "Multi-factor authentication required. Please enter your 6-digit MFA / TOTP code.",
        );
      } else {
        setLocalError(
          err instanceof Error ? err.message : "Passkey authentication failed",
        );
      }
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div
      className="fd-modal-backdrop"
      role="dialog"
      aria-modal="true"
      aria-label="Pair with FerroLoop Hub"
    >
      <div className="fd-modal">
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            marginBottom: "12px",
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

        {/* Auth Mode Switcher Tabs (Record 11) */}
        <div
          role="tablist"
          aria-label="Authentication Mode"
          style={{
            display: "flex",
            gap: "6px",
            marginBottom: "14px",
            borderBottom: "1px solid var(--border-subtle, #1e293b)",
            paddingBottom: "10px",
          }}
        >
          <button
            type="button"
            role="tab"
            aria-selected={authMode === "token"}
            data-testid="auth-tab-token"
            className={`fd-btn${authMode === "token" ? " fd-btn-active" : ""}`}
            onClick={() => {
              setLocalError(null);
              setAuthMode("token");
            }}
          >
            Token &amp; Pairing
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={authMode === "password"}
            data-testid="auth-tab-password"
            className={`fd-btn${authMode === "password" ? " fd-btn-active" : ""}`}
            onClick={() => {
              setLocalError(null);
              setAuthMode("password");
            }}
          >
            Password &amp; MFA
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={authMode === "passkey"}
            data-testid="auth-tab-passkey"
            className={`fd-btn${authMode === "passkey" ? " fd-btn-active" : ""}`}
            onClick={() => {
              setLocalError(null);
              setAuthMode("passkey");
            }}
          >
            Passkey (WebAuthn)
          </button>
        </div>

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

        {authMode === "token" && (
          <>
            <p
              style={{
                margin: "0 0 14px 0",
                fontSize: "12px",
                color: "var(--text-secondary)",
              }}
            >
              Run <code className="fd-mono">fl hub pair</code> on the host to
              generate a 10-minute pairing code, or paste an Owner/Peer bearer
              token directly.
            </p>

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
          </>
        )}

        {authMode === "password" && (
          <form
            onSubmit={handlePasswordSubmit}
            style={{ display: "flex", flexDirection: "column", gap: "12px" }}
          >
            <div>
              <label
                htmlFor="fd-auth-username-input"
                style={{
                  display: "block",
                  fontSize: "11px",
                  fontWeight: 600,
                  color: "var(--text-muted)",
                  marginBottom: "4px",
                  textTransform: "uppercase",
                }}
              >
                Operator Username
              </label>
              <input
                id="fd-auth-username-input"
                type="text"
                data-testid="auth-username-input"
                className="fd-input"
                placeholder="e.g. lmcfarlin"
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                disabled={submitting}
                style={{ width: "100%" }}
              />
            </div>

            <div>
              <label
                htmlFor="fd-auth-password-input"
                style={{
                  display: "block",
                  fontSize: "11px",
                  fontWeight: 600,
                  color: "var(--text-muted)",
                  marginBottom: "4px",
                  textTransform: "uppercase",
                }}
              >
                Password
              </label>
              <input
                id="fd-auth-password-input"
                type="password"
                data-testid="auth-password-input"
                className="fd-input"
                placeholder="Enter operator password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                disabled={submitting}
                style={{ width: "100%" }}
              />
            </div>

            <div>
              <label
                htmlFor="fd-auth-totp-input"
                style={{
                  display: "block",
                  fontSize: "11px",
                  fontWeight: 600,
                  color: "var(--text-muted)",
                  marginBottom: "4px",
                  textTransform: "uppercase",
                }}
              >
                TOTP 6-Digit MFA Code (if enabled)
              </label>
              <input
                id="fd-auth-totp-input"
                type="text"
                inputMode="numeric"
                data-testid="auth-totp-input"
                className="fd-input fd-mono"
                placeholder="123456"
                value={totpCode}
                onChange={(e) => setTotpCode(e.target.value)}
                disabled={submitting}
                style={{ width: "100%" }}
              />
            </div>

            <div style={{ display: "flex", justifyContent: "flex-end" }}>
              <button
                type="submit"
                data-testid="auth-password-submit-btn"
                className="fd-btn fd-btn-primary"
                disabled={submitting}
              >
                {submitting ? "Signing In..." : "Sign In with Password"}
              </button>
            </div>
          </form>
        )}

        {authMode === "passkey" && (
          <form
            onSubmit={handlePasskeySubmit}
            style={{ display: "flex", flexDirection: "column", gap: "12px" }}
          >
            <p
              style={{
                margin: 0,
                fontSize: "12px",
                color: "var(--text-secondary)",
              }}
            >
              Authenticate using a registered WebAuthn hardware security key or
              platform passkey.
            </p>

            <div>
              <label
                htmlFor="fd-auth-passkey-id-input"
                style={{
                  display: "block",
                  fontSize: "11px",
                  fontWeight: 600,
                  color: "var(--text-muted)",
                  marginBottom: "4px",
                  textTransform: "uppercase",
                }}
              >
                Passkey Credential ID (optional override)
              </label>
              <input
                id="fd-auth-passkey-id-input"
                type="text"
                data-testid="auth-passkey-id-input"
                className="fd-input fd-mono"
                placeholder="cred-webauthn-1"
                value={passkeyId}
                onChange={(e) => setPasskeyId(e.target.value)}
                disabled={submitting}
                style={{ width: "100%" }}
              />
            </div>

            <div>
              <label
                htmlFor="fd-pairing-passkey-totp-input"
                style={{
                  display: "block",
                  fontSize: "11px",
                  fontWeight: 600,
                  color: "var(--text-muted)",
                  marginBottom: "4px",
                  textTransform: "uppercase",
                }}
              >
                6-Digit MFA / TOTP Code (if enabled)
              </label>
              <input
                id="fd-pairing-passkey-totp-input"
                type="text"
                inputMode="numeric"
                data-testid="pairing-passkey-totp-input"
                className="fd-input fd-mono"
                placeholder="123456"
                value={passkeyTotpCode}
                onChange={(e) => setPasskeyTotpCode(e.target.value)}
                disabled={submitting}
                style={{ width: "100%" }}
              />
            </div>

            <div style={{ display: "flex", justifyContent: "flex-end" }}>
              <button
                type="submit"
                data-testid="auth-passkey-submit-btn"
                className="fd-btn fd-btn-primary"
                disabled={submitting}
              >
                {submitting ? "Verifying Passkey..." : "Sign In with Passkey"}
              </button>
            </div>
          </form>
        )}
      </div>
    </div>
  );
};
