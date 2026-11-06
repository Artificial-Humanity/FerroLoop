import { HubApiError } from "../api/client";

export interface TranscriptReadOwner {
  authEpoch: number;
  selectionEpoch: number;
  agentKey: string;
  sessionId: string | null;
  resetEpoch: number;
}

export interface TranscriptReadTicket {
  readonly owner: Readonly<TranscriptReadOwner>;
  readonly sequence: number;
}

const ownerKey = (owner: TranscriptReadOwner) => JSON.stringify([
  owner.authEpoch, owner.selectionEpoch, owner.agentKey, owner.sessionId, owner.resetEpoch,
]);

/** Local to one reader: every completion, including catch/finally, uses the same fence. */
export function createTranscriptReadGuard() {
  let sequence = 0;
  const latest = new Map<string, TranscriptReadTicket>();
  return {
    begin(owner: TranscriptReadOwner): TranscriptReadTicket {
      const ticket = { owner: { ...owner }, sequence: ++sequence };
      latest.set(ownerKey(owner), ticket);
      return ticket;
    },
    accepts(ticket: TranscriptReadTicket, currentOwner: TranscriptReadOwner): boolean {
      const key = ownerKey(ticket.owner);
      return key === ownerKey(currentOwner) && latest.get(key) === ticket;
    },
    invalidate(): void { latest.clear(); },
  };
}

export function isTransientTranscriptReadError(error: unknown): boolean {
  if (error instanceof HubApiError) {
    if (error.status === 401 || error.status === 403) return false;
    return error.status === 503 && error.details?.code === "transcript_unavailable";
  }
  // Fetch and body readers reject with ordinary Error/TypeError/DOMException.
  return error instanceof Error;
}
