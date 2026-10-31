import React, { useCallback, useEffect, useMemo, useState } from "react";
import type { HubApiClient } from "../api/client";
import type {
  CodeSearchKind,
  CodeSymbolHit,
  HandoffEntry,
  HubAuthority,
  ImportWorkspaceStateResponse,
  MemoryEntry,
  RuntimeEntry,
  StateCheckpointResponse,
  StateEntry,
  StateEntryResponse,
  StateSearchHit,
  TokenGainSummary,
  UpsertStateEntryInput,
} from "../types/api";

export interface FerroStatePaneProps {
  entries?: StateEntry[];
  authority?: HubAuthority | null;
  loading?: boolean;
  error?: string | null;
  onRefresh?: () => void;
  onUpsertEntry?: (input: UpsertStateEntryInput) => Promise<StateEntryResponse> | void;
  onCreateCheckpoint?: () => Promise<StateCheckpointResponse> | void;
  onImportWorkspace?: () => Promise<ImportWorkspaceStateResponse> | void;
  apiClient?: HubApiClient;
}

/** Mirrors `validate_slug` in `crates/core/src/state.rs` (server lowercases). */
const SLUG_PATTERN = /^[a-z][a-z0-9_-]{0,63}$/;
/** `MAX_SUMMARY_CHARS` in `crates/core/src/state.rs`. */
const MAX_SUMMARY_CHARS = 240;

const MEMORY_CATEGORIES = ["note", "rule", "decision", "warning"] as const;
const MEMORY_BASES = ["inference", "ruling", "measured"] as const;

const noAutofill = {
  autoComplete: "off",
  autoCorrect: "off",
  autoCapitalize: "off",
  spellCheck: false,
  "data-1p-ignore": "true",
  "data-lpignore": "true",
  "data-form-type": "other",
} as const;

const inputStyle: React.CSSProperties = {
  width: "100%",
  // 16px stops iPadOS Safari from zooming the viewport on focus.
  fontSize: "16px",
  minHeight: "40px",
};

const labelStyle: React.CSSProperties = {
  display: "flex",
  flexDirection: "column",
  gap: "4px",
  fontSize: "12px",
  fontWeight: 600,
  color: "var(--text-secondary)",
};

function errorMessage(err: unknown, fallback: string): string {
  return err instanceof Error ? err.message : fallback;
}

function formatCount(value: number): string {
  return value.toLocaleString("en-US");
}

function replaceEntry(list: StateEntry[], saved: StateEntry): StateEntry[] {
  const rest = list.filter(
    (entry) =>
      entry.id !== saved.id &&
      !(
        entry.scope === saved.scope &&
        entry.concern === saved.concern &&
        entry.slug === saved.slug
      ),
  );
  return [...rest, saved];
}

