/**
 * Exact 1:1 TypeScript wire contract matching `crates/hub/src/{api,auth,discovery,terminal,ws}.rs`.
 */

export type CockpitTab =
  | "chat"
  | "mail"
  | "artifacts"
  | "git"
  | "board"
  | "terminal"
  | "hub"
  | "gate"
  | "state";

export type RecordState = "todo" | "doing" | "done";

export interface ProjectRecordItem {
  id: string;
  handle?: number | null;
  display_id: string;
  project: string;
  title: string;
  state: RecordState;
  area?: string | null;
  tier?: string;
  findings_open?: number;
  findings_total?: number;
}

export interface RecordsBoardSummary {
  todo_count: number;
  doing_count: number;
  done_count: number;
  total_count: number;
  completion_pct: number;
  open_findings_count: number;
  attempts_count: number;
  total_cost_usd: number;
}

export interface RecordsListResponse {
  project: string;
  available_projects: string[];
  records: ProjectRecordItem[];
  summary: RecordsBoardSummary;
}

export interface CreateRecordRequest {
  project?: string;
  title: string;
  area?: string;
  state?: RecordState;
}

export interface CreateRecordResponse {
  record: ProjectRecordItem;
}

export interface MoveRecordResponse {
  ok: boolean;
  outcome: string;
  record: ProjectRecordItem;
}

export type ArtifactKind =
  | "markdown"
  | "mermaid"
  | "svg"
  | "image"
  | "pdf"
  | "code"
  | "binary";

export interface ArtifactPreviewResponse {
  path: string;
  name: string;
  ext: string;
  mime_type: string;
  size_bytes: number;
  kind: ArtifactKind;
  content?: string | null;
  /** A structured .docx/.xlsx/.pptx preview supplied with the preview itself. */
  office_preview?: OfficePreview | null;
  raw_url: string;
  download_url: string;
}

export interface ArtifactSummaryItem {
  path: string;
  name: string;
  ext: string;
  kind: ArtifactKind;
  size_bytes: number;
  status?: "live" | "archived";
  original_path?: string;
}

/** One recorded content revision of an artifact (`GET /api/v1/artifacts/versions`). */
export interface ArtifactVersionInfo {
  version: number;
  sha256: string;
  size_bytes: number;
  recorded_at_ms: number;
  is_current: boolean;
  /** False when the revision was above the archive cap and only its digest was kept. */
  has_content?: boolean;
}

export interface ArtifactVersionsResponse {
  ok: boolean;
  path: string;
  versions: ArtifactVersionInfo[];
}

export type OfficeBlock =
  | { type: "heading"; level: number; text: string }
  | { type: "paragraph"; text: string }
  | { type: "table"; rows: string[][] };

export interface DocxPreview {
  kind: "docx";
  blocks: OfficeBlock[];
  truncated?: boolean;
}

export interface XlsxSheet {
  name: string;
  rows: string[][];
  row_count?: number;
  col_count?: number;
  truncated?: boolean;
}

export interface XlsxPreview {
  kind: "xlsx";
  sheets: XlsxSheet[];
  truncated?: boolean;
}

export interface PptxSlide {
  index: number;
  title: string;
  bullets: string[];
  notes: string;
}

export interface PptxPreview {
  kind: "pptx";
  slides: PptxSlide[];
  truncated?: boolean;
}

export type OfficePreview = DocxPreview | XlsxPreview | PptxPreview;

/** `GET /api/v1/artifacts/office-preview`. */
export interface OfficePreviewResponse {
  ok: boolean;
  path: string;
  name?: string;
  ext?: string;
  size_bytes?: number;
  preview: OfficePreview;
}

export interface ArtifactStorageStats {
  root_dir?: string;
  managed_files?: number;
  managed_bytes?: number;
  version_files?: number;
  version_bytes?: number;
  registry_entries?: number;
  truncated?: boolean;
}

export interface ArtifactListResponse {
  artifacts: ArtifactSummaryItem[];
  pruned_count?: number;
  storage?: ArtifactStorageStats | null;
}

export interface PruneArtifactsRequest {
  paths?: string[];
  session_id?: string;
  worktree_path?: string;
  dry_run?: boolean;
  max_age_days?: number;
  status?: "archived" | "live" | "all";
  kinds?: string[];
  prune_versions_keep?: number;
}

export interface PruneArtifactsResponse {
  ok?: boolean;
  dry_run?: boolean;
  removed_entries?: number;
  removed_versions?: number;
  bytes_freed?: number;
  failed_deletes?: number;
  pruned_count?: number;
  pruned?: number;
  archived_count?: number;
  active_count?: number;
  live_count?: number;
  remaining_count?: number;
  pruned_paths?: string[];
  storage?: ArtifactStorageStats | null;
}

