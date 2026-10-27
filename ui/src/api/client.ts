import type {
  AgentsResponse,
  ArchitectureResponse,
  AuthSessionResponse,
  DiscoveredSessionSummary,
  HealthResponse,
  HubStatsResponse,
  LivenessResponse,
  PrMergeRequest,
  PrMergeResponse,
  PromptRequest,
  PromptResponse,
  PrReviewRequest,
  PrReviewResponse,
  PullRequestItem,
  SessionTranscriptResponse,
  StateEntry,
  WireAckRequest,
  WireAckResponse,
  WireInboxItem,
  WireSendRequest,
  WireSendResponse,
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

  public async sendSessionPrompt(
    sessionId: string,
    payload: PromptRequest,
  ): Promise<PromptResponse> {
    return this.request<PromptResponse>(
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/prompt`,
      {
        method: "POST",
        body: JSON.stringify(payload),
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

  public async getState(params?: {
    scope?: string;
    concern?: string;
  }): Promise<{ entries: StateEntry[] }> {
    const qs = new URLSearchParams();
    if (params?.scope) {
      qs.set("scope", params.scope);
    }
    if (params?.concern) {
      qs.set("concern", params.concern);
    }
    const suffix = qs.toString() ? `?${qs.toString()}` : "";
    return this.request<{ entries: StateEntry[] }>(`/api/v1/state${suffix}`);
  }

  public async upsertState(payload: Record<string, unknown>): Promise<{ entry: StateEntry }> {
    return this.request<{ entry: StateEntry }>("/api/v1/state", {
      method: "POST",
      body: JSON.stringify(payload),
    });
  }

  public async getStats(): Promise<HubStatsResponse> {
    return this.request<HubStatsResponse>("/api/v1/stats");
  }

  public async getArchitecture(path?: string): Promise<ArchitectureResponse> {
    const suffix = path ? `?path=${encodeURIComponent(path)}` : "";
    return this.request<ArchitectureResponse>(`/api/v1/architecture${suffix}`);
  }
}