function parseLines(raw: string): string[] {
  return raw
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

type ResolvedPayload =
  | { kind: "memory"; data: MemoryEntry }
  | { kind: "handoff"; data: HandoffEntry }
  | { kind: "runtime"; data: RuntimeEntry };

/**
 * Unwrap the externally tagged `StatePayload` (`{ memory: {...} }`).
 * Also tolerates an internally tagged `{ concern: "memory", ... }` object so
 * the pane keeps rendering if the hub payload shape is ever flattened.
 */
function resolvePayload(entry: StateEntry): ResolvedPayload | null {
  const payload = entry.payload as unknown as Record<string, unknown> | null;
  if (!payload || typeof payload !== "object") {
    return null;
  }
  if (payload.memory && typeof payload.memory === "object") {
    return { kind: "memory", data: payload.memory as MemoryEntry };
  }
  if (payload.handoff && typeof payload.handoff === "object") {
    return { kind: "handoff", data: payload.handoff as HandoffEntry };
  }
  if (payload.runtime && typeof payload.runtime === "object") {
    return { kind: "runtime", data: payload.runtime as RuntimeEntry };
  }
  if (payload.concern === "memory") {
    return { kind: "memory", data: payload as unknown as MemoryEntry };
  }
  if (payload.concern === "handoff") {
    return { kind: "handoff", data: payload as unknown as HandoffEntry };
  }
  if (payload.concern === "runtime") {
    return { kind: "runtime", data: payload as unknown as RuntimeEntry };
  }
  return null;
}

export const FerroStatePane: React.FC<FerroStatePaneProps> = ({
  entries,
  authority = null,
  loading = false,
  error = null,
  onRefresh,
  onUpsertEntry,
  onCreateCheckpoint,
  onImportWorkspace,
  apiClient,
}) => {
  const isOwner = authority?.tier === "owner";

  const [subView, setSubView] = useState<"explorer" | "code">("explorer");
  const [internalEntries, setInternalEntries] = useState<StateEntry[]>([]);
  const [unfilteredEntries, setUnfilteredEntries] = useState<StateEntry[]>(() => entries ?? []);
  const [hits, setHits] = useState<StateSearchHit[] | null>(null);
  const [internalLoading, setInternalLoading] = useState(false);
  const [internalError, setInternalError] = useState<string | null>(null);
  const [actionMessage, setActionMessage] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [seenScopes, setSeenScopes] = useState<string[]>([]);
  const [tokenGain, setTokenGain] = useState<TokenGainSummary | null>(null);

  const [query, setQuery] = useState("");
  const [scopeFilter, setScopeFilter] = useState("");
  const [concernFilter, setConcernFilter] = useState("");

  const [busyCheckpoint, setBusyCheckpoint] = useState(false);
  const [pendingImport, setPendingImport] = useState(false);
  const [busyImport, setBusyImport] = useState(false);

  // Upsert form state
  const [formScope, setFormScope] = useState("workspace");
  const [formSlug, setFormSlug] = useState("");
  const [formConcern, setFormConcern] = useState<"memory" | "handoff">("memory");
  const [formCategory, setFormCategory] = useState<string>("note");
  const [formBasis, setFormBasis] = useState<string>("inference");
  const [formSummary, setFormSummary] = useState("");
  const [formBody, setFormBody] = useState("");
  const [formPinned, setFormPinned] = useState(false);
  const [formSensitive, setFormSensitive] = useState(false);
  const [formShipped, setFormShipped] = useState("");
  const [formInFlight, setFormInFlight] = useState("");
  const [formBlockers, setFormBlockers] = useState("");
  const [formNextSteps, setFormNextSteps] = useState("");
  const [formNotes, setFormNotes] = useState("");
  const [saving, setSaving] = useState(false);

  // Code index state
  const [codeKind, setCodeKind] = useState<CodeSearchKind>("def");
  const [codeQuery, setCodeQuery] = useState("");
  const [symbols, setSymbols] = useState<CodeSymbolHit[] | null>(null);
  const [tokensSavedThisQuery, setTokensSavedThisQuery] = useState<number | null>(null);
  const [codeLoading, setCodeLoading] = useState(false);

  const effectiveEntries = entries ?? internalEntries;
  const effectiveLoading = loading || internalLoading;
  const effectiveError = error ?? internalError;

  const noteScopes = useCallback((list: StateEntry[]) => {
    setSeenScopes((prev) => {
      const next = new Set(prev);
      for (const entry of list) {
        if (entry.scope) {
          next.add(entry.scope);
        }
      }
      return [...next].sort();
    });
  }, []);

  useEffect(() => {
    if (entries) {
      noteScopes(entries);
      setUnfilteredEntries(entries);
    }
  }, [entries, noteScopes]);

  const explorerSeqRef = React.useRef(0);
  const codeSeqRef = React.useRef(0);

  const runExplorer = useCallback(
    async (q: string, scope: string, concern: string) => {
      if (!apiClient) {
        return;
      }
      // Last request wins: responses can arrive out of order, and a slow
      // earlier query must not overwrite (or clear the spinner of) a later one.
      const seq = ++explorerSeqRef.current;
      setInternalLoading(true);
      setInternalError(null);
      try {
        if (q.trim().length > 0) {
          const res = await apiClient.searchStateEntries({
            q: q.trim(),
            scope: scope || undefined,
            concern: concern || undefined,
            limit: 50,
          });
          if (seq !== explorerSeqRef.current) return;
          setHits(res.hits ?? []);
          noteScopes((res.hits ?? []).map((hit) => hit.entry));
        } else {
          const res = await apiClient.listStateEntries({
            scope: scope || undefined,
            concern: concern || undefined,
          });
          if (seq !== explorerSeqRef.current) return;
          setHits(null);
          if (entries === undefined) {
            setInternalEntries(res.entries ?? []);
          }
          if (!scope && !concern) {
            setUnfilteredEntries(res.entries ?? []);
          }
          noteScopes(res.entries ?? []);
        }
      } catch (err) {
        if (seq === explorerSeqRef.current) {
          setInternalError(errorMessage(err, "Failed to load FerroState entries"));
        }
      } finally {
        if (seq === explorerSeqRef.current) {
          setInternalLoading(false);
        }
      }
    },
    [apiClient, entries, noteScopes],
  );

  useEffect(() => {
    if (entries === undefined && apiClient) {
      void runExplorer("", "", "");
    }
    // Initial load only; later loads run through explicit user actions.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [apiClient, entries]);

  const handleExplorerSubmit = (event: React.FormEvent) => {
    event.preventDefault();
    setActionError(null);
    if (apiClient) {
      void runExplorer(query, scopeFilter, concernFilter);
    } else if (onRefresh) {
      onRefresh();
    }
  };

  const handleClearSearch = () => {
    setQuery("");
    setHits(null);
    if (apiClient) {
      void runExplorer("", scopeFilter, concernFilter);
    }
  };

  const handleFilterChange = (scope: string, concern: string) => {
    setScopeFilter(scope);
    setConcernFilter(concern);
    if (apiClient) {
      void runExplorer(query, scope, concern);
    }
  };

  const displayEntries = useMemo(() => {
    if (hits) {
      return hits.map((hit) => ({ entry: hit.entry, hit }));
    }
    return effectiveEntries
      .filter((entry) => !scopeFilter || entry.scope === scopeFilter)
      .filter((entry) => !concernFilter || entry.concern === concernFilter)
      .map((entry) => ({ entry, hit: undefined as StateSearchHit | undefined }));
  }, [hits, effectiveEntries, scopeFilter, concernFilter]);

  const memoryCount = useMemo(
    () => unfilteredEntries.filter((entry) => entry.concern === "memory").length,
    [unfilteredEntries],
  );
  const handoffCount = useMemo(
    () => unfilteredEntries.filter((entry) => entry.concern === "handoff").length,
    [unfilteredEntries],
  );

  const scopeOptions = useMemo(() => {
    const options = new Set<string>(["workspace", "project:ferroloop", ...seenScopes]);
    return [...options].sort();
  }, [seenScopes]);

  const handleUpsert = async (event: React.FormEvent) => {
    event.preventDefault();
    setActionError(null);
    setActionMessage(null);

    const slug = formSlug.trim().toLowerCase();
    if (!SLUG_PATTERN.test(slug)) {
      setActionError(
        "Entry slug must start with a lowercase letter and use only a-z, 0-9, -, or _ (max 64 characters).",
      );
      return;
    }
    if (slug.startsWith("hub-pairing-") || slug.startsWith("hub-session-")) {
      setActionError("Entry slug uses a reserved hub prefix.");
      return;
    }
    const summary = formSummary.trim();
    if (!summary || summary.length > MAX_SUMMARY_CHARS) {
      setActionError(`Summary must be 1 to ${MAX_SUMMARY_CHARS} characters.`);
      return;
    }

    const input: UpsertStateEntryInput =
      formConcern === "memory"
        ? {
            scope: formScope,
            slug,
            concern: "memory",
            category: formCategory,
            basis: formBasis,
            pinned: formPinned,
            ...(formSensitive ? { sensitive: true } : {}),
            summary,
            body: formBody,
          }
        : {
            scope: formScope,
            slug,
            concern: "handoff",
            ...(formSensitive ? { sensitive: true } : {}),
            summary,
            payload: {
              handoff: {
                summary,
                shipped: parseLines(formShipped),
                in_flight: parseLines(formInFlight),
                blockers: parseLines(formBlockers),
                next_steps: parseLines(formNextSteps),
                notes: formNotes.trim() ? formNotes.trim() : null,
              },
            },
          };

    setSaving(true);
    try {
      let saved: StateEntry | undefined;
      if (onUpsertEntry) {
        saved = (await onUpsertEntry(input))?.entry;
      } else if (apiClient) {
        saved = (await apiClient.upsertStateEntry(input)).entry;
      } else {
        throw new Error("No FerroState API client is configured.");
      }
      setHits(null);
      setQuery("");
      if (saved) {
        setUnfilteredEntries((prev) => replaceEntry(prev, saved));
      }
      if (entries === undefined && saved) {
        setInternalEntries((prev) => replaceEntry(prev, saved));
        noteScopes([saved]);
      } else if (entries === undefined) {
        void runExplorer("", scopeFilter, concernFilter);
      } else if (!onUpsertEntry) {
        onRefresh?.();
      }
      setFormSummary("");
      setFormBody("");
      setFormShipped("");
      setFormInFlight("");
      setFormBlockers("");
      setFormNextSteps("");
      setFormNotes("");
      setActionMessage(`Entry ${slug} saved.`);
    } catch (err) {
      setActionError(errorMessage(err, "Failed to save entry"));
    } finally {
      setSaving(false);
    }
  };

  const handleCheckpoint = async () => {
    setActionError(null);
    setActionMessage(null);
    setBusyCheckpoint(true);
    try {
      let res: StateCheckpointResponse | undefined;
      if (onCreateCheckpoint) {
        res = (await onCreateCheckpoint()) ?? undefined;
      } else if (apiClient) {
        res = await apiClient.createStateCheckpoint({});
      } else {
        throw new Error("No FerroState API client is configured.");
      }
      if (res?.entry) {
        const saved = res.entry;
        setUnfilteredEntries((prev) => replaceEntry(prev, saved));
        if (entries === undefined) {
          setInternalEntries((prev) => replaceEntry(prev, saved));
          noteScopes([saved]);
        } else if (!onCreateCheckpoint) {
          onRefresh?.();
        }
      }
      setActionMessage(
        res
          ? `Checkpoint saved on ${res.branch} at ${res.head_sha.slice(0, 12)} (${res.dirty_files} dirty files).`
          : "Checkpoint saved.",
      );
    } catch (err) {
      setActionError(errorMessage(err, "Failed to create checkpoint"));
    } finally {
      setBusyCheckpoint(false);
    }
  };

  const handleImport = async () => {
    setActionError(null);
    setActionMessage(null);
    setBusyImport(true);
    try {
      let res: ImportWorkspaceStateResponse | undefined;
      if (onImportWorkspace) {
        res = (await onImportWorkspace()) ?? undefined;
      } else if (apiClient) {
        res = await apiClient.importWorkspaceState();
      } else {
        throw new Error("No FerroState API client is configured.");
      }
      setPendingImport(false);
      setActionMessage(
        res
          ? `Imported ${res.imported_memories} memories and ${res.imported_handoffs} handoffs (${res.scopes.join(", ")}).`
          : "Workspace import complete.",
      );
      if (entries === undefined && apiClient) {
        void runExplorer(query, scopeFilter, concernFilter);
      } else {
        onRefresh?.();
      }
    } catch (err) {
      setActionError(errorMessage(err, "Failed to import workspace state"));
    } finally {
      setBusyImport(false);
    }
  };

  const handleCodeSearch = async (event: React.FormEvent) => {
    event.preventDefault();
    setActionError(null);
    const q = codeQuery.trim();
    if (!q) {
      setActionError("Code query must not be empty.");
      return;
    }
    if (!apiClient) {
      setActionError("No FerroState API client is configured.");
      return;
    }
    const codeSeq = ++codeSeqRef.current;
    setCodeLoading(true);
    try {
      const res = await apiClient.searchCodeIndex({ q, kind: codeKind, limit: 50 });
      if (codeSeq !== codeSeqRef.current) return;
      setSymbols(res.symbols ?? []);
      setTokensSavedThisQuery(res.tokens_saved_this_query ?? 0);
      if (res.token_gain) {
        setTokenGain(res.token_gain);
      }
    } catch (err) {
      if (codeSeq === codeSeqRef.current) {
        setActionError(errorMessage(err, "Code index search failed"));
      }
    } finally {
      if (codeSeq === codeSeqRef.current) {
        setCodeLoading(false);
      }
    }
  };

  const renderStringList = (label: string, items: string[]) => {
    if (items.length === 0) {
      return null;
    }
    return (
      <div>
        <span style={{ fontSize: "11px", fontWeight: 700, color: "var(--text-muted)" }}>
          {label}
        </span>
        <ul style={{ margin: "4px 0 0", paddingLeft: "18px", fontSize: "13px" }}>
          {items.map((item, index) => (
            <li key={`${index}-${item}`}>{item}</li>
          ))}
        </ul>
      </div>
    );
  };

  const renderEntryCard = (entry: StateEntry, hit?: StateSearchHit) => {
    const resolved = resolvePayload(entry);
    return (
      <article
        key={entry.id || entry.slug}
        className="fd-card"
        data-testid={`state-entry-${entry.slug}`}
        style={{ display: "flex", flexDirection: "column", gap: "8px" }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
          <span className="fd-mono" style={{ fontWeight: 700, fontSize: "13px", overflowWrap: "anywhere" }}>
            {entry.slug}
          </span>
          <span className="fd-badge fd-mono">{entry.scope}</span>
          <span className="fd-badge">{entry.concern}</span>
          {resolved?.kind === "memory" && resolved.data.pinned && (
            <span className="fd-badge">★ Pinned</span>
          )}
          {(entry.sensitive || entry.ever_sensitive) && (
            <span className="fd-badge">Sensitive</span>
          )}
          {hit && <span className="fd-badge">score {hit.score_millis}</span>}
          {hit && <span className="fd-badge">~{formatCount(hit.estimated_tokens)} tokens</span>}
          {hit?.matched_terms.map((term) => (
            <span key={term} className="fd-badge">
              {term}
            </span>
          ))}
        </div>

        {hit?.snippet && (
          <p style={{ margin: 0, fontSize: "12px", color: "var(--text-muted)", fontStyle: "italic" }}>
            {hit.snippet}
          </p>
        )}

        {resolved?.kind === "memory" && (
          <>
            <div style={{ display: "flex", gap: "6px", flexWrap: "wrap", alignItems: "center" }}>
              <span className="fd-badge">{resolved.data.category}</span>
              <span className="fd-badge">{resolved.data.basis}</span>
              <span className="fd-badge">{resolved.data.lifecycle}</span>
            </div>
            <p style={{ margin: 0, fontSize: "13px", color: "var(--text-secondary)" }}>
              {resolved.data.summary}
            </p>
            {resolved.data.body && (
              <details>
                <summary style={{ cursor: "pointer", fontSize: "12px", color: "var(--text-muted)", minHeight: "28px" }}>
                  Body
                </summary>
                <div
                  style={{
                    whiteSpace: "pre-wrap",
                    overflowWrap: "anywhere",
                    fontSize: "13px",
                    paddingTop: "6px",
                  }}
                >
                  {resolved.data.body}
                </div>
              </details>
            )}
          </>
        )}

        {resolved?.kind === "handoff" && (
          <>
            <p style={{ margin: 0, fontSize: "13px", color: "var(--text-secondary)" }}>
              {resolved.data.summary}
            </p>
            {renderStringList("Shipped", resolved.data.shipped)}
            {renderStringList("In Flight", resolved.data.in_flight)}
            {renderStringList("Blockers", resolved.data.blockers)}
            {renderStringList("Next Steps", resolved.data.next_steps)}
            {resolved.data.notes && (
              <p style={{ margin: 0, fontSize: "13px", color: "var(--text-muted)" }}>
                {resolved.data.notes}
              </p>
            )}
          </>
        )}

        {resolved?.kind === "runtime" && (
          <>
            <span className="fd-badge">merge: {resolved.data.merge_mode}</span>
            <details>
              <summary style={{ cursor: "pointer", fontSize: "12px", color: "var(--text-muted)", minHeight: "28px" }}>
                Value
              </summary>
              <pre
                className="fd-mono"
                style={{ margin: "6px 0 0", fontSize: "12px", overflowX: "auto" }}
              >
                {JSON.stringify(resolved.data.value, null, 2)}
              </pre>
            </details>
          </>
        )}

        {!resolved && (
          <p style={{ margin: 0, fontSize: "12px", color: "var(--text-dim)" }}>
            Unknown payload shape.
          </p>
        )}

        <div style={{ display: "flex", gap: "6px", flexWrap: "wrap", fontSize: "12px", color: "var(--text-muted)" }}>
          <span>
            Updated: <span className="fd-mono">{entry.updated_at}</span>
          </span>
          <span>
            By: <span className="fd-mono">{entry.by}</span>
          </span>
        </div>
      </article>
    );
  };

  return (
    <section className="fd-pane" data-testid="ferrostate-pane" aria-label="FerroState">
      <div className="fd-scroll" style={{ display: "flex", flexDirection: "column", gap: "20px" }}>
        <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: "10px", flexWrap: "wrap" }}>
          <div style={{ display: "flex", alignItems: "center", gap: "10px", flexWrap: "wrap" }}>
            <h2 style={{ margin: 0, fontSize: "16px" }}>FerroState</h2>
            <span className="fd-badge" data-testid="ferrostate-badge">
              Durable Memory, Handoffs &amp; Structural Code Index
            </span>
            <span className="fd-badge" data-testid="memory-count">
              Memory {memoryCount}
            </span>
            <span className="fd-badge" data-testid="handoff-count">
              Handoffs {handoffCount}
            </span>
          </div>
          <div style={{ display: "flex", gap: "6px", flexWrap: "wrap" }}>
            <button
              type="button"
              className="fd-btn"
              style={{ minHeight: "36px" }}
              data-testid="checkpoint-btn"
              disabled={busyCheckpoint}
              onClick={() => void handleCheckpoint()}
            >
              {busyCheckpoint ? "Saving…" : "Snapshot Checkpoint"}
            </button>
            {isOwner &&
              (pendingImport ? (
                <>
                  <button
                    type="button"
                    className="fd-btn fd-btn-primary"
                    style={{ minHeight: "36px" }}
                    data-testid="import-workspace-confirm-btn"
                    disabled={busyImport}
                    onClick={() => void handleImport()}
                  >
                    {busyImport ? "Importing…" : "Confirm Import"}
                  </button>
                  <button
                    type="button"
                    className="fd-btn"
                    style={{ minHeight: "36px" }}
                    onClick={() => setPendingImport(false)}
                  >
                    Cancel
                  </button>
                </>
              ) : (
                <button
                  type="button"
                  className="fd-btn"
                  style={{ minHeight: "36px" }}
                  data-testid="import-workspace-btn"
                  onClick={() => setPendingImport(true)}
                >
                  Import Workspace (.agents/memory &amp; Notes)
                </button>
              ))}
          </div>
        </div>

        <div
          data-testid="token-gain-bar"
          className="fd-card"
          style={{ display: "flex", gap: "16px", flexWrap: "wrap", fontSize: "13px" }}
        >
          <span>
            Tokens saved:{" "}
            <strong data-testid="token-gain-saved">
              {tokenGain ? formatCount(tokenGain.tokens_saved) : "—"}
            </strong>
          </span>
          <span>
            Code queries:{" "}
            <strong data-testid="token-gain-queries">
              {tokenGain ? formatCount(tokenGain.queries_count) : "—"}
            </strong>
          </span>
          <span>
            Symbols indexed:{" "}
            <strong data-testid="token-gain-symbols">
              {tokenGain ? formatCount(tokenGain.symbols_indexed) : "—"}
            </strong>
          </span>
          <span>
            Files indexed:{" "}
            <strong data-testid="token-gain-files">
              {tokenGain ? formatCount(tokenGain.files_indexed) : "—"}
            </strong>
          </span>
          {!tokenGain && (
            <span style={{ color: "var(--text-dim)" }}>
              Token-gain telemetry updates after the first code index query.
            </span>
          )}
        </div>

        <div role="tablist" aria-label="FerroState Views" style={{ display: "flex", gap: "6px", flexWrap: "wrap" }}>
          <button
            type="button"
            role="tab"
            aria-selected={subView === "explorer"}
            className={`fd-btn${subView === "explorer" ? " fd-btn-active" : ""}`}
            style={{ minHeight: "36px" }}
            data-testid="subview-explorer-btn"
            onClick={() => setSubView("explorer")}
          >
            Memory &amp; Handoffs
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={subView === "code"}
            className={`fd-btn${subView === "code" ? " fd-btn-active" : ""}`}
            style={{ minHeight: "36px" }}
            data-testid="subview-code-btn"
            onClick={() => setSubView("code")}
          >
            Code Index
          </button>
        </div>

        {effectiveError && (
          <div role="alert" className="fd-card" style={{ color: "var(--diff-del-text)" }}>
            {effectiveError}
          </div>
        )}
        {actionError && (
          <div role="alert" className="fd-card" style={{ color: "var(--diff-del-text)" }}>
            {actionError}
          </div>
        )}
        {actionMessage && (
          <div role="status" className="fd-card" style={{ color: "var(--diff-add-text)" }}>
            {actionMessage}
          </div>
        )}

        {subView === "explorer" && (
          <>
            <form
              data-testid="state-search-form"
              onSubmit={handleExplorerSubmit}
              style={{ display: "flex", gap: "10px", flexWrap: "wrap", alignItems: "end" }}
            >
              <label style={{ ...labelStyle, flex: "1 1 240px" }}>
                Search Memory and Handoffs
                <input
                  type="search"
                  className="fd-input"
                  style={inputStyle}
                  name="state-search-q"
                  {...noAutofill}
                  placeholder="BM25 recall, for example token ceilings"
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                />
              </label>
              <label style={labelStyle}>
                Scope Filter
                <select
                  className="fd-select"
                  style={{ ...inputStyle, width: "auto", minWidth: "160px" }}
                  value={scopeFilter}
                  onChange={(e) => handleFilterChange(e.target.value, concernFilter)}
                >
                  <option value="">All</option>
                  {scopeOptions.map((scope) => (
                    <option key={scope} value={scope}>
                      {scope}
                    </option>
                  ))}
                </select>
              </label>
              <label style={labelStyle}>
                Concern Filter
                <select
                  className="fd-select"
                  style={{ ...inputStyle, width: "auto", minWidth: "140px" }}
                  value={concernFilter}
                  onChange={(e) => handleFilterChange(scopeFilter, e.target.value)}
                >
                  <option value="">All</option>
                  <option value="memory">memory</option>
                  <option value="handoff">handoff</option>
                  <option value="runtime">runtime</option>
                </select>
              </label>
              <button type="submit" className="fd-btn fd-btn-primary" style={{ minHeight: "40px" }} disabled={effectiveLoading}>
                Search
              </button>
              <button
                type="button"
                className="fd-btn"
                style={{ minHeight: "40px" }}
                onClick={handleClearSearch}
              >
                Clear
              </button>
            </form>

            <div data-testid="state-entries-section" style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
              {effectiveLoading && displayEntries.length === 0 ? (
                <p style={{ margin: 0, color: "var(--text-muted)" }}>Loading entries…</p>
              ) : displayEntries.length === 0 ? (
                effectiveError ? (
                  <p style={{ margin: 0, color: "var(--text-muted)", fontSize: "13px" }}>
                    Entries failed to load. Use Search to retry.
                  </p>
                ) : (
                  <p
                    data-testid="state-entries-empty"
                    style={{ margin: 0, color: "var(--text-muted)", fontSize: "13px" }}
                  >
                    No state entries match. Add a memory or handoff below.
                  </p>
                )
              ) : (
                displayEntries.map(({ entry, hit }) => renderEntryCard(entry, hit))
              )}
            </div>

            <form
              data-testid="state-upsert-form"
              onSubmit={(event) => void handleUpsert(event)}
              className="fd-card"
              style={{ display: "flex", flexDirection: "column", gap: "12px" }}
            >
              <h3 style={{ margin: 0, fontSize: "13px" }}>+ Add Memory / Handoff</h3>
              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))",
                  gap: "12px",
                }}
              >
                <label style={labelStyle}>
                  Entry Scope
                  <select
                    className="fd-select"
                    style={inputStyle}
                    value={formScope}
                    onChange={(e) => setFormScope(e.target.value)}
                  >
                    {scopeOptions.map((scope) => (
                      <option key={scope} value={scope}>
                        {scope}
                      </option>
                    ))}
                  </select>
                </label>
                <label style={labelStyle}>
                  Entry Slug
                  <input
                    className="fd-input fd-mono"
                    style={inputStyle}
                    name="state-entry-slug"
                    {...noAutofill}
                    placeholder="token-ceilings"
                    value={formSlug}
                    onChange={(e) => setFormSlug(e.target.value)}
                  />
                  <span style={{ fontWeight: 400, color: "var(--text-dim)" }}>
                    Lowercase letter first, then a-z, 0-9, -, or _.
                  </span>
                </label>
                <label style={labelStyle}>
                  Entry Concern
                  <select
                    className="fd-select"
                    style={inputStyle}
                    value={formConcern}
                    onChange={(e) => setFormConcern(e.target.value as "memory" | "handoff")}
                  >
                    <option value="memory">Memory</option>
                    <option value="handoff">Handoff</option>
                  </select>
                </label>
                <label style={labelStyle}>
                  Summary
                  <input
                    className="fd-input"
                    style={inputStyle}
                    name="state-entry-summary"
                    {...noAutofill}
                    maxLength={MAX_SUMMARY_CHARS}
                    placeholder="One line summary"
                    value={formSummary}
                    onChange={(e) => setFormSummary(e.target.value)}
                  />
                </label>

                {formConcern === "memory" ? (
                  <>
                    <label style={labelStyle}>
                      Category
                      <select
                        className="fd-select"
                        style={inputStyle}
                        value={formCategory}
                        onChange={(e) => setFormCategory(e.target.value)}
                      >
                        {MEMORY_CATEGORIES.map((category) => (
                          <option key={category} value={category}>
                            {category}
                          </option>
                        ))}
                      </select>
                    </label>
                    <label style={labelStyle}>
                      Basis
                      <select
                        className="fd-select"
                        style={inputStyle}
                        value={formBasis}
                        onChange={(e) => setFormBasis(e.target.value)}
                      >
                        {MEMORY_BASES.map((basis) => (
                          <option key={basis} value={basis}>
                            {basis}
                          </option>
                        ))}
                      </select>
                    </label>
                    <label style={{ ...labelStyle, gridColumn: "1 / -1" }}>
                      Body
                      <textarea
                        className="fd-textarea"
                        style={{ ...inputStyle, minHeight: "88px" }}
                        name="state-entry-body"
                        {...noAutofill}
                        placeholder="Details behind the summary"
                        value={formBody}
                        onChange={(e) => setFormBody(e.target.value)}
                      />
                    </label>
                    <label style={{ display: "flex", alignItems: "center", gap: "8px", fontSize: "13px", fontWeight: 600, color: "var(--text-secondary)", minHeight: "40px" }}>
                      <input
                        type="checkbox"
                        checked={formPinned}
                        onChange={(e) => setFormPinned(e.target.checked)}
                        style={{ width: "20px", height: "20px" }}
                      />
                      Pinned
                    </label>
                  </>
                ) : (
                  <>
                    <label style={labelStyle}>
                      Shipped
                      <textarea
                        className="fd-textarea"
                        style={{ ...inputStyle, minHeight: "64px" }}
                        name="state-entry-shipped"
                        {...noAutofill}
                        placeholder="One item per line"
                        value={formShipped}
                        onChange={(e) => setFormShipped(e.target.value)}
                      />
                    </label>
                    <label style={labelStyle}>
                      In Flight
                      <textarea
                        className="fd-textarea"
                        style={{ ...inputStyle, minHeight: "64px" }}
                        name="state-entry-in-flight"
                        {...noAutofill}
                        placeholder="One item per line"
                        value={formInFlight}
                        onChange={(e) => setFormInFlight(e.target.value)}
                      />
                    </label>
                    <label style={labelStyle}>
                      Blockers
                      <textarea
                        className="fd-textarea"
                        style={{ ...inputStyle, minHeight: "64px" }}
                        name="state-entry-blockers"
                        {...noAutofill}
                        placeholder="One item per line"
                        value={formBlockers}
                        onChange={(e) => setFormBlockers(e.target.value)}
                      />
                    </label>
                    <label style={labelStyle}>
                      Next Steps
                      <textarea
                        className="fd-textarea"
                        style={{ ...inputStyle, minHeight: "64px" }}
                        name="state-entry-next-steps"
                        {...noAutofill}
                        placeholder="One item per line"
                        value={formNextSteps}
                        onChange={(e) => setFormNextSteps(e.target.value)}
                      />
                    </label>
                    <label style={{ ...labelStyle, gridColumn: "1 / -1" }}>
                      Notes
                      <textarea
                        className="fd-textarea"
                        style={{ ...inputStyle, minHeight: "64px" }}
                        name="state-entry-notes"
                        {...noAutofill}
                        placeholder="Optional notes"
                        value={formNotes}
                        onChange={(e) => setFormNotes(e.target.value)}
                      />
                    </label>
                  </>
                )}

                <label style={{ display: "flex", alignItems: "center", gap: "8px", fontSize: "13px", fontWeight: 600, color: "var(--text-secondary)", minHeight: "40px" }}>
                  <input
                    type="checkbox"
                    checked={formSensitive}
                    onChange={(e) => setFormSensitive(e.target.checked)}
                    style={{ width: "20px", height: "20px" }}
                  />
                  Sensitive (owner only)
                </label>
              </div>
              <div>
                <button
                  type="submit"
                  className="fd-btn fd-btn-primary"
                  style={{ minHeight: "40px", padding: "8px 18px" }}
                  disabled={saving}
                >
                  {saving ? "Saving…" : "Save Entry"}
                </button>
              </div>
            </form>
          </>
        )}

        {subView === "code" && (
          <>
            <form
              data-testid="code-search-form"
              onSubmit={(event) => void handleCodeSearch(event)}
              style={{ display: "flex", gap: "10px", flexWrap: "wrap", alignItems: "end" }}
            >
              <label style={labelStyle}>
                Code Search Mode
                <select
                  className="fd-select"
                  style={{ ...inputStyle, width: "auto", minWidth: "200px" }}
                  value={codeKind}
                  onChange={(e) => setCodeKind(e.target.value as CodeSearchKind)}
                >
                  <option value="def">Definition (def)</option>
                  <option value="search">Symbol Search (search)</option>
                  <option value="outline">File Outline (outline)</option>
                </select>
              </label>
              <label style={{ ...labelStyle, flex: "1 1 240px" }}>
                Code Query
                <input
                  className="fd-input fd-mono"
                  style={inputStyle}
                  name="code-search-q"
                  {...noAutofill}
                  placeholder="Symbol name or file path"
                  value={codeQuery}
                  onChange={(e) => setCodeQuery(e.target.value)}
                />
              </label>
              <button type="submit" className="fd-btn fd-btn-primary" style={{ minHeight: "40px" }} disabled={codeLoading}>
                {codeLoading ? "Searching…" : "Search Code"}
              </button>
            </form>

            {tokensSavedThisQuery !== null && (
              <div
                data-testid="code-tokens-saved"
                className="fd-card"
                style={{ color: "var(--diff-add-text)", fontSize: "13px" }}
              >
                Saved {formatCount(tokensSavedThisQuery)} tokens on this query versus whole
                files.
              </div>
            )}

            <div data-testid="code-symbols-section" style={{ display: "flex", flexDirection: "column", gap: "10px" }}>
              {symbols === null ? (
                <p style={{ margin: 0, color: "var(--text-muted)", fontSize: "13px" }}>
                  Run a code index query to see structural symbols with token savings.
                </p>
              ) : symbols.length === 0 ? (
                <p
                  data-testid="code-symbols-empty"
                  style={{ margin: 0, color: "var(--text-muted)", fontSize: "13px" }}
                >
                  No symbols matched the query.
                </p>
              ) : (
                symbols.map((symbol) => (
                  <article
                    key={`${symbol.path}:${symbol.start_line}:${symbol.name}`}
                    className="fd-card"
                    data-testid={`code-symbol-${symbol.name}`}
                    style={{ display: "flex", flexDirection: "column", gap: "8px" }}
                  >
                    <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
                      <span className="fd-mono" style={{ fontWeight: 700, fontSize: "13px" }}>
                        {symbol.name}
                      </span>
                      <span className="fd-badge">{symbol.kind}</span>
                      <span className="fd-badge fd-mono" style={{ overflowWrap: "anywhere" }}>
                        {symbol.path}:{symbol.start_line}-{symbol.end_line}
                      </span>
                      <span className="fd-badge">
                        saved {formatCount(symbol.tokens_saved)} /{" "}
                        {formatCount(symbol.estimated_file_tokens)} file tokens
                      </span>
                    </div>
                    {symbol.signature && (
                      <pre
                        className="fd-mono"
                        style={{
                          margin: 0,
                          fontSize: "12px",
                          overflowX: "auto",
                          backgroundColor: "var(--bg-input)",
                          border: "1px solid var(--border-subtle)",
                          borderRadius: "var(--radius-sm)",
                          padding: "8px 10px",
                        }}
                      >
                        {symbol.signature}
                      </pre>
                    )}
                    {symbol.snippet && (
                      <details>
                        <summary style={{ cursor: "pointer", fontSize: "12px", color: "var(--text-muted)", minHeight: "28px" }}>
                          Snippet (~{formatCount(symbol.snippet_tokens)} tokens)
                        </summary>
                        <pre
                          className="fd-mono"
                          style={{ margin: "6px 0 0", fontSize: "12px", overflowX: "auto", whiteSpace: "pre-wrap" }}
                        >
                          {symbol.snippet}
                        </pre>
                      </details>
                    )}
                  </article>
                ))
              )}
            </div>
          </>
        )}
      </div>
    </section>
  );
};
