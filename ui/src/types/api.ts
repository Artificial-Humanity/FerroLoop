/**
 * Exact 1:1 TypeScript wire contract matching `crates/hub/src/{api,auth,discovery,terminal,ws}.rs`.
 */

export type CockpitTab = "chat" | "git" | "mail" | "terminal" | "hub" | "gate";

export type HubAuthority =
  | { tier: "owner"; login: string }
  | { tier: "peer"; agent?: string | null };

export interface AuthSessionResponse {
  authenticated: boolean;
  authority?: HubAuthority;
  session_token?: string;
}

export interface HealthResponse {
  status: string;
  service: string;
}

export interface AgentRecord {
  id: string;
  name_key: string;
  display_name: string;
  email?: string;
  project: string;
  role: string;
  status: string;
  reports_to?: string | null;
  extra_areas?: string[];
  covered_areas?: string[];
  updated_at?: string;
  by?: string;
}

export interface AreaRecord {
  area_key: string;
  name: string;
  path?: string | null;
  purpose: string;
  updated_at?: string;
  by?: string;
}

export interface DiscoveredSessionSummary {
  id: string;
  source: "route" | "attempt" | string;
  adapter: "claude" | "antigravity" | "codex" | "muse" | "grok" | "webhook" | string;
  persona?: string | null;
  status: string;
  updated_at?: string | null;
  cwd?: string | null;
  record_id?: string | null;
  transcript_path?: string | null;
}

export interface AgentsResponse {
  agents: AgentRecord[];
  areas: AreaRecord[];
  routes: DiscoveredSessionSummary[];
}

export interface LivenessVerdict {
  /** Serialized by `fl-core` (`"live" | "unverifiable" | "exited"`). */
  status?: "live" | "unverifiable" | "exited" | string;
  /** Optional normalized UI alias (`"alive" | "stale" | "dead" | "unknown"`). */
  state?: "alive" | "stale" | "dead" | "unknown" | string;
  reason?: string;
  pid?: number;
  host?: string | null;
  heartbeat_at?: string | null;
  outcome?: string;
  exit_at?: string | null;
  duration_ms?: number;
  tokens_in?: number;
  tokens_out?: number;
  cost_usd_micros?: number;
}

export interface AgentLivenessItem {
  agent: string;
  project: string;
  role: string;
  has_route: boolean;
  adapter: string | null;
  verdict: LivenessVerdict;
}

export interface LivenessResponse {
  liveness: AgentLivenessItem[];
}

export function resolveLivenessState(
  verdict?: LivenessVerdict | null,
  hasRoute?: boolean,
): "alive" | "stale" | "dead" {
  if (!verdict) {
    return hasRoute ? "alive" : "dead";
  }
  const raw = (verdict.state ?? verdict.status ?? "").toLowerCase();
  if (raw === "alive" || raw === "live") {
    return "alive";
  }
  if (raw === "stale" || (raw === "unverifiable" && verdict.reason === "stale_status")) {
    return "stale";
  }
  if (hasRoute && raw === "unverifiable") {
    return "alive";
  }
  return "dead";
}

export interface TranscriptToolCall {
  name: string;
  arguments: unknown;
}

export interface TranscriptTurn {
  index: number;
  role: "user" | "assistant" | "system" | string;
  content: string;
  thinking?: string | null;
  tool_calls: TranscriptToolCall[];
  created_at?: string | null;
}

export interface SessionTranscriptResponse {
  session_id: string;
  turns: TranscriptTurn[];
  is_busy?: boolean;
  queue?: QueuedOperatorPrompt[];
}

export type TranscriptResponse = SessionTranscriptResponse;

export interface PromptAttachment {
  name: string;
  media_type: string;
  data_base64: string;
}

export type PromptAttachmentInput = PromptAttachment;

export interface SavedPromptAttachment {
  name: string;
  media_type: string;
  path: string;
  url: string;
}

export interface QueuedOperatorPrompt {
  id: string;
  session_id: string;
  persona?: string | null;
  prompt: string;
  effective_body?: string;
  by: string;
  attachments?: SavedPromptAttachment[];
  created_at: string;
}

export interface SessionQueueResponse {
  session_id: string;
  is_busy: boolean;
  queue: QueuedOperatorPrompt[];
}

export interface PromptRequest {
  prompt: string;
  by?: string;
  urgency?: "normal" | "urgent";
  subject?: string;
  send_now?: boolean;
  attachments?: PromptAttachment[];
}

