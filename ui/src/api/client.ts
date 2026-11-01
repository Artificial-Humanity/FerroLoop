import {
  DEFAULT_CLOUD_ADAPTERS_CONFIG,
  DEFAULT_HUB_SETTINGS,
  DEFAULT_SLASH_COMMANDS,
  type AgentsResponse,
  type ArchitectureResponse,
  type ArtifactListResponse,
  type ArtifactPreviewResponse,
  type ArtifactVersionsResponse,
  type OfficePreviewResponse,
  type AuthSessionResponse,
  type BulkSessionAction,
  type BulkSessionActionResponse,
  type ClearSessionMode,
  type ClearSessionResponse,
  type CloudAdaptersConfig,
  type DecommissionMode,
  type DecommissionSessionResponse,
  type DiscoveredSessionSummary,
  type HealthResponse,
  type HubAuthority,
  type HubSettings,
  type HubStatsResponse,
  type InterruptSessionResponse,
  type LivenessResponse,
  type OperatorAccount,
  type OperatorRole,
  type PrMergeRequest,
  type PrMergeResponse,
  type PromptAttachment,
  type PromptRequest,
  type PromptResponse,
  type PrReviewRequest,
  type PrReviewResponse,
  type PruneArtifactsRequest,
  type PruneArtifactsResponse,
  type PruneAttachmentsRequest,
  type PruneAttachmentsResponse,
  type PullRequestItem,
  type QueuedOperatorPrompt,
  type RecastAgentRequest,
  type RecastAgentResponse,
  type SessionQueueResponse,
  type SessionTranscriptResponse,
  type SetSessionModelResponse,
  type SlashCommandEntry,
  type SpinUpSessionRequest,
  type SpinUpSessionResponse,
  type TotpEnrollmentResponse,
  type CodeSearchKind,
  type CodeSearchResponse,
  type ImportWorkspaceStateResponse,
  type StateCheckpointResponse,
  type StateEntriesResponse,
  type StateEntry,
  type StateEntryResponse,
  type StateSearchResponse,
  type UpsertStateEntryInput,
  type WireAckRequest,
  type WireAckResponse,
  type WireInboxItem,
  type WireSendRequest,
  type WireSendResponse,
  type DeleteGateSecretResponse,
  type DispatchNodeTaskInput,
  type EnrolledNode,
  type FederatedTask,
  type RuntimeMatrix,
  type GateConnectorsResponse,
  type GateSecretResponse,
  type GateSecretsResponse,
  type NodeEnrollTokenResponse,
  type NodesResponse,
  type SetVaultSecretInput,
  type WorkflowDef,
  type WorkflowRun,
  type WorkflowRunDetailResponse,
  type WorktreeDiff,
  type WorktreeSummary,
  type CreateRecordRequest,
  type CreateRecordResponse,
  type MoveRecordResponse,
  type ProjectRecordItem,
  type RecordState,
  type RecordsListResponse,
} from "../types/api";
import { canAttachTokenTo, isSafeLinkUrl } from "../lib/safeUrl";
import { getActiveTimeZone, normalizeTimeZone } from "../lib/datetime";

export function normalizeHubSettings(
  raw: unknown,
  fallback: HubSettings = DEFAULT_HUB_SETTINGS,
): HubSettings {
  const obj =
    raw && typeof raw === "object"
      ? ("settings" in (raw as Record<string, unknown>) &&
        (raw as Record<string, unknown>).settings &&
        typeof (raw as Record<string, unknown>).settings === "object"
          ? ((raw as Record<string, unknown>).settings as Record<string, unknown>)
          : (raw as Record<string, unknown>))
      : {};

  const themeRaw = obj.theme_preference;
  const theme_preference: HubSettings["theme_preference"] =
    themeRaw === "dark" || themeRaw === "light" || themeRaw === "system"
      ? themeRaw
      : fallback.theme_preference;

  const timezone = normalizeTimeZone(obj.timezone, fallback.timezone || "UTC");

  const retentionRaw = obj.attachment_retention_days;
  const attachment_retention_days =
    typeof retentionRaw === "number" &&
    Number.isFinite(retentionRaw) &&
    retentionRaw >= 0 &&
    retentionRaw <= 365
      ? Math.round(retentionRaw)
      : (fallback.attachment_retention_days ?? 7);

  const subRaw =
    obj.subsystems && typeof obj.subsystems === "object"
      ? (obj.subsystems as Record<string, unknown>)
      : {};
  const notifRaw =
    obj.notifications && typeof obj.notifications === "object"
      ? (obj.notifications as Record<string, unknown>)
      : {};
  const connRaw =
    obj.hub_connection && typeof obj.hub_connection === "object"
      ? (obj.hub_connection as Record<string, unknown>)
      : {};

  return {
    theme_preference,
    timezone,
    attachment_retention_days,
    subsystems: {
      ferrogate_enabled:
        typeof subRaw.ferrogate_enabled === "boolean"
          ? subRaw.ferrogate_enabled
          : fallback.subsystems.ferrogate_enabled,
      ferrostate_enabled:
        typeof subRaw.ferrostate_enabled === "boolean"
          ? subRaw.ferrostate_enabled
          : fallback.subsystems.ferrostate_enabled,
      ferrostep_enabled:
        typeof subRaw.ferrostep_enabled === "boolean"
          ? subRaw.ferrostep_enabled
          : fallback.subsystems.ferrostep_enabled,
    },
    notifications: {
      notify_on_pr_review:
        typeof notifRaw.notify_on_pr_review === "boolean"
          ? notifRaw.notify_on_pr_review
          : fallback.notifications.notify_on_pr_review,
      notify_on_session_error:
        typeof notifRaw.notify_on_session_error === "boolean"
          ? notifRaw.notify_on_session_error
          : fallback.notifications.notify_on_session_error,
      notify_on_wire_urgent:
        typeof notifRaw.notify_on_wire_urgent === "boolean"
          ? notifRaw.notify_on_wire_urgent
          : fallback.notifications.notify_on_wire_urgent,
      sound_enabled:
        typeof notifRaw.sound_enabled === "boolean"
          ? notifRaw.sound_enabled
          : fallback.notifications.sound_enabled,
    },
    hub_connection: {
      auto_reconnect_ws:
        typeof connRaw.auto_reconnect_ws === "boolean"
          ? connRaw.auto_reconnect_ws
          : fallback.hub_connection.auto_reconnect_ws,
      poll_interval_secs:
        typeof connRaw.poll_interval_secs === "number" &&
        Number.isFinite(connRaw.poll_interval_secs) &&
        connRaw.poll_interval_secs > 0
          ? connRaw.poll_interval_secs
          : fallback.hub_connection.poll_interval_secs,
    },
  };
}

