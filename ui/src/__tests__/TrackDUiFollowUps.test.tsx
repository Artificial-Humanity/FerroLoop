import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  HubApiClient,
  HubApiError,
  PASSKEY_IDB_DATABASE_NAME,
  PASSKEY_IDB_STORE_NAME,
  PASSKEY_KEYSTORE_STORAGE_KEY,
  base64UrlToBytes,
} from "../api/client";
import { ChatPane, sessionDraftMemory } from "../components/ChatPane";
import { HubDrawer } from "../components/HubDrawer";
import { PairingModal } from "../components/PairingModal";
import { RichMarkdown } from "../components/RichMarkdown";
import { TerminalPane } from "../components/TerminalPane";
import type { ArchitectureResponse, HubWsFrame } from "../types/api";

function derToP1363(der: Uint8Array): Uint8Array<ArrayBuffer> {
  if (der[0] !== 0x30) {
    throw new Error("Expected DER SEQUENCE");
  }
  let offset = 2;
  if (der[offset] !== 0x02) {
    throw new Error("Expected DER INTEGER for r");
  }
  const rLen = der[offset + 1];
  const rBytes = der.subarray(offset + 2, offset + 2 + rLen);
  offset += 2 + rLen;
  if (der[offset] !== 0x02) {
    throw new Error("Expected DER INTEGER for s");
  }
  const sLen = der[offset + 1];
  const sBytes = der.subarray(offset + 2, offset + 2 + sLen);

  const out = new Uint8Array(64);
  const rTrim = rBytes.length > 32 ? rBytes.subarray(rBytes.length - 32) : rBytes;
  const sTrim = sBytes.length > 32 ? sBytes.subarray(sBytes.length - 32) : sBytes;
  out.set(rTrim, 32 - rTrim.length);
  out.set(sTrim, 64 - sTrim.length);
  return out;
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/**
 * Minimal in-memory IndexedDB fake for jsdom that supports `open`,
 * `onupgradeneeded`, `createObjectStore`, and `transaction` (`get` / `put`).
 */
function createFakeIndexedDb() {
  const databases = new Map<string, Map<string, Map<string, unknown>>>();

  const open = vi.fn((dbName: string, _version?: number) => {
    const req: {
      result: unknown;
      error: DOMException | null;
      onupgradeneeded: ((ev: Event) => void) | null;
      onsuccess: ((ev: Event) => void) | null;
      onerror: ((ev: Event) => void) | null;
    } = {
      result: null,
      error: null,
      onupgradeneeded: null,
      onsuccess: null,
      onerror: null,
    };

    const isNew = !databases.has(dbName);
    if (isNew) {
      databases.set(dbName, new Map());
    }
    const stores = databases.get(dbName)!;

    const db = {
      objectStoreNames: {
        contains: (name: string) => stores.has(name),
      },
      createObjectStore: (name: string) => {
        if (!stores.has(name)) {
          stores.set(name, new Map());
        }
        return {};
      },
      transaction: (storeName: string, _mode?: IDBTransactionMode) => {
        if (!stores.has(storeName)) {
          stores.set(storeName, new Map());
        }
        const storeMap = stores.get(storeName)!;
        return {
          objectStore: () => ({
            put: (value: unknown, key: string) => {
              const putReq: {
                result: unknown;
                error: DOMException | null;
                onsuccess: ((ev: Event) => void) | null;
                onerror: ((ev: Event) => void) | null;
              } = {
                result: key,
                error: null,
                onsuccess: null,
                onerror: null,
              };
              storeMap.set(key, value);
              queueMicrotask(() => {
                putReq.onsuccess?.(new Event("success"));
              });
              return putReq;
            },
            get: (key: string) => {
              const getReq: {
                result: unknown;
                error: DOMException | null;
                onsuccess: ((ev: Event) => void) | null;
                onerror: ((ev: Event) => void) | null;
              } = {
                result: storeMap.get(key),
                error: null,
                onsuccess: null,
                onerror: null,
              };
              queueMicrotask(() => {
                getReq.onsuccess?.(new Event("success"));
              });
              return getReq;
            },
          }),
        };
      },
      close: vi.fn(),
    };

    req.result = db;
    queueMicrotask(() => {
      if (isNew) {
        req.onupgradeneeded?.(new Event("upgradeneeded"));
      }
      req.onsuccess?.(new Event("success"));
    });

    return req as unknown as IDBOpenDBRequest;
  });

  return {
    indexedDB: { open } as unknown as IDBFactory,
    databases,
    getStoredKey: (credentialId: string): unknown =>
      databases
        .get(PASSKEY_IDB_DATABASE_NAME)
        ?.get(PASSKEY_IDB_STORE_NAME)
        ?.get(credentialId),
  };
}

describe("Track D UI Follow-ups — Wave C F-3 & FerroDeck UI Audit O-3, O-4, O-6", () => {
  beforeEach(() => {
    window.localStorage.clear();
    sessionDraftMemory.clear();
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  describe("Wave C F-3 — Non-Extractable Passkey Private Keys in IndexedDB & Passkey MFA Support", () => {
    it("generates non-extractable ECDSA P-256 CryptoKeys in IndexedDB and omits private_jwk from localStorage", async () => {
      const fakeIdb = createFakeIndexedDb();
      vi.stubGlobal("indexedDB", fakeIdb.indexedDB);

      const generateKeySpy = vi.spyOn(globalThis.crypto.subtle, "generateKey");
      const calls: Array<{ url: string; body: Record<string, unknown> }> = [];
      const fetchImpl: typeof fetch = vi.fn(async (input, init) => {
        const url = String(input);
        const body = init?.body ? JSON.parse(String(init.body)) : {};
        calls.push({ url, body });
        if (url.endsWith("/api/v1/auth/passkey/register")) {
          return new Response(JSON.stringify({ ok: true }), { status: 200 });
        }
        if (url.endsWith("/api/v1/auth/passkey/challenge")) {
          return new Response(
            JSON.stringify({ challenge: "challenge-idb-001", expires_in_secs: 60 }),
            { status: 200 },
          );
        }
        if (url.endsWith("/api/v1/auth/login")) {
          return new Response(
            JSON.stringify({
              ok: true,
              session_token: "sess-idb-tok",
              authority: { tier: "owner", login: "lmcfarlin" },
            }),
            { status: 200 },
          );
        }
        return new Response(JSON.stringify({ ok: true }), { status: 200 });
      });

      const client = new HubApiClient({ baseUrl: "http://localhost:9000", fetchImpl });
      const credentialId = "passkey-idb-nonextractable-0001";

      await client.registerPasskey({
        credential_id: credentialId,
        label: "YubiKey 5C Nano",
      });

      // 1. Keypair was generated with extractable: false
      expect(generateKeySpy).toHaveBeenCalledWith(
        { name: "ECDSA", namedCurve: "P-256" },
        false,
        ["sign", "verify"],
      );

      // 2. Non-extractable private CryptoKey is stored in IndexedDB
      const idbKey = fakeIdb.getStoredKey(credentialId) as CryptoKey | undefined;
      expect(idbKey).toBeDefined();
      expect(idbKey?.type).toBe("private");
      expect(idbKey?.extractable).toBe(false);

      // 3. localStorage contains public metadata only — no private_jwk
      const rawKeystore = window.localStorage.getItem(PASSKEY_KEYSTORE_STORAGE_KEY);
      expect(rawKeystore).toBeTruthy();
      const parsedKeystore = JSON.parse(rawKeystore!);
      expect(parsedKeystore[credentialId]).toBeDefined();
      expect(parsedKeystore[credentialId].credential_id).toBe(credentialId);
      expect(parsedKeystore[credentialId].label).toBe("YubiKey 5C Nano");
      expect(typeof parsedKeystore[credentialId].public_key_b64).toBe("string");
      expect(parsedKeystore[credentialId].sign_count).toBe(0);
      expect(parsedKeystore[credentialId].private_jwk).toBeUndefined();
      expect(rawKeystore).not.toContain("private_jwk");

      // 4. loginWithPasskey signs assertion using the non-extractable CryptoKey from IndexedDB
      // and includes totp_code when passed as (credentialId, totpCode)
      const loginRes = await client.loginWithPasskey(credentialId, "654321");
      expect(loginRes.ok).toBe(true);

      const loginCall = calls.find((c) => c.url.endsWith("/api/v1/auth/login"));
      expect(loginCall).toBeDefined();
      expect(loginCall!.body.mode).toBe("passkey");
      expect(loginCall!.body.credential_id).toBe(credentialId);
      expect(loginCall!.body.totp_code).toBe("654321");
      expect(typeof loginCall!.body.signature_b64).toBe("string");

      // Verify signature with the registered public key
      const pubBytes = new Uint8Array(
        base64UrlToBytes(parsedKeystore[credentialId].public_key_b64),
      );
      const pubKey = await globalThis.crypto.subtle.importKey(
        "raw",
        pubBytes,
        { name: "ECDSA", namedCurve: "P-256" },
        true,
        ["verify"],
      );
      const authData = base64UrlToBytes(String(loginCall!.body.authenticator_data_b64));
      const clientData = new Uint8Array(
        base64UrlToBytes(String(loginCall!.body.client_data_json_b64)),
      );
      const clientDataHash = new Uint8Array(
        await globalThis.crypto.subtle.digest("SHA-256", clientData),
      );
      const signedData = new Uint8Array(authData.length + clientDataHash.length);
      signedData.set(authData, 0);
      signedData.set(clientDataHash, authData.length);
      const derSig = base64UrlToBytes(String(loginCall!.body.signature_b64));
      const valid = await globalThis.crypto.subtle.verify(
        { name: "ECDSA", hash: "SHA-256" },
        pubKey,
        derToP1363(derSig),
        signedData,
      );
      expect(valid).toBe(true);

      // 5. Updated keystore in localStorage still has no private_jwk and sign_count = 1
      const updatedKeystore = JSON.parse(
        window.localStorage.getItem(PASSKEY_KEYSTORE_STORAGE_KEY)!,
      );
      expect(updatedKeystore[credentialId].sign_count).toBe(1);
      expect(updatedKeystore[credentialId].private_jwk).toBeUndefined();
    });

    it("migrates legacy private_jwk entries to IndexedDB and strips private_jwk on login", async () => {
      const fakeIdb = createFakeIndexedDb();
      vi.stubGlobal("indexedDB", fakeIdb.indexedDB);

      const kp = await globalThis.crypto.subtle.generateKey(
        { name: "ECDSA", namedCurve: "P-256" },
        true,
        ["sign", "verify"],
      );
      const rawPub = new Uint8Array(
        await globalThis.crypto.subtle.exportKey("raw", kp.publicKey),
      );
      const privJwk = await globalThis.crypto.subtle.exportKey("jwk", kp.privateKey);
      const legacyId = "legacy-jwk-passkey-0001";

      window.localStorage.setItem(
        PASSKEY_KEYSTORE_STORAGE_KEY,
        JSON.stringify({
          [legacyId]: {
            credential_id: legacyId,
            public_key_b64: btoa(String.fromCharCode(...rawPub))
              .replace(/\+/g, "-")
              .replace(/\//g, "_")
              .replace(/=+$/g, ""),
            private_jwk: privJwk,
            sign_count: 3,
          },
        }),
      );

      const fetchImpl: typeof fetch = vi.fn(async (input) => {
        const url = String(input);
        if (url.endsWith("/api/v1/auth/passkey/challenge")) {
          return new Response(
            JSON.stringify({ challenge: "migrate-chal", expires_in_secs: 60 }),
            { status: 200 },
          );
        }
        return new Response(JSON.stringify({ ok: true }), { status: 200 });
      });

      const client = new HubApiClient({ baseUrl: "http://localhost:9000", fetchImpl });
      await client.loginWithPasskey({ credential_id: legacyId });

      const migratedKey = fakeIdb.getStoredKey(legacyId) as CryptoKey | undefined;
      expect(migratedKey).toBeDefined();
      expect(migratedKey?.extractable).toBe(false);

      const afterStore = JSON.parse(
        window.localStorage.getItem(PASSKEY_KEYSTORE_STORAGE_KEY)!,
      );
      expect(afterStore[legacyId].sign_count).toBe(4);
      expect(afterStore[legacyId].private_jwk).toBeUndefined();
    });

    it("falls back gracefully to JWK storage when indexedDB.open rejects (e.g. Firefox Private Browsing SecurityError)", async () => {
      const rejectingOpen = vi.fn(() => {
        const req: {
          result: unknown;
          error: DOMException | null;
          onupgradeneeded: ((ev: Event) => void) | null;
          onsuccess: ((ev: Event) => void) | null;
          onerror: ((ev: Event) => void) | null;
        } = {
          result: null,
          error: new DOMException("The operation is insecure.", "SecurityError"),
          onupgradeneeded: null,
          onsuccess: null,
          onerror: null,
        };
        queueMicrotask(() => {
          req.onerror?.(new Event("error"));
        });
        return req as unknown as IDBOpenDBRequest;
      });
      vi.stubGlobal("indexedDB", { open: rejectingOpen } as unknown as IDBFactory);

      const fetchImpl: typeof fetch = vi.fn(async () => {
        return new Response(JSON.stringify({ ok: true }), { status: 200 });
      });

      const client = new HubApiClient({ baseUrl: "http://localhost:9000", fetchImpl });
      const credentialId = "passkey-idb-reject-fallback-0001";

      await expect(
        client.registerPasskey({
          credential_id: credentialId,
          label: "Private Browsing Key",
        }),
      ).resolves.toEqual({ ok: true });

      const rawKeystore = window.localStorage.getItem(PASSKEY_KEYSTORE_STORAGE_KEY);
      expect(rawKeystore).toBeTruthy();
      const parsedKeystore = JSON.parse(rawKeystore!);
      expect(parsedKeystore[credentialId]).toBeDefined();
      expect(parsedKeystore[credentialId].private_jwk).toBeDefined();
    });

    it("renders pairing-passkey-totp-input in PairingModal, passes totpCode to onLoginWithPasskey, and prompts on requires_totp", async () => {
      const onLoginWithPasskey = vi
        .fn()
        .mockResolvedValueOnce({ ok: false, requires_totp: true })
        .mockRejectedValueOnce(
          new HubApiError(401, "TOTP code is required for this account", undefined, true),
        )
        .mockRejectedValueOnce(
          new HubApiError(401, "Invalid TOTP code", undefined, false),
        )
        .mockResolvedValueOnce({
          ok: true,
          authority: { tier: "owner", login: "lmcfarlin" },
        });
      const onClose = vi.fn();
      const onAuthenticated = vi.fn();

      render(
        <PairingModal
          isOpen={true}
          onClose={onClose}
          onPairWithCode={vi.fn()}
          onPairWithToken={vi.fn()}
          onLoginWithPasskey={onLoginWithPasskey}
          onAuthenticated={onAuthenticated}
        />,
      );

      fireEvent.click(screen.getByTestId("auth-tab-passkey"));
      fireEvent.change(screen.getByTestId("auth-passkey-id-input"), {
        target: { value: "cred-mfa-passkey-01" },
      });

      // 1. Submit without TOTP when hub returns { ok: false, requires_totp: true }
      fireEvent.click(screen.getByTestId("auth-passkey-submit-btn"));
      await waitFor(() => {
        expect(onLoginWithPasskey).toHaveBeenCalledWith("cred-mfa-passkey-01", undefined);
      });
      expect(await screen.findByRole("alert")).toHaveTextContent(
        /Multi-factor authentication required.*6-digit MFA/i,
      );
      expect(onClose).not.toHaveBeenCalled();

      // 2. Submit when hub throws 401 HubApiError with requires_totp: true
      fireEvent.click(screen.getByTestId("auth-passkey-submit-btn"));
      await waitFor(() => {
        expect(onLoginWithPasskey).toHaveBeenCalledTimes(2);
      });
      expect(await screen.findByRole("alert")).toHaveTextContent(
        /Multi-factor authentication required.*6-digit MFA/i,
      );

      // 3. Enter an invalid 6-digit MFA code: surfaces the server error instead of generic prompt
      const totpInput = screen.getByTestId("pairing-passkey-totp-input");
      fireEvent.change(totpInput, { target: { value: "000000" } });
      fireEvent.click(screen.getByTestId("auth-passkey-submit-btn"));
      await waitFor(() => {
        expect(onLoginWithPasskey).toHaveBeenCalledWith("cred-mfa-passkey-01", "000000");
      });
      expect(await screen.findByRole("alert")).toHaveTextContent("Invalid TOTP code");

      // 4. Enter valid 6-digit MFA code in pairing-passkey-totp-input and submit
      fireEvent.change(totpInput, { target: { value: "482910" } });
      fireEvent.click(screen.getByTestId("auth-passkey-submit-btn"));

      await waitFor(() => {
        expect(onLoginWithPasskey).toHaveBeenLastCalledWith(
          "cred-mfa-passkey-01",
          "482910",
        );
      });
      expect(onAuthenticated).toHaveBeenCalledWith({
        tier: "owner",
        login: "lmcfarlin",
      });
      expect(onClose).toHaveBeenCalledTimes(1);
    });
  });

  describe("FerroDeck UI Audit O-3 — ChatPane Per-Target Submitting State & Standalone Target Guards", () => {
    it("scopes submitting state per target so sending to Agent A does not disable Send on Agent B", async () => {
      const pendingA = deferred<unknown>();
      const pendingB = deferred<unknown>();
      const onSendPrompt = vi.fn((target: string) =>
        target === "sess-a" ? pendingA.promise : pendingB.promise,
      );

      const { rerender } = render(
        <ChatPane
          agentName="alpha"
          sessionId="sess-a"
          turns={[]}
          onSendPrompt={onSendPrompt}
        />,
      );

      const getComposer = () =>
        screen.getByPlaceholderText(/Send a message/i) as HTMLTextAreaElement;
      const getSendBtn = () =>
        screen.getByRole("button", {
          name: /Send Message|Sending/i,
        }) as HTMLButtonElement;

      fireEvent.change(getComposer(), { target: { value: "Prompt for Alpha" } });
      expect(getSendBtn().disabled).toBe(false);
      fireEvent.click(getSendBtn());

      await waitFor(() => {
        expect(onSendPrompt).toHaveBeenCalledWith(
          "sess-a",
          expect.objectContaining({ prompt: "Prompt for Alpha" }),
        );
      });
      // Agent A's Send button is disabled while its send is in flight
      expect(getSendBtn().disabled).toBe(true);
      expect(getSendBtn()).toHaveTextContent(/Sending/i);

      // Switch to Agent B while Agent A's send is still in flight
      rerender(
        <ChatPane
          agentName="beta"
          sessionId="sess-b"
          turns={[]}
          onSendPrompt={onSendPrompt}
        />,
      );

      fireEvent.change(getComposer(), { target: { value: "Prompt for Beta" } });
      // Agent B's Send button is NOT disabled by Agent A's in-flight request!
      expect(getSendBtn().disabled).toBe(false);
      expect(getSendBtn()).toHaveTextContent(/^Send Message$/);

      // Switch back to Agent A while its request is still in flight: still shows Sending
      rerender(
        <ChatPane
          agentName="alpha"
          sessionId="sess-a"
          turns={[]}
          onSendPrompt={onSendPrompt}
        />,
      );
      expect(getSendBtn().disabled).toBe(true);
      expect(getSendBtn()).toHaveTextContent(/Sending/i);

      // Resolve Agent A's send
      await act(async () => {
        pendingA.resolve({ queued: false });
        await pendingA.promise;
      });
      expect(getComposer().value).toBe("");
    });

    it("guards standalone apiClient post-await turn and queue mutations when the target switched", async () => {
      const sendDeferred = deferred<{
        status: string;
        queued: boolean;
        effective_body: string;
        is_busy: boolean;
        queue: Array<{ id: string; prompt: string; created_at: string }>;
      }>();

      const mockApiClient = {
        listSlashCommands: vi.fn(async () => ({ commands: [] })),
        getSessionTranscript: vi.fn(async (targetId: string) => ({
          session_id: targetId,
          turns:
            targetId === "sess-b"
              ? [
                  {
                    index: 0,
                    role: "assistant",
                    content: "Beta existing transcript turn",
                    thinking: null,
                    tool_calls: [],
                  },
                ]
              : [],
          is_busy: false,
          queue: [],
        })),
        sendSessionPrompt: vi.fn(() => sendDeferred.promise),
      } as unknown as HubApiClient;

      const { rerender } = render(
        <ChatPane
          agentName="alpha"
          sessionId="sess-a"
          apiClient={mockApiClient}
        />,
      );

      const getComposer = () =>
        screen.getByPlaceholderText(/Send a message/i) as HTMLTextAreaElement;
      fireEvent.change(getComposer(), { target: { value: "Alpha standalone message" } });
      fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));

      await waitFor(() => {
        expect(mockApiClient.sendSessionPrompt).toHaveBeenCalledTimes(1);
      });

      // Switch to Agent B before Agent A's standalone sendSessionPrompt resolves
      rerender(
        <ChatPane
          agentName="beta"
          sessionId="sess-b"
          apiClient={mockApiClient}
        />,
      );

      expect(
        await screen.findByText("Beta existing transcript turn"),
      ).toBeInTheDocument();

      // Now resolve Agent A's sendSessionPrompt with Agent A's optimistic turn and queue
      await act(async () => {
        sendDeferred.resolve({
          status: "delivered",
          queued: false,
          effective_body: "Alpha standalone message",
          is_busy: true,
          queue: [{ id: "q-alpha-1", prompt: "Alpha queued item", created_at: "now" }],
        });
        await sendDeferred.promise;
      });

      // Agent B's transcript and queue must not be polluted by Agent A's late response
      expect(screen.queryByText("Alpha standalone message")).toBeNull();
      expect(screen.queryByText("Alpha queued item")).toBeNull();
      expect(screen.getByText("Beta existing transcript turn")).toBeInTheDocument();
    });
  });

  describe("FerroDeck UI Audit O-4 — TerminalPane Batch Frame Processing", () => {
    it("processes every new frame in terminalFrames when multiple frames arrive in a single render batch", () => {
      const initialFrames: HubWsFrame[] = [
        {
          channel: "terminal",
          event: "opened",
          data: { session_id: "term-batch-1", pid: 9001, cols: 120, rows: 36 },
        },
        {
          channel: "terminal",
          event: "output",
          data: { session_id: "term-batch-1", data: "first-line\r\n" },
        },
        {
          channel: "terminal",
          event: "output",
          data: { session_id: "term-batch-1", data: "second-line\r\n" },
        },
      ];

      const { rerender } = render(
        <TerminalPane
          authority={{ tier: "owner", login: "lmcfarlin" }}
          sessionId="term-batch-1"
          terminalFrames={initialFrames}
          onSendTerminalFrame={vi.fn()}
        />,
      );

      expect(screen.getByTestId("terminal-status-badge")).toHaveTextContent(
        /connected \(pid 9001\)/i,
      );
      const outputLog = screen.getByTestId("terminal-output-log");
      expect(outputLog).toHaveTextContent("first-line");
      expect(outputLog).toHaveTextContent("second-line");

      // Append two more frames in a single update
      const nextFrames: HubWsFrame[] = [
        ...initialFrames,
        {
          channel: "terminal",
          event: "output",
          data: { session_id: "term-batch-1", data: "third-line\r\n" },
        },
        {
          channel: "terminal",
          event: "output",
          data: { session_id: "term-batch-1", data: "fourth-line\r\n" },
        },
      ];

      rerender(
        <TerminalPane
          authority={{ tier: "owner", login: "lmcfarlin" }}
          sessionId="term-batch-1"
          terminalFrames={nextFrames}
          onSendTerminalFrame={vi.fn()}
        />,
      );

      expect(outputLog.textContent).toBe(
        "first-line\nsecond-line\nthird-line\nfourth-line\n",
      );
    });
  });

  describe("FerroDeck UI Audit O-6 — HubDrawer Copy Timeout Cleanup & RichMarkdown Domain Badge", () => {
    it("clears the 1.8s C4 DSL copy-status timeout when HubDrawer unmounts", async () => {
      vi.useFakeTimers();
      const clearTimeoutSpy = vi.spyOn(globalThis, "clearTimeout");
      Object.defineProperty(navigator, "clipboard", {
        value: { writeText: vi.fn().mockResolvedValue(undefined) },
        configurable: true,
      });

      const architecture: ArchitectureResponse = {
        likec4_dsl: "specification { element service }\nmodel { hub = service 'Hub' }",
        model: {
          components: [],
          edges: [],
        },
        conformance: {
          passed: true,
          violations: [],
        },
      } as unknown as ArchitectureResponse;

      const { unmount } = render(
        <HubDrawer defaultSubTab="architecture" architecture={architecture} />,
      );

      const copyBtn = screen.getByRole("button", { name: /Copy \.c4 DSL|Copied!/i });
      await act(async () => {
        fireEvent.click(copyBtn);
      });
      expect(copyBtn).toHaveTextContent(/Copied/i);

      unmount();
      expect(clearTimeoutSpy).toHaveBeenCalled();
      vi.runOnlyPendingTimers();
      vi.useRealTimers();
    });

    it("renders a muted (hostname) domain badge on external markdown links when the label omits the target hostname", () => {
      const { rerender } = render(
        <RichMarkdown text="Sign in at [https://bank.example/login](https://evil.example/phish)" />,
      );

      const link = screen.getByRole("link", {
        name: "https://bank.example/login",
      });
      expect(link).toHaveAttribute("href", "https://evil.example/phish");
      const badge = screen.getByTestId("markdown-link-domain-badge");
      expect(badge).toHaveTextContent("(evil.example)");

      // When the visible label already mentions the target hostname, no redundant badge is rendered
      rerender(
        <RichMarkdown text="Visit [docs.example.com guide](https://docs.example.com/getting-started)" />,
      );
      expect(screen.queryByTestId("markdown-link-domain-badge")).toBeNull();

      // Spoofed query parameter containing targetHostname still renders the (evil.example) badge
      rerender(
        <RichMarkdown text="Sign in at [https://bank.example/login?next=evil.example](https://evil.example/phish)" />,
      );
      expect(screen.getByTestId("markdown-link-domain-badge")).toHaveTextContent(
        "(evil.example)",
      );

      // Spoofed subdomain prefix containing targetHostname still renders the (evil.example) badge
      rerender(
        <RichMarkdown text="Visit [evil.example.bank.example](https://evil.example/phish)" />,
      );
      expect(screen.getByTestId("markdown-link-domain-badge")).toHaveTextContent(
        "(evil.example)",
      );

      // Hyphenated prefix lookalike (not-evil.example) still renders the (evil.example) badge
      rerender(
        <RichMarkdown text="Visit [not-evil.example](https://evil.example/phish)" />,
      );
      expect(screen.getByTestId("markdown-link-domain-badge")).toHaveTextContent(
        "(evil.example)",
      );
    });
  });
});