export interface PromptResponse {
  session_id: string;
  persona?: string | null;
  status?: "sent" | "queued";
  queued?: boolean;
  is_busy?: boolean;
  delivered?: boolean;
  prompt_id?: string;
  prompt?: string;
  effective_body?: string;
  by?: string;
  queued_prompt?: QueuedOperatorPrompt;
  queue?: QueuedOperatorPrompt[];
  message?: WireMessage;
  receipts?: WireReceipt[];
  attachments?: SavedPromptAttachment[];
}

export type SessionPromptResponse = PromptResponse;

export interface WorktreeSummary {
  id: string;
  path: string;
  head: string;
  branch?: string | null;
  dirty_files: string[];
  is_clean: boolean;
}

export interface WorktreeFileStatus {
  status: string;
  path: string;
}

export interface WorktreeDiff {
  id: string;
  path: string;
  branch?: string | null;
  head: string;
  files: WorktreeFileStatus[];
  diff: string;
}

export interface PrCommitSummary {
  sha: string;
  message: string;
  author_login?: string;
}

export interface PrReviewSummary {
  reviewer_login: string;
  state: string;
  body: string;
  commit_id?: string;
}

export interface PullRequestItem {
  number: number;
  title?: string;
  author?: string;
  project?: string;
  branch?: string;
  head_sha?: string;
  status?: string;
  url?: string;
  files?: string[];
  commits?: PrCommitSummary[];
  reviews?: PrReviewSummary[];
  [key: string]: unknown;
}

export interface PrReviewRequest {
  verdict?: "approve" | "request_changes" | "comment" | string;
  approve?: boolean;
  request_changes?: boolean;
  body?: string;
  by?: string;
  project?: string;
  head_sha?: string;
  commits?: PrCommitSummary[];
  files?: string[];
}

export interface PrReviewResponse {
  number: number;
  verdict: string;
  reviewer: string;
  role?: string;
  head_sha: string;
  body: string;
  policy: unknown;
}

export interface PrMergeRequest {
  method?: "merge" | "squash" | "rebase" | string;
  by?: string;
  project?: string;
  head_sha?: string;
  commits?: PrCommitSummary[];
  files?: string[];
  reviews?: PrReviewSummary[];
}

export interface PrMergeResponse {
  number: number;
  merged: boolean;
  method: string;
  merged_by: string;
  head_sha: string;
  policy: unknown;
}

export interface WireMessage {
  id: string;
  thread_id: string;
  reply_to?: string | null;
  correlation_id?: string | null;
  sender_id?: string | null;
  sender_name: string;
  sender_host?: string | null;
  to: string;
  kind: "mail" | "ask" | "reply" | "handoff" | "task_offer" | "approval_request" | "system" | string;
  urgency: "normal" | "urgent" | string;
  durable: boolean;
  subject: string;
  body: string;
  created_at: string;
  deadline_at?: string | null;
}

export interface WireReceipt {
  message_id: string;
  recipient_id: string;
  recipient_name: string;
  status: "pending" | "delivered" | "acked" | "expired" | string;
  attempts: number;
  updated_at: string;
  acked_at?: string | null;
}

export type DeliveryReceipt = WireReceipt;

export interface WireInboxItem {
  message: WireMessage;
  receipt?: WireReceipt;
  receipts?: WireReceipt[];
}

export interface WireSendRequest {
  to: string;
  subject: string;
  body: string;
  from?: string;
  sender_name?: string;
  kind?: string;
  urgency?: "normal" | "urgent";
  durable?: boolean;
  reply_to?: string;
  thread_id?: string;
}

export interface WireSendResponse {
  message: WireMessage;
  receipts: WireReceipt[];
}

export interface WireAckRequest {
  id: string;
  by?: string;
}

export interface WireAckResponse {
  message_id: string;
  by: string;
  receipt?: WireReceipt | null;
}

export interface StepDef {
  step_id: string;
  title: string;
  prompt: string;
  assignee?: unknown;
  reviews_step?: string | null;
  require_different_vendor?: boolean;
  completion?: {
    mode: "gated" | "human_sign_off" | "ungated" | string;
    approval_tier?: "owner" | "lab_manager" | "lead" | string;
    gates?: string[];
  };
  join_policy?: unknown;
  max_attempts?: number;
  max_visits?: number;
  budget_usd_micros?: number | null;
  max_tokens?: number | null;
  isolate_worktree?: boolean;
}

export interface EdgeDef {
  from_step: string;
  to_step: string;
  on: "pass" | "fail" | "needs_human" | "always" | string;
}

export interface WorkflowDef {
  id: string;
  project: string;
  slug: string;
  title: string;
  start_step: string;
  steps: StepDef[];
  edges: EdgeDef[];
  max_total_steps?: number;
  budget_usd_micros?: number | null;
  max_tokens?: number | null;
  authored_at_commit?: string;
  authored_by?: string;
}