export interface PruneAttachmentsRequest {
  older_than_days?: number;
}

export interface PruneAttachmentsResponse {
  deleted_count: number;
  deleted_bytes: number;
  kept_count: number;
  older_than_days?: number | null;
  retention_disabled?: boolean;
}

export interface ChangeArtifactsDirRequest {
  new_path: string | null;
  migrate: boolean;
}

export interface ChangeArtifactsDirResponse {
  project?: string;
  old_path: string;
  new_path: string;
  migrated_files: number;
  migrated_bytes: number;
  artifacts_dir: string | null;
  default_artifacts_dir: string;
  effective_artifacts_dir: string;
}

export interface ProjectArtifactsDirInfo {
  id: string;
  artifacts_dir: string | null;
  default_artifacts_dir: string;
  effective_artifacts_dir: string;
}

export interface ProjectsListResponse {
  projects: ProjectArtifactsDirInfo[];
}

export type ProjectKind = "project" | "virtual";

export type ProjectTracker = {
  kind: "github";
  repo: string;
  credential: "app" | "env";
};

/**
 * One entry of `GET /api/v1/projects`, and the body of the `POST` and `PATCH` answers.
 * The artifact-directory fields mirror `ProjectArtifactsDirInfo`; they are optional because an
 * older hub may omit them. `id` always equals `slug`.
 */
export interface ProjectSummary {
  id: string;
  slug: string;
  name: string;
  kind: ProjectKind;
  /** `null` for a virtual group. */
  path: string | null;
  has_git: boolean;
  purpose: string | null;
  tracker: ProjectTracker | null;
  store_path: string | null;
  active_agent_count: number;
  /** The three artifact fields are explicit `null` for a virtual group, which has no directory. */
  artifacts_dir?: string | null;
  default_artifacts_dir?: string | null;
  effective_artifacts_dir?: string | null;
  /**
   * Set by `listProjects()` on an entry an older hub served: it had no `slug`, `name` or
   * `kind`, only an `id` and artifact directories. Never sent by a hub.
   */
  legacy?: boolean;
}

export interface CreateProjectRequest {
  name: string;
  slug?: string;
  kind: ProjectKind;
  path?: string | null;
  purpose?: string;
  git_init?: boolean;
  /** Tracker repo, `owner/name`. Omit for none. */
  github?: string;
  credential?: "app" | "env";
}

export interface UpdateProjectRequest {
  name?: string;
  purpose?: string;
  path?: string;
  /** Tracker repo, `owner/name`; `""` clears the tracker on update (never send `credential` with it). */
  github?: string;
  credential?: "app" | "env";
}

export interface DeleteProjectRequest {
  reassign_to?: string | null;
  decommission_agents?: boolean;
  delete_store?: boolean;
  delete_dir?: boolean;
}

export interface DeleteProjectResponse {
  deleted: string;
  reassigned_agents: number;
  decommissioned_agents: number;
  store_deleted: boolean;
  dir_deleted: boolean;
  /**
   * Present only when the store files or the directory could not be deleted AFTER the project
   * was unregistered.
   */
  error?: string;
}

export interface HubSettings {
  theme_preference: "dark" | "light" | "system";
  timezone: string;
  subsystems: {
    ferrogate_enabled: boolean;
    ferrostate_enabled: boolean;
    fl_flow_enabled: boolean;
    ferrodeck_enabled?: boolean;
    ferrowire_enabled?: boolean;
    ferrotrack_enabled?: boolean;
    ferrowatch_enabled?: boolean;
  };
  notifications: {
    notify_on_pr_review: boolean;
    notify_on_session_error: boolean;
    notify_on_wire_urgent: boolean;
    sound_enabled: boolean;
  };
  hub_connection: {
    auto_reconnect_ws: boolean;
    poll_interval_secs: number;
  };
  /** Days the hub keeps chat attachments (0..=365, default 7; 0 = never auto-delete). */
  attachment_retention_days?: number;
  /** Hub home directory (read-only on GET /api/v1/settings). */
  home_dir?: string;
  /** Owner system artifacts directory override (null = none set). */
  artifacts_dir?: string | null;
  /** Default system artifacts directory (<home_dir>/artifacts). */
  default_artifacts_dir?: string;
  /** Effective system artifacts directory (artifacts_dir ?? default_artifacts_dir). */
  effective_artifacts_dir?: string;
  /** Per-project artifacts directory overrides keyed by lowercase project slug. */
  project_artifacts_dirs?: Record<string, string>;
}

export type HubSettingsPatch = Partial<HubSettings>;

