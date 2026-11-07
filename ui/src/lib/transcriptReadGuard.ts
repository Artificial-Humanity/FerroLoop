import { HubApiError } from "../api/client";
import type { TranscriptTurn } from "../types/api";

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

/** A malformed successful body is an unavailable observation, never an empty transcript. */
export function isObservedTranscriptTurns(value: unknown): value is TranscriptTurn[] {
  return Array.isArray(value) && value.every((turn) => turn &&
    typeof turn.index === "number" && typeof turn.role === "string" && typeof turn.content === "string" &&
    (turn.thinking == null || typeof turn.thinking === "string") &&
    (turn.created_at == null || typeof turn.created_at === "string") &&
    (turn.tool_calls == null || (Array.isArray(turn.tool_calls) && turn.tool_calls.every(
      (call: unknown) => call !== null && typeof call === "object" && "name" in call && typeof call.name === "string",
    ))));
}
