import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { FerroWatchPane } from "../components/FerroWatchPane";
import { FerroStatePane } from "../components/FerroStatePane";
import { HubDrawer } from "../components/HubDrawer";

describe("Record #94 — OpenTelemetry OTLP Trace & Metric Exporter UI (FerroWatchPane)", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it("renders OTLP Exporter card, switches grpc / http/protobuf protocol, and exports snapshot with 404 fallback", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response("Not Found", { status: 404 }),
    );

    render(
      <FerroWatchPane
        authority={{ tier: "owner", login: "lmcfarlin" }}
        agents={[]}
        sessions={[]}
      />,
    );

    const card = screen.getByTestId("watch-otlp-exporter-card");
    expect(card).toBeTruthy();
    expect(screen.getByTestId("watch-otlp-endpoint-badge").textContent).toContain(
      "http://127.0.0.1:4317 (grpc)",
    );
    expect(screen.getByTestId("watch-otlp-cadence-badge").textContent).toContain(
      "flush: every 15s",
    );
    expect(screen.getByTestId("watch-otlp-counters").textContent).toContain(
      "attempts: 482",
    );
    expect(screen.getByTestId("watch-otlp-counters").textContent).toContain(
      "regret records: 14",
    );

    // Switch protocol to http/protobuf
    fireEvent.change(screen.getByTestId("watch-otlp-protocol-select"), {
      target: { value: "http/protobuf" },
    });
    expect(screen.getByTestId("watch-otlp-endpoint-badge").textContent).toContain(
      "http/protobuf",
    );

    // Click Export Snapshot / Test Endpoint
    fireEvent.click(screen.getByTestId("watch-otlp-export-btn"));

    await waitFor(() => {
      expect(screen.getByTestId("watch-otlp-export-status").textContent).toContain(
        "Exported OTLP telemetry batch to http://127.0.0.1:4317 (http/protobuf)",
      );
    });
    expect(screen.getByTestId("watch-otlp-counters").textContent).toContain(
      "attempts: 483",
    );
  });
});

describe("Record #91 — FerroState External Reference-Repository Indexing UI (FerroStatePane)", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it("renders Reference Repos (fl state ref-index) section and indexes a new repo with 404 fallback", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response("Not Found", { status: 404 }),
    );

    render(
      <FerroStatePane
        entries={[]}
        authority={{ tier: "owner", login: "lmcfarlin" }}
      />,
    );

    expect(screen.getByTestId("state-reference-repos-section")).toBeTruthy();
    expect(screen.getByTestId("state-ref-repo-tokio-rs-tokio")).toBeTruthy();
    expect(
      screen.getByTestId("state-ref-repo-files-tokio-rs-tokio").textContent,
    ).toContain("612 files");
    expect(
      screen.getByTestId("state-ref-repo-chunks-tokio-rs-tokio").textContent,
    ).toContain("4,820 chunks");

    fireEvent.change(screen.getByTestId("state-ref-repo-input"), {
      target: { value: "serde-rs/serde" },
    });
    fireEvent.change(screen.getByTestId("state-ref-tag-input"), {
      target: { value: "v1.0.215" },
    });
    fireEvent.click(screen.getByTestId("state-index-ref-repo-btn"));

    await waitFor(() => {
      expect(screen.getByTestId("state-ref-index-status").textContent).toContain(
        "Indexed reference repository serde-rs/serde@v1.0.215",
      );
    });
    expect(screen.getByTestId("state-ref-repo-serde-rs-serde")).toBeTruthy();
  });
});

describe("Record #92 — Counted Tokenizer Savings Ledger UI (FerroStatePane, FerroWatchPane, HubDrawer)", () => {
  it("renders exact tokenizer-counted tokens saved vs ceil(chars/4) heuristic across FerroState, FerroWatch, and HubDrawer", () => {
    const { unmount } = render(
      <FerroStatePane
        entries={[]}
        authority={{ tier: "owner", login: "lmcfarlin" }}
      />,
    );

    expect(screen.getByTestId("counted-tokenizer-ledger-card")).toBeTruthy();
    expect(
      screen.getByTestId("tokenizer-exact-saved-o200k_base").textContent,
    ).toContain("184,250 tok");
    expect(
      screen.getByTestId("tokenizer-heuristic-delta-o200k_base").textContent,
    ).toContain("ceil(chars/4): 201,400 (-8.5%)");
    expect(screen.getByTestId("tokenizer-ledger-row-cl100k_base")).toBeTruthy();
    expect(screen.getByTestId("tokenizer-ledger-row-sentencepiece")).toBeTruthy();
    unmount();

    const { unmount: unmountWatch } = render(
      <FerroWatchPane
        authority={{ tier: "owner", login: "lmcfarlin" }}
        agents={[]}
        sessions={[]}
      />,
    );
    expect(screen.getByTestId("watch-tokenizer-ledger-card")).toBeTruthy();
    expect(screen.getByTestId("watch-tokenizer-row-o200k_base")).toBeTruthy();
    unmountWatch();

    render(<HubDrawer defaultSubTab="stats" />);
    expect(screen.getByTestId("hub-tokenizer-ledger-card")).toBeTruthy();
    expect(screen.getByTestId("hub-tokenizer-row-o200k_base")).toBeTruthy();
  });
});