export const DEFAULT_HUB_SETTINGS: HubSettings = {
  theme_preference: "dark",
  timezone: "UTC",
  attachment_retention_days: 7,
  subsystems: {
    ferrogate_enabled: true,
    ferrostate_enabled: true,
    fl_flow_enabled: true,
  },
  notifications: {
    notify_on_pr_review: true,
    notify_on_session_error: true,
    notify_on_wire_urgent: true,
    sound_enabled: false,
  },
  hub_connection: {
    auto_reconnect_ws: true,
    poll_interval_secs: 3,
  },
};

export type HubAuthority =
  | { tier: "owner"; login: string }
  | { tier: "peer"; agent?: string | null };

export type OperatorRole = "owner" | "admin" | "operator" | "viewer";

export interface OperatorAccount {
  id: string;
  username: string;
  display_name: string;
  role: OperatorRole;
  has_password: boolean;
  totp_enabled: boolean;
  passkeys_count: number;
  created_at_ms: number;
}

export interface TotpEnrollmentResponse {
  secret_base32: string;
  otpauth_uri: string;
}

export interface CloudAdaptersConfig {
  store_sync: {
    provider: "none" | "gcp" | "aws" | "azure";
    sync_mode:
      | "write_through"
      | "write_behind"
      | "local_cache"
      | "cloud_pre_cache";
    endpoint_url: string;
    enabled: boolean;
  };
  gate_cloud: {
    registry_provider: "none" | "gcp" | "aws" | "azure";
    relay_endpoint_url: string;
    secret_manager_enabled: boolean;
    model_armor_enabled: boolean;
  };
}

