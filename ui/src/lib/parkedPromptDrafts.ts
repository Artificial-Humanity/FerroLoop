import type { PromptAttachment, SavedPromptAttachment } from "../types/api";

export interface ParkedPromptDraft {
  id: string;
  sessionId: string;
  epoch: string;
  text: string;
  queuedText: string;
  files: Pick<SavedPromptAttachment, "name" | "media_type" | "url">[];
  recovery?: "hub_restarted" | "session_replaced" | "refused";
  /** Original files remain available in this tab. A reload uses the saved Hub files. */
  attachments?: PromptAttachment[];
}

export function parkedPromptDraftScope(hub: string, owner: string, seat: string): string {
  return `ferrodeck.parked.${encodeURIComponent(JSON.stringify([hub, owner, seat]))}.`;
}

const memory = new Map<string, ParkedPromptDraft>();

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
  if (remove) memory.delete(key);
  else memory.set(key, draft);
  try {
    if (remove) window.localStorage.removeItem(key);
    else {
      const { attachments: _attachments, ...saved } = draft;
      window.localStorage.setItem(key, JSON.stringify(saved));
    }
  } catch {
    // Keep the in-tab copy if the browser cannot save the backup.
  }
}
