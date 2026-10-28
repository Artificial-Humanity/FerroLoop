import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import { FerroStatePane } from "../components/FerroStatePane";
import { COCKPIT_TABS } from "../components/ContextHeader";
import type { HubAuthority, StateEntry } from "../types/api";

const OWNER: HubAuthority = { tier: "owner", login: "lmcfarlin" };
const PEER: HubAuthority = { tier: "peer", agent: "ferris" };

const MEMORY_ENTRY: StateEntry = {
  id: "urn:fl:state:1",
  scope: "project:ferroloop",
  concern: "memory",
  slug: "pr-review-policy",
  sensitive: false,
  ever_sensitive: false,
  payload: {
    memory: {
      category: "rule",
      basis: "ruling",
      lifecycle: "active",
      superseded_by: null,
      pinned: true,
      summary: "PR authors merge their own pull requests",
      body: "Reviewers approve. Authors merge after approval and green checks.",
    },
  },
  head_revision: "rev-1",
  updated_at: "2026-10-27T04:00:00Z",
  by: "ferris",
};

const HANDOFF_ENTRY: StateEntry = {
  id: "urn:fl:state:2",
  scope: "project:ferroloop",
  concern: "handoff",
  slug: "ferrostate-phase-2",
  sensitive: false,
  ever_sensitive: false,
  payload: {
    handoff: {
      summary: "FerroState Phase 2 in flight",
      shipped: ["State store Format 6"],
      in_flight: ["BM25 recall endpoint"],
      blockers: ["None"],
      next_steps: ["Ship FerroStatePane"],
      notes: "Felicia owns Rust. Felix owns the pane.",
    },
  },
  head_revision: "rev-2",
  updated_at: "2026-10-27T05:00:00Z",
  by: "felicia",
};

const SAMPLE_ENTRIES = [MEMORY_ENTRY, HANDOFF_ENTRY];

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

/** Fake hub fetch that records calls and serves the FerroState endpoints. */
function makeStateClient() {
  const calls: Array<{ url: string; method: string; body?: unknown }> = [];
  let entries: StateEntry[] = [...SAMPLE_ENTRIES];

  const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const method = init?.method ?? "GET";
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    calls.push({ url, method, body });

    if (url.startsWith("/api/v1/state/search")) {
      return jsonResponse({
        query: "merge",
        hits: [
          {
            entry: MEMORY_ENTRY,
            score_millis: 1730,
            matched_terms: ["merge", "authors"],
            snippet: "Authors merge after approval",
            estimated_tokens: 32,
          },
        ],
        total_hits: 1,
      });
    }
    if (url.startsWith("/api/v1/state/code-search")) {
      return jsonResponse({
        query: "VaultGatePane",
        kind: "def",
        symbols: [
          {
            name: "VaultGatePane",
            kind: "component",
            path: "ui/src/components/VaultGatePane.tsx",
            start_line: 72,
            end_line: 260,
            signature: "export const VaultGatePane: React.FC<VaultGatePaneProps>",
            snippet: "export const VaultGatePane = ({...}) => { ... }",
            estimated_file_tokens: 4200,
            snippet_tokens: 180,
            tokens_saved: 4020,
          },
        ],
        tokens_saved_this_query: 4020,
        token_gain: {
          queries_count: 7,
          tokens_saved: 21340,
          files_indexed: 128,
          symbols_indexed: 1934,
        },
      });
    }
    if (url.startsWith("/api/v1/state/checkpoint") && method === "POST") {
      return jsonResponse({
        entry: HANDOFF_ENTRY,
        branch: "refs/heads/fl/state",
        head_sha: "abc1234deadbeef",
        dirty_files: 2,
      });
    }
    if (url.startsWith("/api/v1/state/import-workspace") && method === "POST") {
      return jsonResponse({
        imported_memories: 42,
        imported_handoffs: 3,
        scopes: ["workspace", "project:ferroloop"],
      });
    }
    if (url.startsWith("/api/v1/state") && method === "GET") {
      return jsonResponse({ entries });
    }
    if (url.startsWith("/api/v1/state") && method === "POST") {
      const entry: StateEntry = {
        id: "urn:fl:state:new",
        scope: body.scope ?? "workspace",
        concern: body.concern,
        slug: body.slug,
        sensitive: Boolean(body.sensitive),
        ever_sensitive: false,
        payload: body.payload ?? {
          memory: {
            category: body.category ?? "note",
            basis: body.basis ?? "inference",
            lifecycle: "active",
            superseded_by: null,
            pinned: Boolean(body.pinned),
            summary: body.summary ?? "",
            body: body.body ?? "",
          },
        },
        head_revision: "rev-new",
        updated_at: "2026-10-28T00:00:00Z",
        by: "felix",
      };
      entries = [...entries, entry];
      return jsonResponse({ entry });
    }
    return jsonResponse({ error: "not found" }, 404);
  });

  const client = new HubApiClient({
    fetchImpl: fetchImpl as unknown as typeof fetch,
  });
  return { client, calls };
}

