import type { PromptAttachment, SavedPromptAttachment } from "../types/api";

export interface ParkedPromptDraft {
  id: string;
  sessionId: string;
  epoch: string;
  text: string;
  queuedText: string;
  files: Pick<SavedPromptAttachment, "name" | "media_type" | "url">[];
  recovery?: "hub_restarted" | "session_replaced" | "refused" | "submission_refused";
  /** A refused POST may have no Hub file references to recover. */
  attachmentCount?: number;
  attachments?: PromptAttachment[];
}

export function parkedPromptDraftScope(hub: string, owner: string, seat: string): string {
  return `ferrodeck.parked.${encodeURIComponent(JSON.stringify([hub, owner, seat]))}.`;
}

const memory = new Map<string, ParkedPromptDraft>();
// Retain at most one maximum-size request's worth of encoded, unsaved uploads.
const MAX_REFUSED_ATTACHMENT_CHARS = 24 * 1024 * 1024;

function attachmentChars(attachments: PromptAttachment[] = []): number {
  return attachments.reduce((count, file) => count + file.data_base64.length, 0);
}

export function readParkedPromptDrafts(scope: string): ParkedPromptDraft[] {
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
  return [...drafts.values()];
}

export function writeParkedPromptDraft(scope: string, draft: ParkedPromptDraft, remove = false): void {
  const key = `${scope}${draft.epoch}.${draft.id}`;
  const { attachments, ...saved } = draft;
  memory.delete(key);
  if (!remove) {
    let keepAttachments = false;
    if (attachments?.length) {
      let chars = attachmentChars(attachments);
      for (const other of memory.values()) chars += attachmentChars(other.attachments);
      keepAttachments = chars <= MAX_REFUSED_ATTACHMENT_CHARS;
    }
    memory.set(key, keepAttachments ? draft : saved);
  }
  try {
    if (remove) window.localStorage.removeItem(key);
    else window.localStorage.setItem(key, JSON.stringify(saved));
  } catch {
    // Keep the in-tab copy if the browser cannot save the backup.
  }
}
