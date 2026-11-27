import type { PromptAttachment, SavedPromptAttachment } from "../types/api";

export interface ParkedPromptDraft {
  id: string;
  sessionId: string;
  epoch: string;
  text: string;
  queuedText: string;
  files: Pick<SavedPromptAttachment, "name" | "media_type" | "url">[];
  recovery?: "hub_restarted" | "session_replaced" | "refused" | "submission_refused" | "submission_unconfirmed";
  /** A POST without a queue receipt has no Hub file references to recover. */
  attachmentCount?: number;
  attachments?: PromptAttachment[];
  savedAt?: number;
  /** View-only metadata carried by the current Hub snapshot. */
  retainedByHub?: true;
  refusalDetail?: string | null;
}

const PARKED_DRAFT_PREFIX = "ferrodeck.parked.";
const MAX_RECOVERY_DRAFTS = 128;

export function parkedPromptDraftScope(hub: string, owner: string, seat: string): string {
  return `${PARKED_DRAFT_PREFIX}${encodeURIComponent(JSON.stringify([hub, owner, seat]))}.`;
}

const memory = new Map<string, ParkedPromptDraft>();
// Retain at most one maximum-size request's worth of encoded, unsaved uploads.
const MAX_UNSAVED_ATTACHMENT_CHARS = 24 * 1024 * 1024;

function attachmentChars(attachments: PromptAttachment[] = []): number {
  return attachments.reduce((count, file) => count + file.data_base64.length, 0);
}

function readParkedPromptDraftEntries(scope: string): Map<string, ParkedPromptDraft> {
  const drafts = new Map<string, ParkedPromptDraft>();
  try {
    for (let index = 0; index < window.localStorage.length; index++) {
      const key = window.localStorage.key(index);
      if (key?.startsWith(scope)) {
        const value = window.localStorage.getItem(key);
        if (!value) continue;
        try {
          const draft = JSON.parse(value) as ParkedPromptDraft | null;
          if (draft?.id && draft.epoch && draft.sessionId && typeof draft.text === "string" && Array.isArray(draft.files)) {
            drafts.set(key, draft);
          }
        } catch {
          // One unreadable backup must not hide any of the other drafts.
        }
      }
    }
  } catch {
    // The in-tab copy remains available when browser storage is disabled.
  }
  for (const [key, draft] of memory) {
    if (key.startsWith(scope)) drafts.set(key, draft);
  }
  return drafts;
}

export function readParkedPromptDrafts(scope: string): ParkedPromptDraft[] {
  return [...readParkedPromptDraftEntries(scope).values()];
}

function removeParkedDraft(key: string): void {
  memory.delete(key);
  try {
    window.localStorage.removeItem(key);
  } catch {
    // Browser storage may be disabled.
  }
}

export function clearParkedPromptDrafts(): void {
  for (const key of memory.keys()) {
    if (key.startsWith(PARKED_DRAFT_PREFIX)) memory.delete(key);
  }
  try {
    for (let index = window.localStorage.length - 1; index >= 0; index--) {
      const key = window.localStorage.key(index);
      if (key?.startsWith(PARKED_DRAFT_PREFIX)) window.localStorage.removeItem(key);
    }
  } catch {
    // Browser storage may be disabled.
  }
}

export function writeParkedPromptDraft(scope: string, draft: ParkedPromptDraft, remove = false): void {
  const key = `${scope}${draft.epoch}.${draft.id}`;
  if (remove) {
    removeParkedDraft(key);
    return;
  }
  const { attachments, ...saved } = { ...draft, savedAt: Date.now() };
  memory.delete(key);
  let keepAttachments = false;
  if (attachments?.length) {
    let chars = attachmentChars(attachments);
    for (const other of memory.values()) chars += attachmentChars(other.attachments);
    keepAttachments = chars <= MAX_UNSAVED_ATTACHMENT_CHARS;
  }
  memory.set(key, keepAttachments ? { ...saved, attachments } : saved);
  try {
    window.localStorage.setItem(key, JSON.stringify(saved));
  } catch {
    // Keep the in-tab copy if the browser cannot save the backup.
  }
  if (saved.recovery) {
    const recovered = [...readParkedPromptDraftEntries(PARKED_DRAFT_PREFIX)]
      .filter(([, entry]) => entry.recovery)
      .sort(([, left], [, right]) => (left.savedAt ?? 0) - (right.savedAt ?? 0));
    for (const [oldest] of recovered.slice(0, Math.max(0, recovered.length - MAX_RECOVERY_DRAFTS))) {
      removeParkedDraft(oldest);
    }
  }
}
