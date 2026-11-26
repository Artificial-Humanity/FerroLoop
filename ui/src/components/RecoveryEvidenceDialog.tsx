import { useEffect, useId, useRef, useState } from "react";
import type { RefObject } from "react";
import { HubApiError, type HubApiClient } from "../api/client";
import type { HostedRecoveryEvidenceResponse } from "../types/api";
import {
  ALERT_STYLE,
  BACKDROP_STYLE,
  DIALOG_STYLE,
  trapTabKey,
  useDialogBehavior,
} from "./settings/projectModalShared";

interface RecoveryEvidenceDialogProps {
  sessionId: string;
  apiClient: Pick<HubApiClient, "getHostedRecoveryEvidence">;
  returnFocusTo: RefObject<HTMLElement | null>;
  onClose: () => void;
}

type ReadState =
  | { status: "loading" }
  | { status: "loaded"; evidence: HostedRecoveryEvidenceResponse["evidence"] }
  | { status: "error"; message: string };

export function RecoveryEvidenceDialog({
  sessionId,
  apiClient,
  returnFocusTo,
  onClose,
}: RecoveryEvidenceDialogProps) {
  const titleId = useId();
  const dialogRef = useRef<HTMLDivElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);
  const busyRef = useRef(false); // An in-flight read can always be dismissed.
  const [read, setRead] = useState<ReadState>({ status: "loading" });

  useDialogBehavior({
    getInitialFocus: () => closeRef.current,
    returnFocusTo,
    busyRef,
    onClose,
  });

  useEffect(() => {
    const controller = new AbortController();
    setRead({ status: "loading" });
    void (async () => {
      try {
        const response = await apiClient.getHostedRecoveryEvidence(sessionId, controller.signal);
        if (controller.signal.aborted) return;
        if (response.session_id !== sessionId) throw new Error("Mismatched recovery session");
        setRead({ status: "loaded", evidence: response.evidence });
      } catch (error) {
        if (controller.signal.aborted) return;
        const message = error instanceof HubApiError && error.status === 404
          ? "This recovery incident is no longer available."
          : "Could not read recovery evidence.";
        setRead({ status: "error", message });
      }
    })();
    return () => controller.abort();
  }, [apiClient, sessionId]);

  const evidence = read.status === "loaded" ? read.evidence : null;
  const exit = evidence?.exit_code != null
    ? `Exit code ${evidence.exit_code}`
    : evidence?.signal != null ? `Signal ${evidence.signal}` : "Exit status unavailable";

  return (
    <div style={BACKDROP_STYLE} onClick={(event) => {
      if (event.target === event.currentTarget) onClose();
    }}>
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        ref={dialogRef}
        style={DIALOG_STYLE}
        onKeyDown={(event) => trapTabKey(event, dialogRef.current)}
      >
        <header style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: 12 }}>
          <h2 id={titleId} style={{ fontSize: 16, margin: 0 }}>Hosted recovery evidence</h2>
          <button ref={closeRef} type="button" className="fd-btn" onClick={onClose}>Close</button>
        </header>
        <p style={{ margin: 0, overflowWrap: "anywhere" }}>{sessionId}</p>
        {read.status === "loading" && <p role="status">Loading recovery evidence…</p>}
        {read.status === "error" && <p role="alert" style={ALERT_STYLE}>{read.message}</p>}
        {read.status === "loaded" && !evidence && <p>No exit evidence was captured.</p>}
        {evidence && (
          <>
            <p style={{ margin: 0 }}>Latest available exit · {exit}</p>
            {evidence.terminal_tail ? (
              <pre
                tabIndex={0}
                aria-label="Sanitized terminal excerpt"
                style={{
                  margin: 0,
                  padding: 10,
                  maxHeight: "50dvh",
                  overflow: "auto",
                  whiteSpace: "pre-wrap",
                  overflowWrap: "anywhere",
                  background: "var(--bg-elevated)",
                  borderRadius: "var(--radius-sm)",
                }}
              >{evidence.terminal_tail}</pre>
            ) : <p>No terminal output was captured.</p>}
          </>
        )}
      </div>
    </div>
  );
}