export function normalizeCloudAdaptersConfig(
  raw: unknown,
  fallback: CloudAdaptersConfig = DEFAULT_CLOUD_ADAPTERS_CONFIG,
): CloudAdaptersConfig {
  const obj =
    raw && typeof raw === "object"
      ? ("adapters" in (raw as Record<string, unknown>) &&
        (raw as Record<string, unknown>).adapters &&
        typeof (raw as Record<string, unknown>).adapters === "object"
          ? ((raw as Record<string, unknown>).adapters as Record<string, unknown>)
          : (raw as Record<string, unknown>))
      : {};

  const storeRaw =
    obj.store_sync && typeof obj.store_sync === "object"
      ? (obj.store_sync as Record<string, unknown>)
      : {};
  const gateRaw =
    obj.gate_cloud && typeof obj.gate_cloud === "object"
      ? (obj.gate_cloud as Record<string, unknown>)
      : {};

  const provider: CloudAdaptersConfig["store_sync"]["provider"] =
    storeRaw.provider === "none" ||
    storeRaw.provider === "gcp" ||
    storeRaw.provider === "aws" ||
    storeRaw.provider === "azure"
      ? storeRaw.provider
      : fallback.store_sync.provider;

  const sync_mode: CloudAdaptersConfig["store_sync"]["sync_mode"] =
    storeRaw.sync_mode === "write_through" ||
    storeRaw.sync_mode === "write_behind" ||
    storeRaw.sync_mode === "local_cache" ||
    storeRaw.sync_mode === "cloud_pre_cache"
      ? storeRaw.sync_mode
      : fallback.store_sync.sync_mode;

  const registry_provider: CloudAdaptersConfig["gate_cloud"]["registry_provider"] =
    gateRaw.registry_provider === "none" ||
    gateRaw.registry_provider === "gcp" ||
    gateRaw.registry_provider === "aws" ||
    gateRaw.registry_provider === "azure"
      ? gateRaw.registry_provider
      : fallback.gate_cloud.registry_provider;

  return {
    store_sync: {
      provider,
      sync_mode,
      endpoint_url:
        typeof storeRaw.endpoint_url === "string"
          ? storeRaw.endpoint_url
          : fallback.store_sync.endpoint_url,
      enabled:
        typeof storeRaw.enabled === "boolean"
          ? storeRaw.enabled
          : fallback.store_sync.enabled,
    },
    gate_cloud: {
      registry_provider,
      relay_endpoint_url:
        typeof gateRaw.relay_endpoint_url === "string"
          ? gateRaw.relay_endpoint_url
          : fallback.gate_cloud.relay_endpoint_url,
      secret_manager_enabled:
        typeof gateRaw.secret_manager_enabled === "boolean"
          ? gateRaw.secret_manager_enabled
          : fallback.gate_cloud.secret_manager_enabled,
      model_armor_enabled:
        typeof gateRaw.model_armor_enabled === "boolean"
          ? gateRaw.model_armor_enabled
          : fallback.gate_cloud.model_armor_enabled,
    },
  };
}

export const DEFAULT_TOKEN_STORAGE_KEY = "fl_hub_session_token";
export const PASSKEY_KEYSTORE_STORAGE_KEY = "fl_hub_passkey_keystore";
export const LAST_PASSKEY_ID_STORAGE_KEY = "fl_hub_last_passkey_credential_id";
export const PASSKEY_IDB_DATABASE_NAME = "fl_hub_passkey_idb";
export const PASSKEY_IDB_STORE_NAME = "keys";

export interface StoredClientPasskey {
  credential_id: string;
  label?: string;
  public_key_b64: string;
  private_jwk?: JsonWebKey;
  sign_count: number;
  created_at_ms?: number;
}

const inMemoryPasskeyStore = new Map<string, StoredClientPasskey>();
const inMemoryPasskeyCryptoKeys = new Map<string, CryptoKey>();
let inMemoryLastPasskeyId: string | null = null;

function hasIndexedDbSupport(): boolean {
  return (
    typeof globalThis.indexedDB !== "undefined" &&
    globalThis.indexedDB !== null &&
    typeof globalThis.indexedDB.open === "function"
  );
}

function openPasskeyIdb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    try {
      const req = globalThis.indexedDB.open(PASSKEY_IDB_DATABASE_NAME, 1);
      req.onupgradeneeded = () => {
        const db = req.result;
        if (!db.objectStoreNames.contains(PASSKEY_IDB_STORE_NAME)) {
          db.createObjectStore(PASSKEY_IDB_STORE_NAME);
        }
      };
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error ?? new Error("Failed to open passkey IndexedDB"));
    } catch (err) {
      reject(err);
    }
  });
}

async function putPasskeyInIdb(
  credentialId: string,
  privateKey: CryptoKey,
): Promise<boolean> {
  inMemoryPasskeyCryptoKeys.set(credentialId, privateKey);
  if (!hasIndexedDbSupport()) {
    return false;
  }
  let db: IDBDatabase | null = null;
  try {
    db = await openPasskeyIdb();
    await new Promise<void>((resolve, reject) => {
      const tx = db!.transaction(PASSKEY_IDB_STORE_NAME, "readwrite");
      const store = tx.objectStore(PASSKEY_IDB_STORE_NAME);
      const putReq =
        store.keyPath === "credential_id"
          ? store.put({ credential_id: credentialId, privateKey })
          : store.put(privateKey, credentialId);
      putReq.onsuccess = () => resolve();
      putReq.onerror = () =>
        reject(putReq.error ?? new Error("Failed to store passkey in IndexedDB"));
    });
    return true;
  } catch {
    return false;
  } finally {
    try {
      db?.close();
    } catch {
      // Ignore close errors
    }
  }
}

async function getPasskeyFromIdb(credentialId: string): Promise<CryptoKey | null> {
  if (!hasIndexedDbSupport()) {
    return inMemoryPasskeyCryptoKeys.get(credentialId) ?? null;
  }
  let db: IDBDatabase | null = null;
  try {
    db = await openPasskeyIdb();
    const raw = await new Promise<unknown>((resolve, reject) => {
      const tx = db!.transaction(PASSKEY_IDB_STORE_NAME, "readonly");
      const store = tx.objectStore(PASSKEY_IDB_STORE_NAME);
      const getReq = store.get(credentialId);
      getReq.onsuccess = () => resolve(getReq.result);
      getReq.onerror = () =>
        reject(getReq.error ?? new Error("Failed to read passkey from IndexedDB"));
    });
    if (!raw || typeof raw !== "object") {
      return inMemoryPasskeyCryptoKeys.get(credentialId) ?? null;
    }
    if (
      "type" in (raw as CryptoKey) &&
      typeof (raw as CryptoKey).type === "string" &&
      "extractable" in (raw as CryptoKey)
    ) {
      return raw as CryptoKey;
    }
    if (
      "privateKey" in (raw as Record<string, unknown>) &&
      (raw as { privateKey?: unknown }).privateKey &&
      typeof (raw as { privateKey?: unknown }).privateKey === "object"
    ) {
      return (raw as { privateKey: CryptoKey }).privateKey;
    }
    return inMemoryPasskeyCryptoKeys.get(credentialId) ?? null;
  } catch {
    return inMemoryPasskeyCryptoKeys.get(credentialId) ?? null;
  } finally {
    try {
      db?.close();
    } catch {
      // Ignore close errors
    }
  }
}

export function bytesToBase64Url(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i++) {
    binary += String.fromCharCode(bytes[i]);
  }
  return btoa(binary)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/g, "");
}

export function base64UrlToBytes(b64: string): Uint8Array {
  const trimmed = b64.trim().replace(/-/g, "+").replace(/_/g, "/");
  const padLen = (4 - (trimmed.length % 4)) % 4;
  const padded = trimmed + "=".repeat(padLen);
  const binary = atob(padded);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    out[i] = binary.charCodeAt(i);
  }
  return out;
}

function encodeDerInteger(raw: Uint8Array): Uint8Array {
  let start = 0;
  while (start < raw.length - 1 && raw[start] === 0) {
    start++;
  }
  const trimmed = raw.subarray(start);
  const needsLeadingZero = (trimmed[0] & 0x80) !== 0;
  const bodyLen = trimmed.length + (needsLeadingZero ? 1 : 0);
  const out = new Uint8Array(2 + bodyLen);
  out[0] = 0x02;
  out[1] = bodyLen;
  if (needsLeadingZero) {
    out[2] = 0x00;
    out.set(trimmed, 3);
  } else {
    out.set(trimmed, 2);
  }
  return out;
}

/**
 * Converts a 64-byte WebCrypto IEEE P1363 ECDSA P-256 signature (`r || s`)
 * into an ASN.1 DER-encoded ECDSA signature (`0x30 len 0x02 rlen r 0x02 slen s`).
 */
