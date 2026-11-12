import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { HubApiClient } from "../api/client";
import { FerroWatchUsage } from "../components/FerroWatchUsage";
import { FerroWatchPane } from "../components/FerroWatchPane";
import type { WatchUsageReport } from "../types/api";

const unavailable = { input: false, output: false, cache_read: true, cache_write: true };
function report(): WatchUsageReport {
  return {
    requested_start_ms: Date.parse("2026-11-10T00:10:00Z"), requested_end_ms: Date.parse("2026-11-11T00:10:00Z"),
    effective_start_ms: Date.parse("2026-11-10T00:00:00Z"), effective_end_ms: Date.parse("2026-11-11T01:00:00Z"), by: "persona", unit: "tokens",
    rows: [
      { key: "Zoe", input: 0, output: 2, cache_read: null, cache_write: 20, turns: null, turns_unavailable: true, subagents: 1, unavailable },
      { key: "Felix", input: 1200, output: 8, cache_read: 10, cache_write: null, turns: null, turns_unavailable: true, subagents: 0, unavailable },
    ],
    undated: { key: "undated", input: null, output: null, cache_read: null, cache_write: null, turns: null, turns_unavailable: true, subagents: 0, unavailable },
    coverage: { incomplete: false, pending: false, unavailable_sources: 0, malformed_lines: 0, oversized_lines: 0, last_success_ms: Date.parse("2026-11-11T00:09:00Z"), unattributed_sources: 0, ambiguous_sources: 0, unknown_launch_times: 0, undated_observations: 0 },
  };
}
function client(getUsage: ReturnType<typeof vi.fn>): HubApiClient {
  return { getWatchUsage: getUsage } as unknown as HubApiClient;
}

describe("FerroWatch usage", () => {
  it("renders an accessible sortable table, zero, no data, partial values and effective UTC window", async () => {
    const getUsage = vi.fn().mockResolvedValue(report());
    render(<FerroWatchUsage apiClient={client(getUsage)} />);
    const table = await screen.findByRole("table", { name: /token usage/i });
    expect(getUsage).toHaveBeenCalledWith({ since: "24h", by: "persona" });
    expect(screen.getByText(/2026-11-10 00:00 UTC.*2026-11-11 01:00 UTC/)).toBeInTheDocument();
    expect(within(table).getAllByText("No data").length).toBeGreaterThan(0);
    expect(within(table).getByText("20 (partial)")).toBeInTheDocument();
    expect(within(table).getAllByText("0").length).toBeGreaterThan(0);
    const input = within(table).getByRole("button", { name: /sort by input/i });
    input.focus();
    expect(input).toHaveFocus();
    fireEvent.click(input);
    expect(input.closest("th")).toHaveAttribute("aria-sort", "descending");
    expect(within(table).getAllByRole("row")[1]).toHaveTextContent("Felix");
    fireEvent.click(input);
    expect(input.closest("th")).toHaveAttribute("aria-sort", "ascending");
    expect(within(table).getAllByRole("row")[1]).toHaveTextContent("Zoe");
    expect(table.closest("[data-testid=usage-table-scroll]")).toHaveStyle({ overflowX: "auto" });
    fireEvent.change(screen.getByLabelText("Group usage by"), { target: { value: "vendor" } });
    await waitFor(() => expect(getUsage).toHaveBeenCalledWith({ since: "24h", by: "vendor" }));
  });
  it("displays loading and empty states", async () => {
    let resolve!: (value: WatchUsageReport) => void;
    const pending = new Promise<WatchUsageReport>((r) => { resolve = r; });
    render(<FerroWatchUsage apiClient={client(vi.fn().mockReturnValue(pending))} />);
    expect(screen.getByText(/loading usage/i)).toBeInTheDocument();
    resolve({ ...report(), rows: [] });
    expect(await screen.findByText(/no usage for this period/i)).toBeInTheDocument();
  });
  it("reports incomplete collection and retains stale data after refresh failure", async () => {
    const sample = report();
    sample.coverage.incomplete = true;
    sample.coverage.pending = true;
    const getUsage = vi.fn().mockResolvedValueOnce(sample).mockRejectedValueOnce(new Error("Network unavailable"));
    render(<FerroWatchUsage apiClient={client(getUsage)} />);
    expect(await screen.findByText(/collection is incomplete/i)).toBeInTheDocument();
    expect(screen.getByText(/collection in progress/i)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /refresh usage/i }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/network unavailable/i);
    expect(screen.getByText(/showing previous results/i)).toBeInTheDocument();
    expect(screen.getByRole("table", { name: /token usage/i })).toBeInTheDocument();
  });
  it("shows an error when the initial request fails", async () => {
    render(<FerroWatchUsage apiClient={client(vi.fn().mockRejectedValue(new Error("Access denied")))} />);
    expect(await screen.findByRole("alert")).toHaveTextContent("Access denied");
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
  });
  it("keeps prior grouping labeled correctly while a new grouping loads", async () => {
    let resolve!: (value: WatchUsageReport) => void;
    const pending = new Promise<WatchUsageReport>((r) => { resolve = r; });
    const getUsage = vi.fn().mockResolvedValueOnce(report()).mockReturnValueOnce(pending);
    render(<FerroWatchUsage apiClient={client(getUsage)} />);
    await screen.findByRole("table", { name: /token usage/i });
    fireEvent.change(screen.getByLabelText("Group usage by"), { target: { value: "vendor" } });
    expect(screen.getByText(/showing previous results/i)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Sort by persona" })).toBeInTheDocument();
    resolve({ ...report(), by: "vendor" });
    expect(await screen.findByRole("button", { name: "Sort by vendor" })).toBeInTheDocument();
  });

  it("shows unavailable-source coverage and both requested and effective boundaries", async () => {
    const sample = report();
    sample.coverage.unavailable_sources = 1;
    sample.coverage.unattributed_sources = 2;
    render(<FerroWatchUsage apiClient={client(vi.fn().mockResolvedValue(sample))} />);
    expect(await screen.findByText(/1 source.*no data/i)).toBeInTheDocument();
    expect(screen.getByText(/2.*unattributed/i)).toBeInTheDocument();
    expect(screen.getByText(/requested window: 2026-11-10 00:10 UTC.*2026-11-11 00:10 UTC/i)).toBeInTheDocument();
  });

  it("never requests the Owner report for a peer", () => {
    const getUsage = vi.fn();
    render(<FerroWatchPane authority={{ tier: "peer", agent: "peer" }} apiClient={client(getUsage)} />);
    expect(getUsage).not.toHaveBeenCalled();
    expect(screen.queryByTestId("watch-usage-section")).not.toBeInTheDocument();
  });

  it("shows an error instead of crashing for an incompatible response", async () => {
    render(<FerroWatchUsage apiClient={client(vi.fn().mockResolvedValue({ telemetry: {} }))} />);
    expect(await screen.findByRole("alert")).toHaveTextContent(/invalid usage response/i);
  });

  it("preserves seconds and milliseconds in requested boundaries", async () => {
    const sample = report();
    sample.requested_start_ms += 12345;
    render(<FerroWatchUsage apiClient={client(vi.fn().mockResolvedValue(sample))} />);
    expect(await screen.findByText(/2026-11-10 00:10:12.345 UTC/)).toBeInTheDocument();
  });

});