export const DEFAULT_CLOUD_ADAPTERS_CONFIG: CloudAdaptersConfig = {
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

export interface RecastAgentRequest {
  project: string;
  role: string;
  extra_areas?: string[];
  create_area?: boolean;
  replace_lead?: boolean;
}

export interface RecastAgentResponse {
  agent?: AgentRecord;
  /** The retired Lead's full record. Present only when this call retired a Lead. */
  retired_lead?: AgentRecord;
  /** The retired Lead's display name, or null. */
  retires_lead?: string | null;
  /** The areas this call created. */
  creates_areas?: string[];
  /** The endpoint never returns this. SettingsModal reads it only as a fallback after `agent`. */
  after?: AgentRecord;
  /** 409 body: the Lead that blocks the recast until `replace_lead` is true. */
  current_lead?: { display_name: string; id: string };
  /** 409 body: the areas that do not exist until `create_area` is true. */
  missing_areas?: string[];
}

export interface AreaRecord {
  area_key: string;
  name: string;
  path?: string | null;
  purpose: string;
  updated_at?: string;
  by?: string;
}

export interface QuotaWindowUsage {
  used_percent: number;
  window_minutes?: number | null;
  resets_at_ms?: number | null;
}

export interface SessionQuotaUsage {
  tier?: string | null;
  primary_window?: QuotaWindowUsage | null;
  secondary_window?: QuotaWindowUsage | null;
  exhausted_reason?: string | null;
}

export interface DiscoveredSessionSummary {
  id: string;
  source: "route" | "attempt" | string;
  adapter: "claude" | "antigravity" | "codex" | "muse" | "grok" | "webhook" | string;
  model?: string | null;
  thinking_level?: string | null;
  available_models?: string[] | null;
  persona?: string | null;
  status: string;
  updated_at?: string | null;
  cwd?: string | null;
  record_id?: string | null;
  transcript_path?: string | null;
  quota_usage?: SessionQuotaUsage | null;
  /** Server-measured context window usage; present only when the runtime reports one. */
  context_usage?: SessionContextUsage | null;
  /**
   * Effective execution (permission) mode: `auto`, `yolo`, `plan`, `default`, `accept-edits`,
   * or a lower-cased vendor value. Absent when the runtime states none.
   */
  execution_mode?: string | null;
}

export type DiscoveredSession = DiscoveredSessionSummary;
export type SessionSummary = DiscoveredSessionSummary;

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

/**
 * How the hub found a context window: `reported` (the session or the vendor said it for this
 * session), `catalog` (the vendor's model catalog lists it) or `assumed` (a default: nothing says
 * it).
 */
export type ContextWindowSource = "reported" | "catalog" | "assumed";

export interface SessionContextUsage {
  used_tokens: number;
  max_tokens: number;
  /** How `max_tokens` was found. Absent from an older hub: treat as unknown. */
  source?: ContextWindowSource;
  /** Window sizes the model offers, ascending; absent when no vendor file lists them. */
  offered_windows?: number[];
}

export interface SessionTranscriptResponse {
  session_id: string;
  turns: TranscriptTurn[];
  is_busy?: boolean;
  in_flight_unconfirmed?: boolean;
  queue?: QueuedOperatorPrompt[];
  /** Server-measured context usage when the hub reports it. */
  context_usage?: SessionContextUsage | null;
  /** Vendor account quota/allowance usage when reported by the adapter/hub. */
  quota_usage?: SessionQuotaUsage | null;
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
  delivery_state?: "pending" | "sending" | "unconfirmed";
  delivery_error?: string | null;
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
  in_flight_unconfirmed?: boolean;
  queue: QueuedOperatorPrompt[];
}

export type ClearSessionMode = "clear" | "reforge";

export interface ClearSessionResponse {
  ok: boolean;
  session_id: string;
  persona?: string | null;
  mode: ClearSessionMode;
  prompt?: string | null;
}

export interface SpinUpSessionRequest {
  persona: string;
  project: string;
  role: string;
  vendor: string;
  model?: string;
  node_id?: string;
  hosted?: boolean;
  cwd?: string;
  worktree_policy?: "create" | "reuse" | "shared";
  allocated_port?: number;
  allocated_vite_port?: number;
  permission_policy?: unknown;
}

export interface SpinUpSessionResponse {
  session_id: string;
  persona: string;
  project: string;
  role: string;
  vendor: string;
  model?: string | null;
  node_id: string;
  hosted: boolean;
  status: string;
  tmux_target?: string | null;
  hydration_dispatched: boolean;
  prompt?: string | null;
}

export type DecommissionMode = "prepare" | "teardown";

export interface DecommissionSessionResponse {
  session_id: string;
  persona?: string | null;
  mode: DecommissionMode;
  status: string;
  handshake_dispatched: boolean;
  unregistered: boolean;
  runtime_stopped: boolean;
  prompt?: string | null;
}

export interface InterruptSessionResponse {
  session_id: string;
  interrupted: boolean;
  vendor: string;
  method: string;
}

export interface SetSessionModelRequest {
  model: string;
  thinking_level?: string | null;
}

export interface SetSessionModelResponse {
  session_id: string;
  vendor: string;
  model: string;
  thinking_level?: string | null;
  switched: boolean;
  method: string;
  /**
   * Set only for Antigravity: the effort-suffixed model id stored for the next launch. A
   * running Antigravity session keeps its model; nothing is sent to it.
   */
  cli_model?: string;
}

export type BulkSessionAction =
  | "compact"
  | "clear"
  | "reforge"
  | "prepare"
  | "teardown";

export interface BulkSessionActionResponse {
  action: BulkSessionAction;
  total: number;
  succeeded: number;
  results: Array<{
    session_id: string;
    ok: boolean;
    error?: string | null;
  }>;
}

export interface SlashCommandEntry {
  name: string;
  kind: "command" | "skill" | "workflow";
  description: string;
}

export const DEFAULT_SLASH_COMMANDS: SlashCommandEntry[] = [
  {
    name: "compact",
    kind: "command",
    description: "Summarize and compact the active session context window",
  },
  {
    name: "clear",
    kind: "command",
    description: "Clear the conversation history for a clean-slate session",
  },
  {
    name: "model",
    kind: "command",
    description: "Switch the active language model for this session",
  },
  {
    name: "goal",
    kind: "command",
    description: "Run an autonomous long-running task until the goal is achieved",
  },
  {
    name: "plan",
    kind: "command",
    description: "Draft a structured step-by-step plan before execution",
  },
  {
    name: "boost",
    kind: "command",
    description: "Deep thinking, strategic planning, and rigorous verification",
  },
  {
    name: "code-review",
    kind: "skill",
    description: "Subagent-driven code review, finding evaluation, and remediation",
  },
  {
    name: "subagent-dev",
    kind: "skill",
    description: "Execute a multi-task plan with per-task implementer and reviewer subagents",
  },
];

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
  in_flight_unconfirmed?: boolean;
  persona?: string | null;
  status?: "sent" | "queued";
  queued?: boolean;
  is_busy?: boolean;
  delivered?: boolean;
  delivery_error?: string | null;
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
  project?: string | null;
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

/** A pull request review, as the Hub reads it (`fl_core::pr_policy::PrReviewSummary`). */
export interface PrReviewSummary {
  id: number;
  author_login: string;
  /** The author's numeric GitHub user id; absent for a deleted account. The Hub counts an
   * agent approval from the Reviewer App by this id, not by login. */
  author_id?: number | null;
  state: string;
  body: string;
  commit_id?: string | null;
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

export type CommitteeVoteVerdict =
  | "APPROVE"
  | "APPROVE WITH AMENDMENTS"
  | "BLOCK"
  | "PENDING";

export interface WireDelegateMetadata {
  target_record: string;
  deliverable: string;
  branch_or_pr?: string | null;
  resume_on_reply: boolean;
}

export interface WireCommitteeMetadata {
  council_track: string;
  quorum: string;
  artifact_path: string;
  artifact_sha256?: string | null;
  votes?: Record<string, CommitteeVoteVerdict>;
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
  kind:
    | "mail"
    | "ask"
    | "reply"
    | "handoff"
    | "task_offer"
    | "approval_request"
    | "delegate"
    | "committee"
    | "system"
    | string;
  urgency: "normal" | "urgent" | string;
  durable: boolean;
  subject: string;
  body: string;
  created_at: string;
  deadline_at?: string | null;
  delegate_meta?: WireDelegateMetadata | null;
  committee_meta?: WireCommitteeMetadata | null;
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
  delegate_meta?: WireDelegateMetadata | null;
  committee_meta?: WireCommitteeMetadata | null;
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

export type MemoryPayload = MemoryEntry;
export type HandoffPayload = HandoffEntry;
export type RuntimePayload = RuntimeEntry;

/**
 * `StatePayload` in `crates/core/src/state.rs` is an externally tagged enum,
 * so the wire shape is `{ memory: {...} }`, not `{ concern, ... }`.
 */
export type StatePayload =
  | { memory: MemoryEntry }
  | { handoff: HandoffEntry }
  | { runtime: RuntimeEntry };

export interface StateEntry {
  id: string;
  /** Wire scope string: `"workspace" | "project:<slug>" | "agent:<slug>" | ...`. */
  scope: string;
  concern: "memory" | "handoff" | "runtime" | string;
  slug: string;
  sensitive: boolean;
  ever_sensitive: boolean;
  payload: StatePayload;
  head_revision: string;
  updated_at: string;
  by: string;
}

export interface StateEntriesResponse {
  entries: StateEntry[];
}

export interface StateEntryResponse {
  entry: StateEntry;
}

/** Flat `POST /api/v1/state` body (`handle_post_state` also accepts `payload`). */
export interface UpsertStateEntryInput {
  scope?: string;
  slug: string;
  concern: "memory" | "handoff" | "runtime";
  category?: string;
  basis?: string;
  pinned?: boolean;
  sensitive?: boolean;
  summary?: string;
  body?: string;
  notes?: string;
  value?: unknown;
  merge_mode?: string;
  by?: string;
  payload?: StatePayload;
}

export interface StateSearchHit {
  entry: StateEntry;
  score_millis: number;
  matched_terms: string[];
  snippet: string;
  estimated_tokens: number;
}

export interface StateSearchResponse {
  query: string;
  hits: StateSearchHit[];
  total_hits: number;
}

export interface CodeSymbolHit {
  name: string;
  kind: string;
  path: string;
  start_line: number;
  end_line: number;
  signature: string;
  snippet: string;
  estimated_file_tokens: number;
  snippet_tokens: number;
  tokens_saved: number;
}

export interface TokenGainSummary {
  queries_count: number;
  tokens_saved: number;
  files_indexed: number;
  symbols_indexed: number;
}

export type CodeSearchKind = "def" | "search" | "outline";

export interface CodeSearchResponse {
  query: string;
  kind: string;
  symbols: CodeSymbolHit[];
  tokens_saved_this_query: number;
  token_gain: TokenGainSummary;
}

export interface StateCheckpointResponse {
  entry: StateEntry;
  branch: string;
  head_sha: string;
  dirty_files: number;
}

export interface ImportWorkspaceStateResponse {
  imported_memories: number;
  imported_handoffs: number;
  scopes: string[];
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
  token_gain?: TokenGainSummary;
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

// Deliberately excluded from the UI types (secret-name references must never render):
// connector `default_headers`, tool `headers`/`input_schema`, and bridge
// `verify_secret`/`bot_token_secret`/`allowed_user_ids`/`allowed_guild_ids`.
export interface GateConnectorToolSummary {
  name: string;
  description: string;
  method: string;
  path: string;
  path_params: string[];
  query_params: string[];
  body_params: string[];
  response_fields: string[];
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
  title: string;
  version: string;
  description: string;
  base_url: string;
  allowed_hosts: string[];
  enabled: boolean;
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

/**
 * Record #86 (Gate G6): Observational `GET /api/v1/gate/vault-consumers` report
 * (`crates/hub/src/vault_consumers_api.rs`).
 */
export type VaultConsumerSourceClass =
  | "vault-ref"
  | "plaintext-file"
  | "env"
  | "unconfigured"
  | "unknown";

export type VaultAuditState =
  | "ok"
  | "absent"
  | "symlink"
  | "unreadable"
  | "not-a-regular-file"
  | "unverified";

export interface VaultConsumerDeniedInfo {
  outcome: string;
}

export interface VaultConsumerReportEntry {
  id: string;
  label: string;
  class: VaultConsumerSourceClass;
  evidence: string;
  plaintext_path_present: boolean | null;
  last_denied: VaultConsumerDeniedInfo | null;
}

export interface VaultCustodySummary {
  mode: string;
  note: string;
  vault_key_readable_by_group_or_other: boolean | null;
  vault_key_mode: string | null;
}

export interface VaultConsumersAuditSummary {
  state: VaultAuditState | string;
  last_denied: VaultConsumerDeniedInfo | null;
}

export interface VaultConsumersReport {
  schema_version: number;
  disclaimer: string;
  custody: VaultCustodySummary;
  consumers: VaultConsumerReportEntry[];
  audit: VaultConsumersAuditSummary;
}

export interface EnrolledNode {
  node_id: string;
  display_name: string;
  role: "hub" | "worker";
  hostname: string;
  os: string;
  arch: string;
  listen_addr?: string | null;
  capabilities: string[];
  labels: string[];
  status: "online" | "stale" | "offline" | "revoked";
  enrolled_at_ms: number;
  last_seen_at_ms: number;
  environment_kind?: EnvironmentKind;
  federated_subsystems?: string[];
  active_tasks?: number;
  load_avg?: number | null;
  reported_status?: "idle" | "busy" | "draining" | null;
}

export type EnvironmentKind =
  | "ai-lab"
  | "macos-xcode"
  | "cloud-worker"
  | "linux-worker";

export type FederatedTaskStatus = "queued" | "running" | "completed" | "failed";

export interface FederatedTask {
  task_id: string;
  node_id: string;
  kind: string;
  command: string;
  project: string;
  status: FederatedTaskStatus;
  created_at_ms: number;
  updated_at_ms: number;
  output: string;
  exit_code: number | null;
}

export interface DispatchNodeTaskInput {
  command: string;
  kind?: string;
  project?: string;
}

export interface CliRuntimeStatus {
  name: string;
  installed: boolean;
  path: string | null;
  version: string | null;
  error: string | null;
  active_routes: number;
  live_sessions: number;
  probe_ms: number;
  /** Whether the hub can run this CLI's own update command. */
  update_supported?: boolean;
  /** The update command as shown to a person, such as `claude update`. */
  update_command?: string | null;
}

export interface VaultProviderStatus {
  provider: string;
  secret_name: string;
  provisioned: boolean;
  expired: boolean;
  status: "ready" | "expired" | "missing";
  expires_at: string | null;
}

export interface LocalEngineStatus {
  name: string;
  endpoint: string;
  reachable: boolean;
  latency_ms: number | null;
  models: string[];
  error: string | null;
}

export interface RuntimeMatrix {
  generated_at_ms: number;
  cli_runtimes: CliRuntimeStatus[];
  vault_providers: VaultProviderStatus[];
  local_engines: LocalEngineStatus[];
  vault_error: string | null;
  /** Account allowance by adapter id (`claude`, `codex`, `grok`, `antigravity`, `muse`). */
  vendor_quotas?: Record<string, SessionQuotaUsage>;
}

export type RuntimeUpdateStatus =
  | "updated"
  | "unchanged"
  | "unknown_version"
  | "failed"
  | "unsupported"
  | "not_installed"
  | "timeout";

/** Outcome of `POST /api/v1/runtimes/update` for one CLI runtime. */
export interface RuntimeUpdateResult {
  runtime: string;
  ok: boolean;
  version_before: string | null;
  version_after: string | null;
  status: RuntimeUpdateStatus;
  output_tail: string;
}

export interface NodesResponse {
  nodes?: EnrolledNode[];
}

export interface NodeEnrollTokenResponse {
  code: string;
  expires_at_ms: number;
  ttl_secs: number;
  join_command: string;
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
 * FerroWatch (`/api/v1/watch/*`, `crates/watch/src/{lib,alerts,liveness,rollups,runtime}.rs`
 * and `crates/hub/src/watch_api.rs`) + Session Remodel (`/api/v1/sessions/:id/remodel`).
 */
export type WatchSeverity = "info" | "warning" | "critical";

export type WatchAlertRule =
  | "spend"
  | "quota"
  | "heartbeat_stale"
  | "heartbeat_dead"
  | "crash_loop"
  | "transition_regret"
  | string;

export type WatchEntityKind =
  | "workspace"
  | "project"
  | "agent"
  | "vendor"
  | "record"
  | "transition"
  | string;

export interface WatchEntity {
  kind: WatchEntityKind;
  id: string;
  label?: string | null;
}

export type WatchQuotaWindowKind = "primary" | "secondary";

export interface WatchQuotaWindow {
  vendor: string;
  window: WatchQuotaWindowKind;
  used_percent: number;
  resets_at_ms?: number | null;
  session?: string | null;
  window_minutes?: number | null;
  exhausted_reason?: string | null;
}

export interface WatchThresholds {
  warn_quota_pct: number;
  critical_quota_pct: number;
  warn_cost_usd_micros: number;
  critical_cost_usd_micros: number;
  stale_after_ms: number;
  crash_loop_warn_streak?: number;
  crash_loop_critical_streak?: number;
}

export interface WatchThresholdOverrides {
  warn_quota_pct?: number;
  critical_quota_pct?: number;
  warn_cost_usd_micros?: number;
  critical_cost_usd_micros?: number;
  stale_after_ms?: number;
}

export const DEFAULT_WATCH_THRESHOLDS: WatchThresholds = {
  warn_quota_pct: 80,
  critical_quota_pct: 95,
  warn_cost_usd_micros: 100_000_000,
  critical_cost_usd_micros: 500_000_000,
  stale_after_ms: 30_000,
  crash_loop_warn_streak: 3,
  crash_loop_critical_streak: 5,
};

export interface WatchAlert {
  id: string;
  rule: WatchAlertRule;
  severity: WatchSeverity;
  entity: WatchEntity;
  message: string;
  observed: number;
  threshold: number;
  at_ms: number;
  resets_at_ms?: number | null;
  remediation: string;
}

export interface WatchHostedSession {
  session_id: string;
  persona: string;
  project: string;
  role: string;
  vendor: string;
  model?: string | null;
  node_id: string;
  status: string;
  restart_count: number;
  updated_at_ms: number;
}

export type WatchLiveness = "live" | "stale" | "dead" | "exited" | "missing";

export interface WatchAgentHealth {
  agent: string;
  liveness: WatchLiveness;
  active: boolean;
  verdict: LivenessVerdict;
  node?: string | null;
  project?: string | null;
  adapter?: string | null;
  attempt_id?: string | null;
  record?: string | null;
  heartbeat_at_ms?: number | null;
  heartbeat_age_ms?: number | null;
  hosted_session?: WatchHostedSession | null;
}

export interface WatchLivenessCounts {
  agents: number;
  active: number;
  live: number;
  stale: number;
  dead: number;
  exited: number;
  missing: number;
}

export interface WatchGroupHealth extends WatchLivenessCounts {
  key: string;
}

export interface WatchHealthReport {
  now_ms: number;
  stale_after_ms: number;
  local_host: string;
  agents: WatchAgentHealth[];
  totals: WatchLivenessCounts;
  by_node: WatchGroupHealth[];
  by_project: WatchGroupHealth[];
  hosted_sessions: number;
  hosted_sessions_undecodable: number;
  heartbeats_undecodable: number;
}

export interface WatchAttemptCounts {
  attempts: number;
  succeeded: number;
  failed: number;
  crashed: number;
  timed_out: number;
  tokens_in: number;
  tokens_out: number;
  cost_usd_micros: number;
  duration_ms: number;
}

export interface WatchProjectRollup extends WatchAttemptCounts {
  project: string;
  slug: string;
  root: string;
}

export interface WatchBreakdown extends WatchAttemptCounts {
  key: string;
}

export interface WatchGateTotals {
  pass: number;
  fail: number;
  error: number;
  duration_ms: number;
  cost_usd_micros: number;
}

export interface WatchFailureStreak {
  record: string;
  project: string;
  slug: string;
  attempt_streak: number;
  gate_streak: number;
  gate?: string | null;
  gate_name?: string | null;
  last_attempt_status?: string | null;
  last_gate_verdict?: string | null;
}

export interface WatchRegretSummary {
  projects_audited: number;
  challenges: Array<Record<string, unknown>>;
}

export interface WatchTelemetryReport {
  project: string | null;
  source: string;
  totals: WatchAttemptCounts;
  gate_runs: WatchGateTotals;
  by_project: WatchProjectRollup[];
  by_adapter: WatchBreakdown[];
  by_agent: WatchBreakdown[];
  regret: WatchRegretSummary;
  failure_streaks: WatchFailureStreak[];
}

export interface WatchSummaryResponse {
  telemetry: WatchTelemetryReport;
  health: WatchHealthReport;
  quotas: WatchQuotaWindow[];
  alerts: WatchAlert[];
  thresholds: WatchThresholds;
  mq_tier_telemetry?: { topics: Array<{
    topic: string;
    active_tier: string;
    memory_messages: number;
    memory_bytes: number;
    spilled_messages_total: number;
    acked_in_memory_total: number;
    available_ram_bytes: number | null;
    free_disk_bytes: number | null;
  }> } | null;
}

export interface WatchAlertsResponse {
  alerts: WatchAlert[];
  thresholds: WatchThresholds;
}

export interface WatchRollupsResponse {
  telemetry: WatchTelemetryReport;
}

export type RemodelMode = "prepare" | "execute";

export interface RemodelSessionRequest {
  vendor: string;
  model?: string | null;
  mode?: RemodelMode;
  cwd?: string | null;
  node_id?: string | null;
  hosted?: boolean;
}

export interface RemodelSessionResponse {
  mode: RemodelMode;
  session_id: string;
  previous_session_id: string;
  persona: string;
  project: string;
  role: string;
  from_vendor: string;
  vendor: string;
  model: string | null;
  node_id: string;
  hosted: boolean;
  cwd: string | null;
  status: string;
  handshake_dispatched: boolean;
  hydration_dispatched: boolean;
  runtime_stopped: boolean;
  tmux_target: string | null;
  prompt: string | null;
  handoff: string | null;
  warnings: string[];
}

export interface WatchBalancePolicy {
  enabled: boolean;
  primary_trigger_pct: number;
  secondary_trigger_pct: number;
  target_max_primary_pct: number;
  target_max_secondary_pct: number;
  cooldown_ms: number;
  min_interval_ms: number;
  max_switches_per_run: number;
  pinned_personas: string[];
  excluded_personas: string[];
  allowed_target_vendors: string[];
  excluded_target_vendors: string[];
  allow_unknown_quota_targets: boolean;
}

export const DEFAULT_WATCH_BALANCE_POLICY: WatchBalancePolicy = {
  enabled: false,
  primary_trigger_pct: 90,
  secondary_trigger_pct: 85,
  target_max_primary_pct: 70,
  target_max_secondary_pct: 70,
  cooldown_ms: 1_800_000,
  min_interval_ms: 300_000,
  max_switches_per_run: 1,
  pinned_personas: [],
  excluded_personas: [],
  allowed_target_vendors: [],
  excluded_target_vendors: [],
  allow_unknown_quota_targets: false,
};

export interface WatchBalanceStatusResponse {
  policy: WatchBalancePolicy;
  enabled: boolean;
}

export interface WatchBalanceSwitchItem {
  persona: string;
  session_id: string;
  project?: string;
  from_vendor: string;
  to_vendor: string;
  reason: string;
  trigger: "exhausted" | "primary" | "secondary" | string;
  target_score?: number | null;
  target_status?: "exhausted" | "known" | "unknown" | string;
}

export type WatchBalanceSkipReason =
  | "pinned"
  | "excluded"
  | "healthy"
  | "no_reading"
  | "not_remodelable"
  | "cooldown"
  | "persona_switched"
  | "no_eligible_target"
  | "run_limit"
  | string;

export interface WatchBalanceSkippedItem {
  persona: string;
  session_id?: string;
  vendor?: string;
  reason: WatchBalanceSkipReason;
  detail?: string;
}

export interface WatchBalanceVendorWindow {
  used_percent: number;
  resets_at_ms?: number | null;
  window_minutes?: number | null;
  readings?: number;
}

export interface WatchBalanceVendorHealth {
  vendor: string;
  status?: "exhausted" | "known" | "unknown" | string;
  primary?: WatchBalanceVendorWindow | null;
  secondary?: WatchBalanceVendorWindow | null;
  primary_window?: {
    used_percent: number;
    resets_at_ms?: number | null;
  } | null;
  secondary_window?: {
    used_percent: number;
    resets_at_ms?: number | null;
  } | null;
  exhausted?: boolean;
  exhausted_reason?: string | null;
  score?: number | null;
  candidate?: boolean;
  eligible?: boolean;
  ineligible_reason?: string | null;
  unknown?: boolean;
}

export interface WatchBalancePlan {
  evaluated_at_ms: number;
  policy?: WatchBalancePolicy;
  blocked: string | null;
  switches: WatchBalanceSwitchItem[];
  skipped: WatchBalanceSkippedItem[];
  vendors: WatchBalanceVendorHealth[];
}

export type WatchBalanceSwitchStatus = "ok" | "failed" | "refused" | "not_run";

export interface WatchBalanceApplyResultItem {
  persona: string;
  session_id?: string | null;
  from_vendor: string;
  to_vendor: string;
  status?: WatchBalanceSwitchStatus;
  ok?: boolean;
  new_session_id?: string | null;
  outcome?: "remodeled" | "restored" | "down" | string | null;
  restored_session_id?: string | null;
  error?: string | null;
}

export interface WatchBalanceApplyResponse {
  plan: WatchBalancePlan;
  results: WatchBalanceApplyResultItem[];
  audit_slug?: string | null;
  warnings?: string[];
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