export function p1363ToDer(sig: Uint8Array): Uint8Array {
  if (sig.length !== 64) {
    return sig;
  }
  const rDer = encodeDerInteger(sig.subarray(0, 32));
  const sDer = encodeDerInteger(sig.subarray(32, 64));
  const seqLen = rDer.length + sDer.length;
  const out = new Uint8Array(2 + seqLen);
  out[0] = 0x30;
  out[1] = seqLen;
  out.set(rDer, 2);
  out.set(sDer, 2 + rDer.length);
  return out;
}

function readStoredPasskeyMap(): Record<string, StoredClientPasskey> {
  // No prototype: a credential id (or a key in a tampered keystore) such as `__proto__` or
  // `constructor` is then just a name, never a lookup that walks into `Object.prototype`.
  const merged: Record<string, StoredClientPasskey> = Object.create(null);
  for (const [k, v] of inMemoryPasskeyStore.entries()) {
    merged[k] = v;
  }
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      const raw = window.localStorage.getItem(PASSKEY_KEYSTORE_STORAGE_KEY);
      if (raw) {
        const parsed: unknown = JSON.parse(raw);
        if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
          for (const [k, v] of Object.entries(parsed)) {
            // Own entries only, and only ones shaped like a stored key (either public metadata
            // backed by an IndexedDB CryptoKey, or legacy/fallback JWK entries).
            if (k !== "__proto__" && v && typeof v === "object") {
              const candidate = v as StoredClientPasskey;
              const hasValidJwk =
                typeof candidate.private_jwk === "object" &&
                candidate.private_jwk !== null;
              const hasPublicMetadata =
                candidate.private_jwk === undefined &&
                typeof candidate.public_key_b64 === "string" &&
                candidate.public_key_b64.length > 0 &&
                typeof candidate.sign_count === "number";
              if (hasValidJwk || hasPublicMetadata) {
                // The map is keyed by credential id, so an entry that does not carry its own id
                // (or carries a non-string) takes the key it is filed under. Without this the
                // login would send no `credential_id` and `saveStoredPasskey` would file the
                // entry under "undefined".
                merged[k] = {
                  ...candidate,
                  credential_id:
                    typeof candidate.credential_id === "string" &&
                    candidate.credential_id
                      ? candidate.credential_id
                      : k,
                };
              }
            }
          }
        }
      }
    }
  } catch {
    // Ignore storage errors
  }
  return merged;
}

function saveStoredPasskey(entry: StoredClientPasskey): void {
  inMemoryPasskeyStore.set(entry.credential_id, entry);
  inMemoryLastPasskeyId = entry.credential_id;
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      const current = readStoredPasskeyMap();
      current[entry.credential_id] = entry;
      window.localStorage.setItem(
        PASSKEY_KEYSTORE_STORAGE_KEY,
        JSON.stringify(current),
      );
      window.localStorage.setItem(
        LAST_PASSKEY_ID_STORAGE_KEY,
        entry.credential_id,
      );
    }
  } catch {
    // Ignore storage errors
  }
}

export function getLastRegisteredPasskeyId(): string {
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      const stored = window.localStorage.getItem(LAST_PASSKEY_ID_STORAGE_KEY);
      if (stored && stored.trim().length >= 16) {
        return stored.trim();
      }
    }
  } catch {
    // Ignore storage errors
  }
  if (inMemoryLastPasskeyId && inMemoryLastPasskeyId.length >= 16) {
    return inMemoryLastPasskeyId;
  }
  return "passkey-default-0001";
}

/**
 * A fresh, unguessable credential id: 24 random bytes as base64url (32 characters), inside the
 * hub's 16 to 1024 character `[A-Za-z0-9_-]` rule. A clock-based id repeats when two clicks land
 * in one millisecond and can be guessed by anyone who knows roughly when it was made. Throws when
 * the platform has no secure random generator.
 */
export function generatePasskeyCredentialId(): string {
  const crypto = globalThis.crypto;
  if (crypto && typeof crypto.getRandomValues === "function") {
    return bytesToBase64Url(crypto.getRandomValues(new Uint8Array(24)));
  }
  // No clock-and-Math.random fallback: that id is exactly the guessable one this function
  // exists to avoid. The caller shows the error and no passkey is registered.
  throw new Error(
    "Cannot register a passkey: a secure random generator is unavailable in this browser.",
  );
}

/**
 * The origin and relying-party id a passkey is registered and asserted under. One function for
 * both so they cannot drift: the hub pins the registered origin and checks `clientData.origin`
 * against it byte for byte. The page's own origin wins, as a browser would put it in
 * `clientDataJSON`; `baseUrl` is the fallback when there is no page (a worker, a test).
 */
export function resolvePasskeyOrigin(baseUrl: string): {
  origin: string;
  hostname: string;
} {
  let origin = "http://localhost:9000";
  let hostname = "localhost";
  if (
    typeof window !== "undefined" &&
    window.location?.origin &&
    window.location.origin !== "null"
  ) {
    origin = window.location.origin;
    hostname = window.location.hostname || "localhost";
  } else if (baseUrl) {
    try {
      const parsedUrl = new URL(baseUrl);
      origin = parsedUrl.origin;
      hostname = parsedUrl.hostname || "localhost";
    } catch {
      // Keep default localhost origin
    }
  }
  return { origin, hostname };
}

export class HubApiError extends Error {
  public readonly status: number;
  public readonly detail?: string;
  public readonly requires_totp?: boolean;
  public readonly details?: Record<string, unknown>;

  constructor(
    status: number,
    message: string,
    detail?: string,
    requiresTotp?: boolean,
    details?: Record<string, unknown> | null,
  ) {
    super(message);
    this.name = "HubApiError";
    this.status = status;
    this.detail = detail;
    if (requiresTotp !== undefined) {
      this.requires_totp = requiresTotp;
    }
    if (details && typeof details === "object") {
      this.details = details;
    }
  }
}

export interface HubApiClientOptions {
  baseUrl?: string;
  fetchImpl?: typeof fetch;
  storageKey?: string;
}

export class HubApiClient {
  private readonly baseUrl: string;
  private readonly fetchImpl: typeof fetch;
  private readonly storageKey: string;
  private memoryToken: string | null = null;

  constructor(options: HubApiClientOptions = {}) {
    this.baseUrl = (options.baseUrl ?? "").replace(/\/+$/, "");
    this.fetchImpl = options.fetchImpl ?? ((...args) => globalThis.fetch(...args));
    this.storageKey = options.storageKey ?? DEFAULT_TOKEN_STORAGE_KEY;
    this.memoryToken = this.readStoredToken();
  }

  private readStoredToken(): string | null {
    try {
      if (typeof window !== "undefined" && window.localStorage) {
        const val = window.localStorage.getItem(this.storageKey);
        return val && val.trim().length > 0 ? val.trim() : null;
      }
    } catch {
      // Ignore storage access errors in private/sandboxed contexts.
    }
    return null;
  }

  /** The hub origin this client talks to ("" means same-origin). */
  public getBaseUrl(): string {
    return this.baseUrl;
  }

  public getToken(): string | null {
    return this.memoryToken ?? this.readStoredToken();
  }

  public setToken(token: string | null): void {
    const clean = token && token.trim().length > 0 ? token.trim() : null;
    this.memoryToken = clean;
    try {
      if (typeof window !== "undefined" && window.localStorage) {
        if (clean) {
          window.localStorage.setItem(this.storageKey, clean);
        } else {
          window.localStorage.removeItem(this.storageKey);
        }
      }
    } catch {
      // Ignore storage errors.
    }
  }

