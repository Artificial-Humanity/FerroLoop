import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import {
  HubApiClient,
  base64UrlToBytes,
} from "../api/client";
import { PairingModal } from "../components/PairingModal";
import { SettingsModal } from "../components/SettingsModal";
import type {
  CloudAdaptersConfig,
  OperatorAccount,
} from "../types/api";

describe("Wave C UI — Record 11 (Multi-Operator Auth, Passkeys, TOTP & RBAC) + Records 52 & 53 (Cloud Sync & Gate Adapters)", () => {
  describe("Record 11 — PairingModal Multi-Mode Sign-In (Token | Password & MFA | Passkey)", () => {
    it("switches between Token, Password & MFA, and Passkey tabs and authenticates via loginWithPassword and loginWithPasskey", async () => {
      const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
      const passwordSpy = vi
        .spyOn(client, "loginWithPassword")
        .mockResolvedValue({
          ok: true,
          authority: { tier: "owner", login: "lmcfarlin" },
          session_token: "sess-tok-pwd",
        });
      const passkeySpy = vi
        .spyOn(client, "loginWithPasskey")
        .mockResolvedValue({
          ok: true,
          authority: { tier: "owner", login: "lmcfarlin" },
          session_token: "sess-tok-pk",
        });

      const onClose = vi.fn();
      const onAuthenticated = vi.fn();

      render(
        <PairingModal
          isOpen={true}
          onClose={onClose}
          onPairWithCode={vi.fn()}
          onPairWithToken={vi.fn()}
          onAuthenticated={onAuthenticated}
          apiClient={client}
        />,
      );

      // Default tab is Token
      expect(screen.getByTestId("auth-tab-token")).toHaveAttribute(
        "aria-selected",
        "true",
      );

      // Switch to Password & MFA tab
      fireEvent.click(screen.getByTestId("auth-tab-password"));
      expect(screen.getByTestId("auth-tab-password")).toHaveAttribute(
        "aria-selected",
        "true",
      );

      fireEvent.change(screen.getByTestId("auth-username-input"), {
        target: { value: "lmcfarlin" },
      });
      fireEvent.change(screen.getByTestId("auth-password-input"), {
        target: { value: "correct-horse-battery-staple" },
      });
      fireEvent.change(screen.getByTestId("auth-totp-input"), {
        target: { value: "654321" },
      });

      fireEvent.click(screen.getByTestId("auth-password-submit-btn"));

      await waitFor(() => {
        expect(passwordSpy).toHaveBeenCalledWith({
          username: "lmcfarlin",
          password: "correct-horse-battery-staple",
          totp_code: "654321",
        });
      });
      expect(onAuthenticated).toHaveBeenCalledWith({
        tier: "owner",
        login: "lmcfarlin",
      });

      // Switch to Passkey tab
      fireEvent.click(screen.getByTestId("auth-tab-passkey"));
      expect(screen.getByTestId("auth-tab-passkey")).toHaveAttribute(
        "aria-selected",
        "true",
      );

      fireEvent.change(screen.getByTestId("auth-passkey-id-input"), {
        target: { value: "cred-yubikey-01" },
      });
      fireEvent.click(screen.getByTestId("auth-passkey-submit-btn"));

      await waitFor(() => {
        expect(passkeySpy).toHaveBeenCalledWith({
          credential_id: "cred-yubikey-01",
        });
      });
    });
  });

  describe("Record 11 — SettingsModal Operators, Passkeys & MFA Section", () => {
    it("loads operators, adds and removes operators, enrolls/verifies TOTP MFA, and registers a passkey", async () => {
      const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
      const initialOperators: OperatorAccount[] = [
        {
          id: "op-owner-1",
          username: "lmcfarlin",
          display_name: "L. McFarlin",
          role: "owner",
          has_password: true,
          totp_enabled: true,
          passkeys_count: 2,
          created_at_ms: 1700000000000,
        },
      ];

      vi.spyOn(client, "listOperators").mockResolvedValue({
        operators: initialOperators,
      });

      const createOpSpy = vi
        .spyOn(client, "createOperator")
        .mockImplementation(async (req) => ({
          operator: {
            id: `op-${req.username}`,
            username: req.username,
            display_name: req.display_name ?? req.username,
            role: req.role,
            has_password: Boolean(req.password),
            totp_enabled: false,
            passkeys_count: 0,
            created_at_ms: 1700000100000,
          },
        }));

      const deleteOpSpy = vi
        .spyOn(client, "deleteOperator")
        .mockResolvedValue({ ok: true });

      const enrollTotpSpy = vi
        .spyOn(client, "enrollTotp")
        .mockResolvedValue({
          secret_base32: "NB2W45DFOIZA====",
          otpauth_uri:
            "otpauth://totp/FerroDeck:lmcfarlin?secret=NB2W45DFOIZA====&issuer=FerroDeck",
        });

      const verifyTotpSpy = vi
        .spyOn(client, "verifyTotp")
        .mockResolvedValue({ ok: true });

      const registerPasskeySpy = vi
        .spyOn(client, "registerPasskey")
        .mockResolvedValue({ ok: true });

      render(
        <SettingsModal isOpen={true} onClose={vi.fn()} apiClient={client} />,
      );

      const section = screen.getByTestId("settings-operators-section");
      expect(section).toBeInTheDocument();

      const ownerRow = await within(section).findByTestId(
        "operator-row-lmcfarlin",
      );
      expect(ownerRow).toHaveTextContent("owner");
      expect(ownerRow).toHaveTextContent("TOTP: Enabled");
      expect(ownerRow).toHaveTextContent("Passkeys: 2");

      // Add new operator
      fireEvent.change(within(section).getByTestId("operator-username-input"), {
        target: { value: "jarvis_ops" },
      });
      fireEvent.change(within(section).getByTestId("operator-role-select"), {
        target: { value: "admin" },
      });
      fireEvent.change(within(section).getByTestId("operator-password-input"), {
        target: { value: "admin-pass-123" },
      });
      fireEvent.click(within(section).getByTestId("add-operator-submit-btn"));

      await waitFor(() => {
        expect(createOpSpy).toHaveBeenCalledWith({
          username: "jarvis_ops",
          display_name: "jarvis_ops",
          role: "admin",
          password: "admin-pass-123",
        });
      });

      const addedRow = await within(section).findByTestId(
        "operator-row-jarvis_ops",
      );
      expect(addedRow).toHaveTextContent("admin");

      // Delete the newly added operator
      fireEvent.click(
        within(section).getByTestId("delete-operator-jarvis_ops"),
      );
      await waitFor(() => {
        expect(deleteOpSpy).toHaveBeenCalledWith("op-jarvis_ops");
      });
      expect(
        within(section).queryByTestId("operator-row-jarvis_ops"),
      ).not.toBeInTheDocument();

      // Enroll TOTP MFA
      fireEvent.click(within(section).getByTestId("enroll-totp-btn"));
      await waitFor(() => {
        expect(enrollTotpSpy).toHaveBeenCalled();
      });

      const secretDisplay = await within(section).findByTestId(
        "totp-secret-display",
      );
      expect(secretDisplay).toHaveTextContent("NB2W45DFOIZA====");

      fireEvent.change(within(section).getByTestId("totp-verify-input"), {
        target: { value: "112233" },
      });
      fireEvent.click(within(section).getByTestId("totp-verify-btn"));

      await waitFor(() => {
        expect(verifyTotpSpy).toHaveBeenCalledWith("112233");
      });
      expect(
        await within(section).findByTestId("totp-verified-badge"),
      ).toBeInTheDocument();

      // Register Passkey
      fireEvent.click(within(section).getByTestId("register-passkey-btn"));
      await waitFor(() => {
        expect(registerPasskeySpy).toHaveBeenCalledWith(
          expect.objectContaining({
            label: "Operator Passkey",
          }),
        );
      });
    });
  });

  describe("Records 52 & 53 — SettingsModal Cloud Sync & Edge Gate Adapters Section", () => {
    it("loads and updates CloudAdaptersConfig via getCloudAdapters and updateCloudAdapters", async () => {
      const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
      const initialCloud: CloudAdaptersConfig = {
        store_sync: {
          provider: "none",
          sync_mode: "write_through",
          endpoint_url: "",
          enabled: false,
        },
        gate_cloud: {
          registry_provider: "none",
          relay_endpoint_url: "",
          secret_manager_enabled: false,
          model_armor_enabled: false,
        },
      };

      vi.spyOn(client, "getCloudAdapters").mockResolvedValue(initialCloud);
      const updateCloudSpy = vi
        .spyOn(client, "updateCloudAdapters")
        .mockImplementation(async (cfg) => cfg as CloudAdaptersConfig);

      render(
        <SettingsModal isOpen={true} onClose={vi.fn()} apiClient={client} />,
      );

      const cloudSection = screen.getByTestId(
        "settings-cloud-adapters-section",
      );
      expect(cloudSection).toBeInTheDocument();

      fireEvent.change(
        within(cloudSection).getByTestId("cloud-sync-provider-select"),
        { target: { value: "gcp" } },
      );
      fireEvent.change(
        within(cloudSection).getByTestId("cloud-sync-mode-select"),
        { target: { value: "write_behind" } },
      );
      fireEvent.change(
        within(cloudSection).getByTestId("cloud-sync-endpoint-input"),
        { target: { value: "https://sql.artificialhumanity.io" } },
      );
      fireEvent.click(
        within(cloudSection).getByTestId("toggle-cloud-sync-enabled"),
      );

      fireEvent.change(
        within(cloudSection).getByTestId("cloud-gate-provider-select"),
        { target: { value: "gcp" } },
      );
      fireEvent.change(
        within(cloudSection).getByTestId("cloud-relay-endpoint-input"),
        { target: { value: "https://ferrogate-relay.a.run.app" } },
      );
      fireEvent.click(
        within(cloudSection).getByTestId("toggle-cloud-secret-manager"),
      );
      fireEvent.click(
        within(cloudSection).getByTestId("toggle-cloud-model-armor"),
      );

      fireEvent.click(
        within(cloudSection).getByTestId("save-cloud-adapters-btn"),
      );

      await waitFor(() => {
        expect(updateCloudSpy).toHaveBeenCalledWith({
          store_sync: {
            provider: "gcp",
            sync_mode: "write_behind",
            endpoint_url: "https://sql.artificialhumanity.io",
            enabled: true,
          },
          gate_cloud: {
            registry_provider: "gcp",
            relay_endpoint_url: "https://ferrogate-relay.a.run.app",
            secret_manager_enabled: true,
            model_armor_enabled: true,
          },
        });
      });

      expect(
        await screen.findByTestId("cloud-adapters-saved-banner"),
      ).toBeInTheDocument();
    });
  });

  describe("Record 11 — WebCrypto P-256 Passkey Registration & Challenge-Signed Assertion Login", () => {
    it("generates a 65-byte SEC1 P-256 public_key_b64 in registerPasskey and signs POST /api/v1/auth/passkey/challenge with ASN.1 DER in loginWithPasskey", async () => {
      const capturedRegisterBody: Record<string, unknown> = {};
      const capturedLoginBody: Record<string, unknown> = {};

      const mockFetch = vi.fn(
        async (input: RequestInfo | URL, init?: RequestInit) => {
          const url = String(input);
          if (url.endsWith("/api/v1/auth/passkey/register")) {
            Object.assign(
              capturedRegisterBody,
              JSON.parse(String(init?.body ?? "{}")),
            );
            return new Response(JSON.stringify({ ok: true }), { status: 200 });
          }
          if (url.endsWith("/api/v1/auth/passkey/challenge")) {
            return new Response(
              JSON.stringify({
                challenge: "test-challenge-base64url-0123456789abcdef",
                expires_in_secs: 300,
              }),
              { status: 200 },
            );
          }
          if (url.endsWith("/api/v1/auth/login")) {
            Object.assign(
              capturedLoginBody,
              JSON.parse(String(init?.body ?? "{}")),
            );
            return new Response(
              JSON.stringify({
                ok: true,
                authority: { tier: "owner", login: "lmcfarlin" },
                session_token: "sess-passkey-real",
              }),
              { status: 200 },
            );
          }
          return new Response(JSON.stringify({}), { status: 200 });
        },
      );

      const client = new HubApiClient({
        baseUrl: "http://localhost:9000",
        fetchImpl: mockFetch as unknown as typeof fetch,
      });

      const credId = "passkey-webcrypto-test-0001";
      await client.registerPasskey({
        credential_id: credId,
        label: "Lab YubiKey",
      });

      expect(capturedRegisterBody.credential_id).toBe(credId);
      const pubKeyB64 = String(capturedRegisterBody.public_key_b64 ?? "");
      expect(pubKeyB64.length).toBeGreaterThan(80);
      const pubKeyBytes = base64UrlToBytes(pubKeyB64);
      expect(pubKeyBytes.length).toBe(65);
      expect(pubKeyBytes[0]).toBe(0x04);

      const loginRes = await client.loginWithPasskey({
        credential_id: credId,
      });
      expect(loginRes.ok).toBe(true);
      expect(capturedLoginBody.mode).toBe("passkey");
      expect(capturedLoginBody.credential_id).toBe(credId);

      const clientDataBytes = base64UrlToBytes(
        String(capturedLoginBody.client_data_json_b64 ?? ""),
      );
      const clientData = JSON.parse(
        new TextDecoder().decode(clientDataBytes),
      ) as Record<string, unknown>;
      expect(clientData.type).toBe("webauthn.get");
      expect(clientData.challenge).toBe(
        "test-challenge-base64url-0123456789abcdef",
      );
      expect(clientData.crossOrigin).toBe(false);

      const authDataBytes = base64UrlToBytes(
        String(capturedLoginBody.authenticator_data_b64 ?? ""),
      );
      expect(authDataBytes.length).toBe(37);
      expect(authDataBytes[32]).toBe(0x01); // UP flag
      expect(authDataBytes[36]).toBe(1); // signCount === 1

      const sigDerBytes = base64UrlToBytes(
        String(capturedLoginBody.signature_b64 ?? ""),
      );
      expect(sigDerBytes[0]).toBe(0x30); // ASN.1 SEQUENCE
      expect(sigDerBytes[2]).toBe(0x02); // ASN.1 INTEGER (r)
    });
  });
});