export interface StepInstance {
  step_id: string;
  visit_count: number;
  attempt_count: number;
  state: string;
  assigned_agent?: string | null;
  adapter_used?: string | null;
  record_id?: string | null;
  latest_attempt?: string | null;
  last_outcome?: "pass" | "fail" | "needs_human" | string | null;
  tokens_used: number;
  cost_usd_micros: number;
  worktree_branch?: string | null;
  needs_human_reason?: string | null;
}

export interface WorkflowRun {
  id: string;
  workflow_id: string;
  project: string;
  title: string;
  status: "running" | "needs_human" | "completed" | "failed" | "canceled" | string;
  active_steps: string[];
  steps: Record<string, StepInstance>;
  total_steps_executed: number;
  total_tokens: number;
  total_cost_usd_micros: number;
  created_at: string;
  updated_at: string;
  by: string;
}

export interface HandoffEntry {
  summary: string;
  shipped: string[];
  in_flight: string[];
  blockers: string[];
  next_steps: string[];
  notes?: string | null;
}

export interface MemoryEntry {
  category: string;
  basis: string;
  lifecycle: string;
  superseded_by?: string | null;
  pinned: boolean;
  summary: string;
  body: string;
}

export interface RuntimeEntry {
  value: unknown;
  merge_mode: string;
}

export interface StateEntry {
  id: string;
  scope: string;
  concern: "memory" | "handoff" | "runtime" | string;
  slug: string;
  sensitive: boolean;
  ever_sensitive: boolean;
  payload: Record<string, unknown>;
  head_revision: string;
  updated_at: string;
  by: string;
}

export interface WorkflowRunDetailResponse {
  run: WorkflowRun;
  handoffs: StateEntry[];
  facts: StateEntry[];
}

export interface AdapterStats {
  attempts: number;
  completed: number;
  duration_ms: number;
  tokens_in: number;
  tokens_out: number;
  cost_usd_micros: number;
}

export interface RegretChallenge {
  name: string;
  from: string;
  to: string;
  metric: "cost_usd_micros" | "duration_ms" | string;
  measured_mean: number;
  project_median: number;
  ratio: number;
}

export interface HubStatsResponse {
  attempts: number;
  by_status: Record<string, number>;
  wall_clock_ms: number;
  tokens_in: number;
  tokens_out: number;
  cost_usd_micros: number;
  zero_cost_attempts: number;
  by_adapter: Record<string, AdapterStats>;
  regret_audit: RegretChallenge[];
}

export interface ArchComponent {
  id: string;
  title: string;
  kind: "crate" | "service" | "ui" | "store" | "external" | string;
  description: string;
  package?: string | null;
  paths: string[];
}

export interface ArchEdge {
  from: string;
  to: string;
  label: string;
}

export interface ArchitectureModel {
  system_id: string;
  title: string;
  strict_coverage: boolean;
  components: ArchComponent[];
  edges: ArchEdge[];
}

export type ArchViolation =
  | {
      kind: "undeclared_dependency";
      from_component: string;
      to_component: string;
      evidence: string;
    }
  | {
      kind: "unmapped_package";
      package: string;
      manifest_path: string;
    }
  | {
      kind: "unmapped_file";
      path: string;
    };

export interface ArchConformanceReport {
  system_id: string;
  passed: boolean;
  checked_packages: number;
  checked_files: number;
  violations: ArchViolation[];
}

export interface ArchitectureResponse {
  model: ArchitectureModel | null;
  likec4_dsl: string | null;
  conformance: ArchConformanceReport | null;
}

/**
 * FerroGate (`/api/v1/gate/*`, `crates/gate/src/{vault,connector,bridge}.rs`).
 * Secret metadata never carries plaintext values.
 */
export interface VaultSecretMetadata {
  name: string;
  description?: string;
  allowed_hosts: string[];
  allowed_methods: string[];
  allowed_agents: string[];
  allow_private_network: boolean;
  created_at: string;
  updated_at: string;
  lease_ttl_secs?: number;
  expires_at?: string;
}

export interface SetVaultSecretInput {
  name: string;
  value: string;
  description?: string;
  allowed_hosts: string[];
  allowed_methods?: string[];
  allowed_agents?: string[];
  allow_private_network?: boolean;
  lease_ttl_secs?: number;
}

export interface GateConnectorToolSummary {
  name: string;
  description: string;
  method: string;
  path: string;
  path_params?: string[];
  query_params?: string[];
  body_params?: string[];
  response_fields?: string[];
}

export interface GateBridgeSummary {
  id: string;
  kind: string;
  enabled: boolean;
  default_target?: string | null;
  channel_routes: Record<string, string>;
}