  public async request<T>(path: string, init: RequestInit = {}): Promise<T> {
    // One leading slash, always: `//host/x` or `/\\host/x` would be read as a URL for another
    // host, and this request carries the session token.
    const rel = (path.startsWith("/") ? path : `/${path}`).replace(/^[/\\]+/, "/");
    const url = `${this.baseUrl}${rel}`;
    const headers = new Headers(init.headers ?? {});
    if (!headers.has("Accept")) {
      headers.set("Accept", "application/json");
    }
    if (init.body !== undefined && !headers.has("Content-Type")) {
      headers.set("Content-Type", "application/json");
    }
    const tok = this.getToken();
    if (tok && !headers.has("Authorization")) {
      headers.set("Authorization", `Bearer ${tok}`);
    }

    const response = await this.fetchImpl(url, {
      ...init,
      headers,
      credentials: init.credentials ?? "same-origin",
    });

    const rawText = await response.text();
    let parsed: Record<string, unknown> | null = null;
    if (rawText.trim().length > 0) {
      try {
        parsed = JSON.parse(rawText) as Record<string, unknown>;
      } catch {
        parsed = null;
      }
    }

    if (!response.ok) {
      const errMsg =
        (typeof parsed?.detail === "string" && parsed.detail) ||
        (typeof parsed?.error === "string" && parsed.error) ||
        `HTTP ${response.status}`;
      const detail = typeof parsed?.detail === "string" ? parsed.detail : undefined;
      const requiresTotp =
        typeof parsed?.requires_totp === "boolean"
          ? parsed.requires_totp
          : undefined;
      throw new HubApiError(response.status, errMsg, detail, requiresTotp, parsed);
    }

    return (parsed ?? ({} as unknown)) as T;
  }

  public async getHealth(): Promise<HealthResponse> {
    return this.request<HealthResponse>("/api/v1/health");
  }

  public async pair(payload: { code?: string; token?: string }): Promise<AuthSessionResponse> {
    const res = await this.request<AuthSessionResponse>("/api/v1/auth/pair", {
      method: "POST",
      body: JSON.stringify(payload),
    });
    if (res.session_token) {
      this.setToken(res.session_token);
    } else if (payload.token && res.authenticated) {
      this.setToken(payload.token);
    }
    return res;
  }

  public async getSession(): Promise<AuthSessionResponse> {
    return this.request<AuthSessionResponse>("/api/v1/auth/session");
  }

  public async getAuthSession(): Promise<AuthSessionResponse> {
    return this.getSession();
  }

  public async logout(): Promise<{ authenticated: boolean }> {
    try {
      return await this.request<{ authenticated: boolean }>("/api/v1/auth/logout", {
        method: "POST",
      });
    } finally {
      this.setToken(null);
    }
  }

  public async getAgents(params?: {
    project?: string;
    include_retired?: boolean;
  }): Promise<AgentsResponse> {
    const qs = new URLSearchParams();
    if (params?.project) {
      qs.set("project", params.project);
    }
    if (params?.include_retired) {
      qs.set("include_retired", "true");
    }
    const suffix = qs.toString() ? `?${qs.toString()}` : "";
    return this.request<AgentsResponse>(`/api/v1/agents${suffix}`);
  }

  public async getLiveness(staleAfterMs?: number): Promise<LivenessResponse> {
    const suffix =
      staleAfterMs !== undefined ? `?stale_after_ms=${encodeURIComponent(String(staleAfterMs))}` : "";
    return this.request<LivenessResponse>(`/api/v1/agents/liveness${suffix}`);
  }

  public async getAgentsLiveness(staleAfterMs?: number): Promise<LivenessResponse> {
    return this.getLiveness(staleAfterMs);
  }

  public async recastAgent(
    nameKey: string,
    payload: RecastAgentRequest,
  ): Promise<RecastAgentResponse> {
    return this.request<RecastAgentResponse>(
      `/api/v1/agents/${encodeURIComponent(nameKey.trim().toLowerCase())}/recast`,
      {
        method: "POST",
        body: JSON.stringify(payload),
      },
    );
  }

  public async getSessions(): Promise<{ sessions: DiscoveredSessionSummary[] }> {
    return this.request<{ sessions: DiscoveredSessionSummary[] }>("/api/v1/sessions");
  }

