import { describe, expect, it } from "vitest";
import { HubApiError } from "../api/client";
import { createTranscriptReadGuard, isTransientTranscriptReadError, type TranscriptReadOwner } from "../lib/transcriptReadGuard";

const owner: TranscriptReadOwner = { authEpoch: 1, selectionEpoch: 1, agentKey: "freya", sessionId: "session-a", resetEpoch: 0 };
describe("transcript read fencing", () => {
  it.each(["success", "error", "finally", "newer failure then older success"])("rejects an older %s after a newer read starts", () => {
    const guard = createTranscriptReadGuard();
    const old = guard.begin(owner);
    const latest = guard.begin(owner);
    expect(guard.accepts(latest, owner)).toBe(true);
    expect(guard.accepts(old, owner)).toBe(false);
  });
  it("invalidates all outstanding tickets without affecting a subsequent read", () => {
    const guard = createTranscriptReadGuard();
    const old = guard.begin(owner);
    guard.invalidate();
    expect(guard.accepts(old, owner)).toBe(false);
    const next = guard.begin(owner);
    expect(next.sequence).toBeGreaterThan(old.sequence);
    expect(guard.accepts(next, owner)).toBe(true);
    expect(guard.accepts(old, owner)).toBe(false);
  });
  it.each([{ authEpoch: 2 }, { resetEpoch: 1 }, { selectionEpoch: 3 }, { agentKey: "felix" }, { sessionId: null }])("rejects changed ownership %j, including A to B to A", (change) => {
    const guard = createTranscriptReadGuard();
    expect(guard.accepts(guard.begin(owner), { ...owner, ...change })).toBe(false);
  });
  it("does not accept a ticket issued by another reader", () => {
    const first = createTranscriptReadGuard();
    const second = createTranscriptReadGuard();
    const foreign = first.begin(owner);
    second.begin(owner);
    expect(second.accepts(foreign, owner)).toBe(false);
  });
  it("copies ownership at dispatch and keeps guard instances isolated", () => {
    const guard = createTranscriptReadGuard();
    const mutable = { ...owner };
    const ticket = guard.begin(mutable);
    mutable.authEpoch++;
    expect(guard.accepts(ticket, owner)).toBe(true);
    expect(guard.accepts(ticket, mutable)).toBe(false);
    const other = createTranscriptReadGuard();
    other.invalidate();
    expect(guard.accepts(ticket, owner)).toBe(true);
  });
});
describe("transcript error classification", () => {
  it.each([401, 403, 400, 404, 409, 500, 503])("does not treat HTTP %s refusals as stale success", (status) => {
    expect(isTransientTranscriptReadError(new HubApiError(status, "refused"))).toBe(false);
  });
  it.each([new TypeError("Failed to fetch"), new Error("body read failed"), new HubApiError(503, "unavailable", undefined, undefined, { code: "transcript_unavailable" })])("recognizes read unavailability %s", (error) => {
    expect(isTransientTranscriptReadError(error)).toBe(true);
  });
  it.each([401, 403])("prioritizes denied status %s over unavailable code", (status) => {
    expect(isTransientTranscriptReadError(new HubApiError(status, "denied", undefined, undefined, { code: "transcript_unavailable" }))).toBe(false);
  });
});