export interface GateConnectorSummary {
  id: string;
  /** `ConnectorManifest.title` on the wire; `name` kept as an older alias. */
  title: string;
  name?: string;
  version: string;
  description: string;
  base_url: string;
  allowed_hosts: string[];
  enabled?: boolean;
  tools: GateConnectorToolSummary[];
  bridges: GateBridgeSummary[];
}

export interface GateSecretsResponse {
  secrets: VaultSecretMetadata[];
}

export interface GateSecretResponse {
  secret: VaultSecretMetadata;
}

/** `handle_delete_gate_secret` in `crates/hub/src/api.rs` returns `removed`. */
export interface DeleteGateSecretResponse {
  removed: boolean;
  name: string;
}

export interface GateConnectorsResponse {
  connectors: GateConnectorSummary[];
}

export interface HubWsFrame<T = unknown> {
  channel: "system" | "wire" | "session" | "workflow" | "terminal" | string;
  event: string;
  data: T;
}

export interface TerminalOpenPayload {
  session_id?: string;
  cwd?: string;
  cols?: number;
  rows?: number;
  command?: string[] | string;
  args?: string[];
  env?: Record<string, string>;
}

export interface TerminalOpenedPayload {
  session_id: string;
  pid?: number | null;
  cwd: string;
  cols: number;
  rows: number;
}

export interface TerminalOutputPayload {
  session_id: string;
  stream: "stdout" | "stderr" | string;
  data: string;
  output?: string;
}

export interface TerminalInputPayload {
  session_id?: string;
  data: string;
}

export interface TerminalResizePayload {
  session_id?: string;
  cols: number;
  rows: number;
}

export interface TerminalClosePayload {
  session_id?: string;
}

export interface TerminalExitedPayload {
  session_id: string;
  exit_code: number;
}

/**
 * Default AI-Lab-0 workspace agent roster used as fallback metadata when a fresh local store
 * has not yet seeded `AgentRecord` rows.
 */
export const DEFAULT_WORKSPACE_AGENTS: AgentRecord[] = [
  {
    id: "urn:fl:agent:jarvis",
    name_key: "jarvis",
    display_name: "JARVIS",
    email: "jarvis@artificialhumanity.io",
    project: "AI-Lab-AMD",
    role: "lab_manager",
    status: "active",
    covered_areas: ["AI-Lab-AMD", ".agents", "Notes"],
  },
  {
    id: "urn:fl:agent:ferris",
    name_key: "ferris",
    display_name: "Ferris",
    email: "ferris@artificialhumanity.io",
    project: "FerroLoop",
    role: "lead",
    status: "active",
    covered_areas: ["FerroLoop"],
  },
  {
    id: "urn:fl:agent:felicia",
    name_key: "felicia",
    display_name: "Felicia",
    email: "felicia@artificialhumanity.io",
    project: "FerroLoop",
    role: "assistant",
    status: "active",
    reports_to: "ferris",
    covered_areas: ["FerroLoop"],
  },
  {
    id: "urn:fl:agent:kayla",
    name_key: "kayla",
    display_name: "Kayla",
    email: "kayla@artificialhumanity.io",
    project: "Kaggle-Gemma4",
    role: "lead",
    status: "active",
    covered_areas: ["Kaggle-Gemma4"],
  },
  {
    id: "urn:fl:agent:kacey",
    name_key: "kacey",
    display_name: "Kacey",
    email: "kacey@artificialhumanity.io",
    project: "Kaggle-Gemma4",
    role: "assistant",
    status: "active",
    reports_to: "kayla",
    covered_areas: ["Kaggle-Gemma4"],
  },
  {
    id: "urn:fl:agent:lucy",
    name_key: "lucy",
    display_name: "Lucy",
    email: "lucy@artificialhumanity.io",
    project: "Lucida",
    role: "lead",
    status: "active",
    covered_areas: ["Lucida"],
  },
  {
    id: "urn:fl:agent:penelope",
    name_key: "penelope",
    display_name: "Penelope",
    email: "penelope@artificialhumanity.io",
    project: "Prosodia",
    role: "lead",
    status: "active",
    covered_areas: ["Prosodia"],
  },
  {
    id: "urn:fl:agent:sonya",
    name_key: "sonya",
    display_name: "Sonya",
    email: "sonya@artificialhumanity.io",
    project: "Sonora",
    role: "lead",
    status: "active",
    covered_areas: ["Sonora"],
  },
  {
    id: "urn:fl:agent:wesley",
    name_key: "wesley",
    display_name: "Wesley",
    email: "wesley@artificialhumanity.io",
    project: "Website",
    role: "lead",
    status: "active",
    covered_areas: ["Website", ".github"],
  },
];