  public async getSessionTranscript(sessionId: string): Promise<SessionTranscriptResponse> {
    return this.request<SessionTranscriptResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/transcript`,
    );
  }

  public async getSessionQueue(sessionId: string): Promise<SessionQueueResponse> {
    return this.request<SessionQueueResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/queue`,
    );
  }

  public async clearSession(
    sessionId: string,
    mode: ClearSessionMode,
  ): Promise<ClearSessionResponse> {
    return this.request<ClearSessionResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/clear`,
      {
        method: "POST",
        body: JSON.stringify({ mode }),
      },
    );
  }

  public async spinUpSession(
    req: SpinUpSessionRequest,
  ): Promise<SpinUpSessionResponse> {
    return this.request<SpinUpSessionResponse>("/api/v1/sessions/spin-up", {
      method: "POST",
      body: JSON.stringify(req),
    });
  }

  public async decommissionSession(
    sessionId: string,
    mode: DecommissionMode,
  ): Promise<DecommissionSessionResponse> {
    return this.request<DecommissionSessionResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/decommission`,
      {
        method: "POST",
        body: JSON.stringify({ mode }),
      },
    );
  }

  public async interruptSession(
    sessionId: string,
  ): Promise<InterruptSessionResponse> {
    return this.request<InterruptSessionResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/interrupt`,
      {
        method: "POST",
        body: JSON.stringify({}),
      },
    );
  }

  public async setSessionModel(
    sessionId: string,
    model: string,
    thinkingLevel?: string | null,
  ): Promise<SetSessionModelResponse> {
    return this.request<SetSessionModelResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/model`,
      {
        method: "POST",
        body: JSON.stringify({
          model,
          ...(thinkingLevel ? { thinking_level: thinkingLevel } : {}),
        }),
      },
    );
  }

  public async bulkSessionAction(
    action: BulkSessionAction,
  ): Promise<BulkSessionActionResponse> {
    return this.request<BulkSessionActionResponse>(
      "/api/v1/sessions/bulk-action",
      {
        method: "POST",
        body: JSON.stringify({ action }),
      },
    );
  }

  public async listSlashCommands(): Promise<{ commands: SlashCommandEntry[] }> {
    try {
      const data = await this.request<{ commands?: SlashCommandEntry[] }>(
        "/api/v1/commands",
      );
      if (Array.isArray(data.commands) && data.commands.length > 0) {
        return { commands: data.commands };
      }
      return { commands: DEFAULT_SLASH_COMMANDS };
    } catch {
      return { commands: DEFAULT_SLASH_COMMANDS };
    }
  }

  public async sendSessionPrompt(
    sessionId: string,
    payloadOrPrompt: PromptRequest | string,
    attachments?: PromptAttachment[],
    sendNow?: boolean,
  ): Promise<PromptResponse> {
    const body: PromptRequest =
      typeof payloadOrPrompt === "string"
        ? {
            prompt: payloadOrPrompt,
            ...(attachments && attachments.length > 0 ? { attachments } : {}),
            send_now: Boolean(sendNow),
          }
        : {
            ...payloadOrPrompt,
            send_now: Boolean(payloadOrPrompt.send_now ?? sendNow),
          };
    return this.request<PromptResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/prompt`,
      {
        method: "POST",
        body: JSON.stringify(body),
      },
    );
  }

  public async sendQueuedPromptNow(
    sessionId: string,
    promptId: string,
  ): Promise<PromptResponse> {
    return this.request<PromptResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/queue/${encodeURIComponent(promptId)}/send-now`,
      {
        method: "POST",
        body: JSON.stringify({}),
      },
    );
  }

  public async cancelQueuedPrompt(
    sessionId: string,
    promptId: string,
  ): Promise<{
    session_id: string;
    status: string;
    prompt_id: string;
    is_busy?: boolean;
    queue: QueuedOperatorPrompt[];
  }> {
    return this.request(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/queue/${encodeURIComponent(promptId)}`,
      {
        method: "DELETE",
      },
    );
  }

  public async updateQueuedPrompt(
    sessionId: string,
    promptId: string,
    prompt: string,
  ): Promise<{
    session_id: string;
    status: string;
    prompt_id: string;
    queued_prompt: QueuedOperatorPrompt;
    is_busy?: boolean;
    queue: QueuedOperatorPrompt[];
  }> {
    return this.request(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/queue/${encodeURIComponent(promptId)}`,
      {
        method: "PATCH",
        body: JSON.stringify({ prompt }),
      },
    );
  }

  public async getWorktrees(): Promise<{ worktrees: WorktreeSummary[] }> {
    return this.request<{ worktrees: WorktreeSummary[] }>("/api/v1/worktrees");
  }

  public async getWorktreeDiff(worktreeId: string): Promise<WorktreeDiff> {
    return this.request<WorktreeDiff>(
      `/api/v1/worktrees/${encodeURIComponent(worktreeId)}/diff`,
    );
  }

  public async getPullRequests(): Promise<{ prs: PullRequestItem[] }> {
    return this.request<{ prs: PullRequestItem[] }>("/api/v1/prs");
  }

  public async getPrs(): Promise<{ prs: PullRequestItem[] }> {
    return this.getPullRequests();
  }

  public async reviewPullRequest(
    number: number,
    payload: PrReviewRequest,
  ): Promise<PrReviewResponse> {
    return this.request<PrReviewResponse>(`/api/v1/prs/${encodeURIComponent(String(number))}/review`, {
      method: "POST",
      body: JSON.stringify(payload),
    });
  }

  public async mergePullRequest(
    number: number,
    payload: PrMergeRequest = {},
  ): Promise<PrMergeResponse> {
    return this.request<PrMergeResponse>(`/api/v1/prs/${encodeURIComponent(String(number))}/merge`, {
      method: "POST",
      body: JSON.stringify(payload),
    });
  }

  public async getWireMessages(params?: {
    to?: string;
    unread?: boolean;
    thread?: string;
  }): Promise<{ messages: WireInboxItem[] }> {
    const qs = new URLSearchParams();
    if (params?.to) {
      qs.set("to", params.to);
    }
    if (params?.unread) {
      qs.set("unread", "true");
    }
    if (params?.thread) {
      qs.set("thread", params.thread);
    }
    const suffix = qs.toString() ? `?${qs.toString()}` : "";
    return this.request<{ messages: WireInboxItem[] }>(`/api/v1/wire/messages${suffix}`);
  }

  public async sendWireMessage(payload: WireSendRequest): Promise<WireSendResponse> {
    return this.request<WireSendResponse>("/api/v1/wire/send", {
      method: "POST",
      body: JSON.stringify(payload),
    });
  }

  public async ackWireMessage(payload: WireAckRequest): Promise<WireAckResponse> {
    return this.request<WireAckResponse>("/api/v1/wire/ack", {
      method: "POST",
      body: JSON.stringify(payload),
    });
  }

  public async getWorkflows(): Promise<{ workflows: WorkflowDef[] }> {
    return this.request<{ workflows: WorkflowDef[] }>("/api/v1/workflows");
  }

  public async getWorkflowRuns(): Promise<{ runs: WorkflowRun[] }> {
    return this.request<{ runs: WorkflowRun[] }>("/api/v1/workflow-runs");
  }

  public async startWorkflowRun(payload: {
    workflow: string;
    title?: string;
    by?: string;
  }): Promise<{ run: WorkflowRun }> {
    return this.request<{ run: WorkflowRun }>("/api/v1/workflow-runs", {
      method: "POST",
      body: JSON.stringify(payload),
    });
  }

  public async getWorkflowRun(runId: string): Promise<WorkflowRunDetailResponse> {
    return this.request<WorkflowRunDetailResponse>(
      `/api/v1/workflow-runs/${encodeURIComponent(runId)}`,
    );
  }

  public async stepWorkflowRun(
    runId: string,
    payload: {
      step_id?: string;
      adapter?: string;
      offer_only?: boolean;
      by?: string;
    } = {},
  ): Promise<{ step_id: string; action: unknown; run: WorkflowRun }> {
    return this.request<{ step_id: string; action: unknown; run: WorkflowRun }>(
      `/api/v1/workflow-runs/${encodeURIComponent(runId)}/step`,
      {
        method: "POST",
        body: JSON.stringify(payload),
      },
    );
  }

  public async approveWorkflowRun(
    runId: string,
    payload: { step_id?: string; by?: string; note?: string } = {},
  ): Promise<{ run: WorkflowRun }> {
    return this.request<{ run: WorkflowRun }>(
      `/api/v1/workflow-runs/${encodeURIComponent(runId)}/approve`,
      {
        method: "POST",
        body: JSON.stringify(payload),
      },
    );
  }

  public async rejectWorkflowRun(
    runId: string,
    payload: { step_id?: string; by?: string; reason?: string; note?: string } = {},
  ): Promise<{ run: WorkflowRun }> {
    return this.request<{ run: WorkflowRun }>(
      `/api/v1/workflow-runs/${encodeURIComponent(runId)}/reject`,
      {
        method: "POST",
        body: JSON.stringify(payload),
      },
    );
  }

  public async abortWorkflowRun(
    runId: string,
    payload: { by?: string } = {},
  ): Promise<{ run: WorkflowRun }> {
    return this.request<{ run: WorkflowRun }>(
      `/api/v1/workflow-runs/${encodeURIComponent(runId)}/abort`,
      {
        method: "POST",
        body: JSON.stringify(payload),
      },
    );
  }

  public async listStateEntries(params?: {
    scope?: string;
    concern?: string;
  }): Promise<StateEntriesResponse> {
    const qs = new URLSearchParams();
    if (params?.scope) {
      qs.set("scope", params.scope);
    }
    if (params?.concern) {
      qs.set("concern", params.concern);
    }
    const suffix = qs.toString() ? `?${qs.toString()}` : "";
    return this.request<StateEntriesResponse>(`/api/v1/state${suffix}`);
  }

  public async getState(params?: {
    scope?: string;
    concern?: string;
  }): Promise<{ entries: StateEntry[] }> {
    return this.listStateEntries(params);
  }

  public async upsertStateEntry(
    input: UpsertStateEntryInput,
  ): Promise<StateEntryResponse> {
    return this.request<StateEntryResponse>("/api/v1/state", {
      method: "POST",
      body: JSON.stringify(input),
    });
  }

  public async upsertState(payload: Record<string, unknown>): Promise<{ entry: StateEntry }> {
    return this.upsertStateEntry(payload as unknown as UpsertStateEntryInput);
  }

  public async searchStateEntries(params: {
    q: string;
    scope?: string;
    concern?: string;
    limit?: number;
  }): Promise<StateSearchResponse> {
    const qs = new URLSearchParams();
    qs.set("q", params.q);
    if (params.scope) {
      qs.set("scope", params.scope);
    }
    if (params.concern) {
      qs.set("concern", params.concern);
    }
    if (params.limit !== undefined) {
      qs.set("limit", String(params.limit));
    }
    return this.request<StateSearchResponse>(`/api/v1/state/search?${qs.toString()}`);
  }

  public async searchCodeIndex(params: {
    q: string;
    kind?: CodeSearchKind;
    limit?: number;
  }): Promise<CodeSearchResponse> {
    const qs = new URLSearchParams();
    qs.set("q", params.q);
    if (params.kind) {
      qs.set("kind", params.kind);
    }
    if (params.limit !== undefined) {
      qs.set("limit", String(params.limit));
    }
    return this.request<CodeSearchResponse>(`/api/v1/state/code-search?${qs.toString()}`);
  }

  public async createStateCheckpoint(
    input: { scope?: string; slug?: string; summary?: string } = {},
  ): Promise<StateCheckpointResponse> {
    return this.request<StateCheckpointResponse>("/api/v1/state/checkpoint", {
      method: "POST",
      body: JSON.stringify(input),
    });
  }

  /** Owner-only on the hub; a peer session gets `HubApiError` 403. */
  public async importWorkspaceState(): Promise<ImportWorkspaceStateResponse> {
    return this.request<ImportWorkspaceStateResponse>("/api/v1/state/import-workspace", {
      method: "POST",
      body: JSON.stringify({}),
    });
  }

  public async getStats(): Promise<HubStatsResponse> {
    return this.request<HubStatsResponse>("/api/v1/stats");
  }

  public async getArchitecture(path?: string): Promise<ArchitectureResponse> {
    const suffix = path ? `?path=${encodeURIComponent(path)}` : "";
    return this.request<ArchitectureResponse>(`/api/v1/architecture${suffix}`);
  }

  /** Metadata only; the hub never returns plaintext secret values. */
  public async listGateSecrets(): Promise<GateSecretsResponse> {
    return this.request<GateSecretsResponse>("/api/v1/gate/secrets");
  }

  /** Owner-only on the hub; a peer session gets `HubApiError` 403. */
  public async setGateSecret(input: SetVaultSecretInput): Promise<GateSecretResponse> {
    return this.request<GateSecretResponse>("/api/v1/gate/secrets", {
      method: "POST",
      body: JSON.stringify(input),
    });
  }

  /** Owner-only on the hub; a peer session gets `HubApiError` 403. */
  public async deleteGateSecret(name: string): Promise<DeleteGateSecretResponse> {
    return this.request<DeleteGateSecretResponse>(
      `/api/v1/gate/secrets/${encodeURIComponent(name)}`,
      { method: "DELETE" },
    );
  }

  public async listGateConnectors(): Promise<GateConnectorsResponse> {
    return this.request<GateConnectorsResponse>("/api/v1/gate/connectors");
  }

  public async getArtifactPreview(path: string): Promise<ArtifactPreviewResponse> {
    const qs = new URLSearchParams({ path, mode: "json" });
    return this.request<ArtifactPreviewResponse>(`/api/v1/artifacts/preview?${qs.toString()}`);
  }

  /** Recorded content revisions of an artifact, oldest first. */
  public async getArtifactVersions(path: string): Promise<ArtifactVersionsResponse> {
    const qs = new URLSearchParams({ path });
    return this.request<ArtifactVersionsResponse>(
      `/api/v1/artifacts/versions?${qs.toString()}`,
    );
  }

  /** The stored text of one revision of an artifact. */
  public async getArtifactVersionContent(path: string, version: number): Promise<string> {
    const qs = new URLSearchParams({ path, version: String(version) });
    const headers = new Headers();
    const tok = this.getToken();
    if (tok) {
      headers.set("Authorization", `Bearer ${tok}`);
    }
    const response = await this.fetchImpl(
      `${this.baseUrl}/api/v1/artifacts/version-raw?${qs.toString()}`,
      { headers, credentials: "same-origin" },
    );
    const text = await response.text();
    if (!response.ok) {
      let message = `HTTP ${response.status}`;
      try {
        const parsed = JSON.parse(text) as { error?: unknown };
        if (typeof parsed.error === "string" && parsed.error) message = parsed.error;
      } catch {
        // Not JSON: keep the status line.
      }
      throw new HubApiError(response.status, message);
    }
    return text;
  }

  /** URL of the stored bytes of one revision, with the session token for `<img>` and links. */
  public getArtifactVersionRawUrl(path: string, version: number): string {
    const qs = new URLSearchParams({ path, version: String(version) });
    const base = `${this.baseUrl}/api/v1/artifacts/version-raw?${qs.toString()}`;
    const tok = this.getToken();
    if (tok && isSafeLinkUrl(base) && canAttachTokenTo(base, this.baseUrl)) {
      qs.set("token", tok);
      return `${this.baseUrl}/api/v1/artifacts/version-raw?${qs.toString()}`;
    }
    return base;
  }

  /** Structured preview of a .docx, .xlsx or .pptx artifact. */
  public async getOfficePreview(path: string): Promise<OfficePreviewResponse> {
    const qs = new URLSearchParams({ path });
    return this.request<OfficePreviewResponse>(
      `/api/v1/artifacts/office-preview?${qs.toString()}`,
    );
  }

  public async listSessionArtifacts(sessionId?: string | null): Promise<ArtifactListResponse> {
    const qs = new URLSearchParams();
    if (sessionId && sessionId.trim().length > 0) {
      qs.set("session_id", sessionId.trim());
    }
    const suffix = qs.toString() ? `?${qs.toString()}` : "";
    return this.request<ArtifactListResponse>(`/api/v1/artifacts/list${suffix}`);
  }

  public async pruneArtifacts(
    opts?: PruneArtifactsRequest,
  ): Promise<PruneArtifactsResponse> {
    return this.request<PruneArtifactsResponse>("/api/v1/artifacts/prune", {
      method: "POST",
      body: JSON.stringify(opts ?? {}),
    });
  }

  public async pruneAttachments(
    olderThanDays?: number,
  ): Promise<PruneAttachmentsResponse> {
    const body: PruneAttachmentsRequest =
      olderThanDays !== undefined ? { older_than_days: olderThanDays } : {};
    return this.request<PruneAttachmentsResponse>(
      "/api/v1/attachments/prune",
      {
        method: "POST",
        body: JSON.stringify(body),
      },
    );
  }

  public async getSettings(): Promise<HubSettings> {
    const raw = await this.request<unknown>("/api/v1/settings");
    return normalizeHubSettings(raw, {
      ...DEFAULT_HUB_SETTINGS,
      timezone: getActiveTimeZone(),
    });
  }

  public async updateSettings(
    patch: Partial<HubSettings>,
  ): Promise<HubSettings> {
    const mergedFallback = normalizeHubSettings(patch, DEFAULT_HUB_SETTINGS);
    const raw = await this.request<unknown>("/api/v1/settings", {
      method: "PUT",
      body: JSON.stringify(patch),
    });
    return normalizeHubSettings(raw, mergedFallback);
  }

  public getArtifactRawUrl(path: string, download = false): string {
    const qs = new URLSearchParams({
      path,
      mode: "raw",
      download: download ? "1" : "0",
    });
    const tok = this.getToken();
    if (tok) {
      qs.set("token", tok);
    }
    return `${this.baseUrl}/api/v1/artifacts/preview?${qs.toString()}`;
  }

  public async fetchNodes(): Promise<EnrolledNode[]> {
    try {
      const data = await this.request<NodesResponse>("/api/v1/nodes");
      return data.nodes ?? [];
    } catch (err) {
      if (err instanceof HubApiError && err.status === 404) {
        return [];
      }
      throw err;
    }
  }

  public async getNodes(): Promise<EnrolledNode[]> {
    return this.fetchNodes();
  }

  public async createNodeEnrollToken(
    label?: string,
    ttlSecs?: number,
  ): Promise<NodeEnrollTokenResponse> {
    return this.request<NodeEnrollTokenResponse>("/api/v1/nodes/enroll-token", {
      method: "POST",
      body: JSON.stringify({ label, ttl_secs: ttlSecs }),
    });
  }

  public async fetchRuntimeMatrix(): Promise<RuntimeMatrix> {
    return this.request<RuntimeMatrix>("/api/v1/runtimes/matrix");
  }

  /** Re-probe runtimes. `target` narrows to a tier, runtime, provider or engine. */
  public async probeRuntimes(target?: string): Promise<RuntimeMatrix> {
    return this.request<RuntimeMatrix>("/api/v1/runtimes/probe", {
      method: "POST",
      body: JSON.stringify(target ? { target } : {}),
    });
  }

  public async fetchNodeTasks(nodeId: string): Promise<FederatedTask[]> {
    const data = await this.request<{ tasks?: FederatedTask[] }>(
      `/api/v1/nodes/${encodeURIComponent(nodeId)}/tasks`,
    );
    return data.tasks ?? [];
  }

  public async dispatchNodeTask(
    nodeId: string,
    input: DispatchNodeTaskInput,
  ): Promise<FederatedTask> {
    const data = await this.request<{ task: FederatedTask }>(
      `/api/v1/nodes/${encodeURIComponent(nodeId)}/dispatch`,
      { method: "POST", body: JSON.stringify(input) },
    );
    return data.task;
  }

  public async revokeNode(nodeId: string): Promise<void> {
    await this.request<unknown>(`/api/v1/nodes/${encodeURIComponent(nodeId)}`, {
      method: "DELETE",
    });
  }

  public async listRecords(project?: string): Promise<RecordsListResponse> {
    const qs = new URLSearchParams();
    if (project && project.trim().length > 0) {
      qs.set("project", project.trim());
    }
    const suffix = qs.toString() ? `?${qs.toString()}` : "";
    return this.request<RecordsListResponse>(`/api/v1/records${suffix}`);
  }

  public async createRecord(
    req: CreateRecordRequest,
  ): Promise<{ record: ProjectRecordItem }> {
    return this.request<CreateRecordResponse>("/api/v1/records", {
      method: "POST",
      body: JSON.stringify(req),
    });
  }

  public async moveRecord(
    id: string,
    to: RecordState,
    project?: string,
  ): Promise<{ ok: boolean; outcome: string; record: ProjectRecordItem }> {
    const body: { to: RecordState; project?: string } = { to };
    if (project && project.trim().length > 0) {
      body.project = project.trim();
    }
    return this.request<MoveRecordResponse>(
      `/api/v1/records/${encodeURIComponent(id)}/move`,
      {
        method: "POST",
        body: JSON.stringify(body),
      },
    );
  }

  public async loginWithPassword(req: {
    username: string;
    password: string;
    totp_code?: string;
  }): Promise<{
    ok: boolean;
    authority?: HubAuthority;
    requires_totp?: boolean;
    session_token?: string;
  }> {
    const res = await this.request<{
      ok: boolean;
      authority?: HubAuthority;
      requires_totp?: boolean;
      session_token?: string;
    }>("/api/v1/auth/login", {
      method: "POST",
      body: JSON.stringify({ mode: "password", ...req }),
    });
    if (res?.session_token) {
      this.setToken(res.session_token);
    }
    return res;
  }

  public async requestPasskeyChallenge(): Promise<{
    challenge: string;
    expires_in_secs: number;
  }> {
    return this.request<{ challenge: string; expires_in_secs: number }>(
      "/api/v1/auth/passkey/challenge",
      {
        method: "POST",
        body: JSON.stringify({}),
      },
    );
  }

  public async loginWithPasskey(
    reqOrCredentialId?:
      | {
          credential_id?: string;
          totp_code?: string;
          assertion?: string;
          client_data_json_b64?: string;
          authenticator_data_b64?: string;
          signature_b64?: string;
        }
      | string,
    totpCode?: string,
  ): Promise<{
    ok: boolean;
    requires_totp?: boolean;
    authority?: HubAuthority;
    session_token?: string;
  }> {
    const reqObj =
      typeof reqOrCredentialId === "string"
        ? { credential_id: reqOrCredentialId }
        : (reqOrCredentialId ?? {});
    const resolvedCredentialId =
      typeof reqObj.credential_id === "string" && reqObj.credential_id.trim()
        ? reqObj.credential_id.trim()
        : getLastRegisteredPasskeyId();
    const resolvedTotp = (totpCode ?? reqObj.totp_code ?? "").trim();

    let payload: Record<string, unknown> = {
      mode: "passkey",
      ...reqObj,
      credential_id: resolvedCredentialId,
      ...(resolvedTotp ? { totp_code: resolvedTotp } : {}),
    };

    if (
      (!reqObj.signature_b64 ||
        !reqObj.client_data_json_b64 ||
        !reqObj.authenticator_data_b64) &&
      typeof globalThis.crypto !== "undefined" &&
      globalThis.crypto?.subtle
    ) {
      const store = readStoredPasskeyMap();
      const stored =
        store[resolvedCredentialId] ??
        (inMemoryLastPasskeyId ? store[inMemoryLastPasskeyId] : undefined);

      if (stored) {
        let privateKey = await getPasskeyFromIdb(stored.credential_id);
        let backedByIdb = privateKey !== null;

        if (
          !privateKey &&
          stored.private_jwk &&
          typeof stored.private_jwk === "object"
        ) {
          privateKey = await globalThis.crypto.subtle.importKey(
            "jwk",
            stored.private_jwk,
            { name: "ECDSA", namedCurve: "P-256" },
            false,
            ["sign"],
          );
          if (hasIndexedDbSupport()) {
            backedByIdb = await putPasskeyInIdb(
              stored.credential_id,
              privateKey,
            );
          }
        }

        if (privateKey) {
          const challengeRes = await this.requestPasskeyChallenge();
          const challenge = challengeRes.challenge;

          const { origin, hostname } = resolvePasskeyOrigin(this.baseUrl);

          const clientDataJson = JSON.stringify({
            type: "webauthn.get",
            challenge,
            origin,
            crossOrigin: false,
          });
          const encoder = new TextEncoder();
          const clientDataBytes = encoder.encode(clientDataJson);

          const rpIdHash = new Uint8Array(
            await globalThis.crypto.subtle.digest(
              "SHA-256",
              encoder.encode(hostname),
            ),
          );
          const nextCount = (stored.sign_count || 0) + 1;
          if (backedByIdb) {
            const { private_jwk: _omit, ...publicMeta } = stored;
            saveStoredPasskey({
              ...publicMeta,
              sign_count: nextCount,
            });
          } else {
            saveStoredPasskey({
              ...stored,
              sign_count: nextCount,
            });
          }

          const authenticatorData = new Uint8Array(37);
          authenticatorData.set(rpIdHash.subarray(0, 32), 0);
          authenticatorData[32] = 0x01; // UP (User Present) flag
          authenticatorData[33] = (nextCount >>> 24) & 0xff;
          authenticatorData[34] = (nextCount >>> 16) & 0xff;
          authenticatorData[35] = (nextCount >>> 8) & 0xff;
          authenticatorData[36] = nextCount & 0xff;

          const clientDataHash = new Uint8Array(
            await globalThis.crypto.subtle.digest("SHA-256", clientDataBytes),
          );

          const signedData = new Uint8Array(
            authenticatorData.length + clientDataHash.length,
          );
          signedData.set(authenticatorData, 0);
          signedData.set(clientDataHash, authenticatorData.length);

          const p1363Sig = new Uint8Array(
            await globalThis.crypto.subtle.sign(
              { name: "ECDSA", hash: "SHA-256" },
              privateKey,
              signedData,
            ),
          );
          const derSig = p1363ToDer(p1363Sig);

          payload = {
            mode: "passkey",
            // The credential whose key signed, which is not `req.credential_id` when that one is
            // unknown here and the newest key stood in for it.
            credential_id: stored.credential_id,
            client_data_json_b64: bytesToBase64Url(clientDataBytes),
            authenticator_data_b64: bytesToBase64Url(authenticatorData),
            signature_b64: bytesToBase64Url(derSig),
            ...(resolvedTotp ? { totp_code: resolvedTotp } : {}),
          };
        }
      }
    }

    const res = await this.request<{
      ok: boolean;
      requires_totp?: boolean;
      authority?: HubAuthority;
      session_token?: string;
    }>("/api/v1/auth/login", {
      method: "POST",
      body: JSON.stringify(payload),
    });
    if (res?.session_token) {
      this.setToken(res.session_token);
    }
    return res;
  }

  public async listOperators(): Promise<{ operators: OperatorAccount[] }> {
    return this.request<{ operators: OperatorAccount[] }>(
      "/api/v1/auth/operators",
    );
  }

  public async createOperator(req: {
    username: string;
    display_name?: string;
    role: OperatorRole;
    password?: string;
  }): Promise<{ operator: OperatorAccount }> {
    return this.request<{ operator: OperatorAccount }>(
      "/api/v1/auth/operators",
      {
        method: "POST",
        body: JSON.stringify(req),
      },
    );
  }

  public async deleteOperator(id: string): Promise<{ ok: boolean }> {
    return this.request<{ ok: boolean }>(
      `/api/v1/auth/operators/${encodeURIComponent(id)}`,
      {
        method: "DELETE",
      },
    );
  }

  public async enrollTotp(): Promise<TotpEnrollmentResponse> {
    return this.request<TotpEnrollmentResponse>("/api/v1/auth/totp/enroll", {
      method: "POST",
      body: JSON.stringify({}),
    });
  }

  public async verifyTotp(code: string): Promise<{ ok: boolean }> {
    return this.request<{ ok: boolean }>("/api/v1/auth/totp/verify", {
      method: "POST",
      body: JSON.stringify({ code }),
    });
  }

  public async registerPasskey(req: {
    credential_id: string;
    label: string;
    public_key_b64?: string;
  }): Promise<{ ok: boolean }> {
    let publicKeyB64 = req.public_key_b64;
    let generated: StoredClientPasskey | null = null;
    let generatedPrivateKey: CryptoKey | null = null;
    if (
      !publicKeyB64 &&
      typeof globalThis.crypto !== "undefined" &&
      globalThis.crypto?.subtle
    ) {
      let useIdb = false;
      if (hasIndexedDbSupport()) {
        try {
          const probeDb = await openPasskeyIdb();
          if (probeDb) {
            useIdb = true;
            try {
              probeDb.close();
            } catch {
              // Ignore close errors
            }
          }
        } catch {
          useIdb = false;
        }
      }

      const keyPair = await globalThis.crypto.subtle.generateKey(
        { name: "ECDSA", namedCurve: "P-256" },
        !useIdb,
        ["sign", "verify"],
      );
      const rawPub = new Uint8Array(
        await globalThis.crypto.subtle.exportKey("raw", keyPair.publicKey),
      );
      publicKeyB64 = bytesToBase64Url(rawPub);
      if (useIdb) {
        generatedPrivateKey = keyPair.privateKey;
        generated = {
          credential_id: req.credential_id,
          label: req.label,
          public_key_b64: publicKeyB64,
          sign_count: 0,
          created_at_ms: Date.now(),
        };
      } else {
        const privateJwk = await globalThis.crypto.subtle.exportKey(
          "jwk",
          keyPair.privateKey,
        );
        generated = {
          credential_id: req.credential_id,
          label: req.label,
          public_key_b64: publicKeyB64,
          private_jwk: privateJwk,
          sign_count: 0,
          created_at_ms: Date.now(),
        };
      }
    }
    const res = await this.request<{ ok: boolean }>(
      "/api/v1/auth/passkey/register",
      {
        method: "POST",
        body: JSON.stringify({
          credential_id: req.credential_id,
          label: req.label,
          ...(publicKeyB64 ? { public_key_b64: publicKeyB64 } : {}),
          // Pin the origin login will assert under, not the one the hub infers from its `Host`
          // header: behind a proxy, or with `baseUrl` on another origin, those differ and every
          // later login would fail.
          origin: resolvePasskeyOrigin(this.baseUrl).origin,
        }),
      },
    );
    // Keep the private key only once the hub holds the public half. Saved earlier, a refused
    // registration (duplicate id, bad origin) would leave an orphan key, and make it the
    // "last registered" passkey that sign-in then offers.
    if (generated) {
      let canSaveMetadata = !generatedPrivateKey;
      if (generatedPrivateKey) {
        const storedInIdb = await putPasskeyInIdb(
          req.credential_id,
          generatedPrivateKey,
        );
        canSaveMetadata =
          storedInIdb || inMemoryPasskeyCryptoKeys.has(req.credential_id);
      }
      if (canSaveMetadata) {
        saveStoredPasskey(generated);
      }
    }
    return res;
  }

  public async getCloudAdapters(): Promise<CloudAdaptersConfig> {
    const raw = await this.request<unknown>("/api/v1/cloud/adapters");
    return normalizeCloudAdaptersConfig(raw, DEFAULT_CLOUD_ADAPTERS_CONFIG);
  }

  public async updateCloudAdapters(
    cfg: Partial<CloudAdaptersConfig> | CloudAdaptersConfig,
  ): Promise<CloudAdaptersConfig> {
    const mergedFallback = normalizeCloudAdaptersConfig(
      cfg,
      DEFAULT_CLOUD_ADAPTERS_CONFIG,
    );
    const raw = await this.request<unknown>("/api/v1/cloud/adapters", {
      method: "PUT",
      body: JSON.stringify(cfg),
    });
    return normalizeCloudAdaptersConfig(raw, mergedFallback);
  }
}