describe("FerroStatePane", () => {
  it("is reachable from the cockpit tab bar as FerroState", () => {
    expect(COCKPIT_TABS).toContainEqual({ id: "state", label: "FerroState" });
  });

  it("renders memory and handoff cards with counts, pinned badge, and expandable body", () => {
    render(<FerroStatePane entries={SAMPLE_ENTRIES} authority={PEER} />);

    expect(screen.getByTestId("ferrostate-badge")).toHaveTextContent(
      /Durable Memory, Handoffs & Structural Code Index/,
    );
    expect(screen.getByTestId("memory-count")).toHaveTextContent("1");
    expect(screen.getByTestId("handoff-count")).toHaveTextContent("1");

    const memory = screen.getByTestId("state-entry-pr-review-policy");
    expect(memory).toHaveTextContent("project:ferroloop");
    expect(memory).toHaveTextContent("rule");
    expect(memory).toHaveTextContent("ruling");
    expect(memory).toHaveTextContent(/Pinned/);
    expect(memory).toHaveTextContent("PR authors merge their own pull requests");
    expect(memory).toHaveTextContent("Authors merge after approval and green checks.");

    const handoff = screen.getByTestId("state-entry-ferrostate-phase-2");
    expect(handoff).toHaveTextContent("FerroState Phase 2 in flight");
    expect(handoff).toHaveTextContent("State store Format 6");
    expect(handoff).toHaveTextContent("BM25 recall endpoint");
    expect(handoff).toHaveTextContent("Ship FerroStatePane");
    expect(handoff).toHaveTextContent("Felicia owns Rust. Felix owns the pane.");

    // Import is owner-only.
    expect(screen.queryByTestId("import-workspace-btn")).toBeNull();
  });

  it("loads entries via apiClient on mount, switches to BM25 search for a query, and clears back to the list", async () => {
    const { client, calls } = makeStateClient();

    render(<FerroStatePane authority={PEER} apiClient={client} />);

    expect(await screen.findByTestId("state-entry-pr-review-policy")).toBeInTheDocument();
    expect(calls.some((c) => c.url.startsWith("/api/v1/state") && c.method === "GET")).toBe(true);

    const searchInput = screen.getByLabelText(/Search Memory and Handoffs/i) as HTMLInputElement;
    expect(searchInput.dataset["1pIgnore"]).toBe("true");
    fireEvent.change(searchInput, { target: { value: "merge" } });
    fireEvent.click(screen.getByRole("button", { name: /^Search$/i }));

    await waitFor(() => {
      expect(calls.some((c) => c.url.startsWith("/api/v1/state/search?q=merge"))).toBe(true);
    });
    const hit = await screen.findByTestId("state-entry-pr-review-policy");
    expect(hit).toHaveTextContent(/1730/);
    expect(hit).toHaveTextContent("merge");
    expect(hit).toHaveTextContent(/32 tokens/);
    // Handoff did not match and leaves the result list.
    expect(screen.queryByTestId("state-entry-ferrostate-phase-2")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: /Clear/i }));
    expect(await screen.findByTestId("state-entry-ferrostate-phase-2")).toBeInTheDocument();
  });

  it("creates a memory entry through the apiClient and shows the new card", async () => {
    const { client, calls } = makeStateClient();

    render(<FerroStatePane authority={OWNER} apiClient={client} />);
    await screen.findByTestId("state-entry-pr-review-policy");

    fireEvent.change(screen.getByLabelText(/Entry Slug/i), {
      target: { value: "token-ceilings" },
    });
    fireEvent.change(screen.getByLabelText(/^Summary$/i), {
      target: { value: "Agent loops need hard token ceilings" },
    });
    fireEvent.change(screen.getByLabelText(/^Body$/i), {
      target: { value: "Pre-flight checks fail fast on budget exhaustion." },
    });
    fireEvent.click(screen.getByLabelText(/Pinned/i));
    fireEvent.click(screen.getByRole("button", { name: /Save Entry/i }));

    await waitFor(() => {
      const post = calls.find(
        (c) => c.url === "/api/v1/state" && c.method === "POST",
      );
      expect(post?.body).toMatchObject({
        slug: "token-ceilings",
        concern: "memory",
        summary: "Agent loops need hard token ceilings",
        pinned: true,
      });
    });
    expect(await screen.findByTestId("state-entry-token-ceilings")).toHaveTextContent(
      "Agent loops need hard token ceilings",
    );
  });

  it("rejects an invalid slug before calling the API", async () => {
    const { client, calls } = makeStateClient();

    render(<FerroStatePane authority={OWNER} apiClient={client} />);
    await screen.findByTestId("state-entry-pr-review-policy");

    fireEvent.change(screen.getByLabelText(/Entry Slug/i), {
      target: { value: "Bad Slug!" },
    });
    fireEvent.change(screen.getByLabelText(/^Summary$/i), {
      target: { value: "A summary" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Save Entry/i }));

    expect(await screen.findByRole("alert")).toHaveTextContent(/slug/i);
    expect(calls.some((c) => c.method === "POST" && c.url === "/api/v1/state")).toBe(false);
  });

  it("triggers a snapshot checkpoint and reports branch and head", async () => {
    const { client, calls } = makeStateClient();

    render(<FerroStatePane authority={PEER} apiClient={client} />);
    await screen.findByTestId("state-entry-pr-review-policy");

    fireEvent.click(screen.getByTestId("checkpoint-btn"));

    await waitFor(() => {
      expect(
        calls.some((c) => c.url === "/api/v1/state/checkpoint" && c.method === "POST"),
      ).toBe(true);
    });
    expect(await screen.findByRole("status")).toHaveTextContent(/refs\/heads\/fl\/state/);
  });

  it("runs the owner-only workspace import after inline confirmation", async () => {
    const { client, calls } = makeStateClient();

    render(<FerroStatePane authority={OWNER} apiClient={client} />);
    await screen.findByTestId("state-entry-pr-review-policy");

    fireEvent.click(screen.getByTestId("import-workspace-btn"));
    expect(
      calls.some((c) => c.url === "/api/v1/state/import-workspace"),
    ).toBe(false);

    fireEvent.click(screen.getByTestId("import-workspace-confirm-btn"));

    await waitFor(() => {
      expect(
        calls.some((c) => c.url === "/api/v1/state/import-workspace" && c.method === "POST"),
      ).toBe(true);
    });
    expect(await screen.findByRole("status")).toHaveTextContent(/42 memories/);
  });

  it("searches the structural code index and shows token savings in the header", async () => {
    const { client, calls } = makeStateClient();

    render(<FerroStatePane authority={PEER} apiClient={client} />);
    await screen.findByTestId("state-entry-pr-review-policy");

    fireEvent.click(screen.getByTestId("subview-code-btn"));
    fireEvent.change(screen.getByLabelText(/Code Query/i), {
      target: { value: "VaultGatePane" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Search Code/i }));

    await waitFor(() => {
      expect(
        calls.some((c) => c.url.startsWith("/api/v1/state/code-search?q=VaultGatePane")),
      ).toBe(true);
    });

    const symbol = await screen.findByTestId("code-symbol-VaultGatePane");
    expect(symbol).toHaveTextContent("component");
    expect(symbol).toHaveTextContent("ui/src/components/VaultGatePane.tsx:72-260");
    expect(symbol).toHaveTextContent("export const VaultGatePane");
    expect(symbol).toHaveTextContent(/4,020/);
    expect(screen.getByTestId("code-tokens-saved")).toHaveTextContent(/4,020/);
    expect(screen.getByTestId("token-gain-saved")).toHaveTextContent(/21,340/);
    expect(screen.getByTestId("token-gain-queries")).toHaveTextContent("7");
    expect(screen.getByTestId("token-gain-symbols")).toHaveTextContent(/1,934/);
  });
});
