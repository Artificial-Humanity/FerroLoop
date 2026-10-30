import type {
  AgentsResponse,
  ArchitectureResponse,
  ArtifactListResponse,
  ArtifactPreviewResponse,
  AuthSessionResponse,
  ClearSessionMode,
  ClearSessionResponse,
  DiscoveredSessionSummary,
  HealthResponse,
  HubStatsResponse,
  LivenessResponse,
  PrMergeRequest,
  PrMergeResponse,
  PromptAttachment,
  PromptRequest,
  PromptResponse,
  PrReviewRequest,
  PrReviewResponse,
  PullRequestItem,
  QueuedOperatorPrompt,
  SessionQueueResponse,
  SessionTranscriptResponse,
  CodeSearchKind,
  CodeSearchResponse,
  ImportWorkspaceStateResponse,
  StateCheckpointResponse,
  StateEntriesResponse,
  StateEntry,
  StateEntryResponse,
  StateSearchResponse,
  UpsertStateEntryInput,
  WireAckRequest,
  WireAckResponse,
  WireInboxItem,
  WireSendRequest,
  WireSendResponse,
  DeleteGateSecretResponse,
  EnrolledNode,
  GateConnectorsResponse,
  GateSecretResponse,
  GateSecretsResponse,
  NodeEnrollTokenResponse,
  NodesResponse,
  SetVaultSecretInput,
  WorkflowDef,
  WorkflowRun,
  WorkflowRunDetailResponse,
  WorktreeDiff,
  WorktreeSummary,
} from "../types/api";

export const DEFAULT_TOKEN_STORAGE_KEY = "fl_hub_session_token";

export class HubApiError extends Error {
  public readonly status: number;
  public readonly detail?: string;

  constructor(status: number, message: string, detail?: string) {
    super(message);
    this.name = "HubApiError";
    this.status = status;
    this.detail = detail;
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
    const url = `${this.baseUrl}${path.startsWith("/") ? path : `/${path}`}`;
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
      throw new HubApiError(response.status, errMsg, detail);
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

  public async listSessionArtifacts(sessionId?: string | null): Promise<ArtifactListResponse> {
    const qs = new URLSearchParams();
    if (sessionId && sessionId.trim().length > 0) {
      qs.set("session_id", sessionId.trim());
    }
    const suffix = qs.toString() ? `?${qs.toString()}` : "";
    return this.request<ArtifactListResponse>(`/api/v1/artifacts/list${suffix}`);
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

  public async revokeNode(nodeId: string): Promise<void> {
    await this.request<unknown>(`/api/v1/nodes/${encodeURIComponent(nodeId)}`, {
      method: "DELETE",
    });
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