export async function fetchNodes(client?: HubApiClient): Promise<EnrolledNode[]> {
  return (client ?? new HubApiClient()).fetchNodes();
}

export async function createNodeEnrollToken(
  label?: string,
  ttlSecs?: number,
  client?: HubApiClient,
): Promise<NodeEnrollTokenResponse> {
  return (client ?? new HubApiClient()).createNodeEnrollToken(label, ttlSecs);
}

export async function revokeNode(
  nodeId: string,
  client?: HubApiClient,
): Promise<void> {
  return (client ?? new HubApiClient()).revokeNode(nodeId);
}

export async function spinUpSession(
  req: SpinUpSessionRequest,
  client?: HubApiClient,
): Promise<SpinUpSessionResponse> {
  return (client ?? new HubApiClient()).spinUpSession(req);
}

export async function decommissionSession(
  sessionId: string,
  mode: DecommissionMode,
  client?: HubApiClient,
): Promise<DecommissionSessionResponse> {
  return (client ?? new HubApiClient()).decommissionSession(sessionId, mode);
}

export async function interruptSession(
  sessionId: string,
  client?: HubApiClient,
): Promise<InterruptSessionResponse> {
  return (client ?? new HubApiClient()).interruptSession(sessionId);
}

export async function setSessionModel(
  sessionId: string,
  model: string,
  thinkingLevelOrClient?: string | null | HubApiClient,
  client?: HubApiClient,
): Promise<SetSessionModelResponse> {
  if (thinkingLevelOrClient instanceof HubApiClient) {
    return thinkingLevelOrClient.setSessionModel(sessionId, model);
  }
  return (client ?? new HubApiClient()).setSessionModel(
    sessionId,
    model,
    thinkingLevelOrClient,
  );
}

export async function bulkSessionAction(
  action: BulkSessionAction,
  client?: HubApiClient,
): Promise<BulkSessionActionResponse> {
  return (client ?? new HubApiClient()).bulkSessionAction(action);
}

export async function listSlashCommands(
  client?: HubApiClient,
): Promise<{ commands: SlashCommandEntry[] }> {
  return (client ?? new HubApiClient()).listSlashCommands();
}

export async function recastAgent(
  nameKey: string,
  payload: RecastAgentRequest,
  client?: HubApiClient,
): Promise<RecastAgentResponse> {
  return (client ?? new HubApiClient()).recastAgent(nameKey, payload);
}

export async function pruneAttachments(
  olderThanDays?: number,
  client?: HubApiClient,
): Promise<PruneAttachmentsResponse> {
  return (client ?? new HubApiClient()).pruneAttachments(olderThanDays);
}
