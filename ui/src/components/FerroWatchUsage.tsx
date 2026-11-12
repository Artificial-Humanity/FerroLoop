import React, { useCallback, useEffect, useMemo, useState } from "react";
import type { HubApiClient } from "../api/client";
import type { WatchUsageGroup, WatchUsageReport, WatchUsageRow } from "../types/api";

type Column = "key" | "input" | "output" | "cache_read" | "cache_write" | "turns" | "subagents";
const columns: { key: Column; label: string }[] = [
  { key: "key", label: "Name" }, { key: "input", label: "Input" },
  { key: "output", label: "Output" }, { key: "cache_read", label: "Cache read" },
  { key: "cache_write", label: "Cache write" }, { key: "turns", label: "Turns" },
  { key: "subagents", label: "Subagents" },
];

function utc(ms: number): string {
  const iso = new Date(ms).toISOString();
  const precision = ms % 1000 !== 0 ? 23 : ms % 60_000 !== 0 ? 19 : 16;
  return iso.slice(0, precision).replace("T", " ") + " UTC";
}
function display(value: number | null, partial: boolean): string {
  if (value === null) return "No data";
  return value.toLocaleString("en-US") + (partial ? " (partial)" : "");
}
function cell(row: WatchUsageRow, key: Column, unknownLaunches: number): string {
  if (key === "key") return row.key;
  if (key === "subagents") return display(row.subagents, unknownLaunches > 0);
  if (key === "turns") return display(row.turns, row.turns_unavailable);
  return display(row[key], row.unavailable[key]);
}

export function FerroWatchUsage({ apiClient }: { apiClient: HubApiClient }) {
  const [by, setBy] = useState<WatchUsageGroup>("persona");
  const [since, setSince] = useState("24h");
  const [report, setReport] = useState<WatchUsageReport | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refresh, setRefresh] = useState(0);
  const [now, setNow] = useState(() => Date.now());
  const [sort, setSort] = useState<{ key: Column; ascending: boolean }>({ key: "key", ascending: true });

  useEffect(() => {
    const timer = window.setInterval(() => {
      setNow(Date.now());
      setRefresh((n) => n + 1);
    }, 60_000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    let active = true;
    setLoading(true);
    setError(null);
    apiClient.getWatchUsage({ since, by }).then((result) => {
      if (!result || !Array.isArray(result.rows) || !result.coverage || !result.undated ||
          !["persona", "vendor", "model"].includes(result.by) ||
          ![result.requested_start_ms, result.requested_end_ms, result.effective_start_ms, result.effective_end_ms]
            .every((value) => Number.isFinite(value) && Number.isFinite(new Date(value).getTime())) ||
          (result.coverage.last_success_ms !== null &&
            (!Number.isFinite(result.coverage.last_success_ms) || !Number.isFinite(new Date(result.coverage.last_success_ms).getTime()))) ||
          result.rows.some((row) => !row || typeof row.key !== "string" || !row.unavailable)) {
        throw new Error("Invalid usage response.");
      }
      if (active) setReport(result);
    }).catch((err: unknown) => {
      if (active) setError(err instanceof Error ? err.message : "Unable to load usage.");
    }).finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [apiClient, by, since, refresh]);

  const sorted = useMemo(() => {
    return [...(report?.rows ?? [])].sort((a, b) => {
      const av = a[sort.key];
      const bv = b[sort.key];
      if (av == null) return bv == null ? a.key.localeCompare(b.key) : 1;
      if (bv == null) return -1;
      const compare = typeof av === "string" && typeof bv === "string" ? av.localeCompare(bv) : Number(av) - Number(bv);
      return (sort.ascending ? compare : -compare) || a.key.localeCompare(b.key);
    });
  }, [report, sort]);
  const changeSort = useCallback((key: Column) => setSort((old) => ({ key, ascending: old.key === key ? !old.ascending : key === "key" })), []);

  return (
    <section aria-label="Usage" data-testid="watch-usage-section" style={{ padding: "12px", border: "1px solid var(--border-subtle)", borderRadius: "6px", background: "var(--bg-surface)", minWidth: 0 }}>
      <div style={{ display: "flex", alignItems: "center", gap: "10px", flexWrap: "wrap" }}>
        <h2 style={{ margin: 0, fontSize: "14px", flex: "1 1 auto" }}>Token usage</h2>
        <label>Period {" "}<select className="fd-select" aria-label="Usage period" value={since} onChange={(e) => setSince(e.target.value)}>
          <option value="24h">24 hours</option><option value="48h">48 hours</option><option value="7d">7 days</option>
        </select></label>
        <label>Group by {" "}<select className="fd-select" aria-label="Group usage by" value={by} onChange={(e) => setBy(e.target.value as WatchUsageGroup)}>
          <option value="persona">Persona</option><option value="vendor">Vendor</option><option value="model">Model</option>
        </select></label>
        <button type="button" className="fd-btn" onClick={() => setRefresh((n) => n + 1)}>Refresh usage</button>
      </div>
      {loading && <p role="status">Loading usage…</p>}
      {error && <p role="alert">Unable to load usage: {error}</p>}
      {(error || loading) && report && <p>Showing previous results.</p>}
      {report && <>
        <p style={{ color: "var(--text-secondary)", fontSize: "12px" }}>Effective window: {utc(report.effective_start_ms)} – {utc(report.effective_end_ms)} (end exclusive; whole hours)</p>
        <p style={{ color: "var(--text-secondary)", fontSize: "12px" }}>Requested window: {utc(report.requested_start_ms)} – {utc(report.requested_end_ms)} (end exclusive)</p>
        {report.coverage.unavailable_sources > 0 && <p>{report.coverage.unavailable_sources} source(s) have no data.</p>}
        {report.coverage.unattributed_sources > 0 && <p>{report.coverage.unattributed_sources} source(s) are unattributed.</p>}
        {report.coverage.ambiguous_sources > 0 && <p>{report.coverage.ambiguous_sources} source(s) have ambiguous attribution.</p>}
        {report.coverage.pending && <p role="status">Collection in progress.</p>}
        {report.coverage.incomplete && <p role="status">Collection is incomplete; totals may be partial.</p>}
        {report.coverage.last_success_ms === null ? <p>No completed collection yet.</p> : now - report.coverage.last_success_ms > 300_000 && <p>Collection data may be stale. Last collected {utc(report.coverage.last_success_ms)}.</p>}
        {report.coverage.unknown_launch_times > 0 && <p>{report.coverage.unknown_launch_times.toLocaleString("en-US")} subagent launch time(s) unknown; subagent counts are partial.</p>}
        {report.coverage.undated_observations > 0 && <p>Undated usage is outside this window: input {cell(report.undated, "input", 0)}, output {cell(report.undated, "output", 0)}, cache read {cell(report.undated, "cache_read", 0)}, cache write {cell(report.undated, "cache_write", 0)}.</p>}
        {sorted.length === 0 ? <p>No usage for this period.</p> : <div data-testid="usage-table-scroll" tabIndex={0} role="region" aria-label="Scroll usage table" style={{ overflowX: "auto", maxWidth: "100%" }}>
          <table aria-label="Token usage" style={{ width: "100%", borderCollapse: "collapse", whiteSpace: "nowrap", fontSize: "12px" }}>
            <thead><tr>{columns.map(({ key, label }) => <th key={key} scope="col" aria-sort={sort.key === key ? (sort.ascending ? "ascending" : "descending") : undefined} style={{ textAlign: key === "key" ? "left" : "right", padding: "6px" }}>
              <button type="button" className="fd-btn" aria-label={`Sort by ${key === "key" ? report.by : label}`} onClick={() => changeSort(key)}>{key === "key" ? report.by[0].toUpperCase() + report.by.slice(1) : label}{sort.key === key ? (sort.ascending ? " ↑" : " ↓") : ""}</button>
            </th>)}</tr></thead>
            <tbody>{sorted.map((row) => <tr key={row.key}>{columns.map(({ key }) => <td key={key} style={{ textAlign: key === "key" ? "left" : "right", padding: "6px", borderTop: "1px solid var(--border-subtle)" }}>{cell(row, key, report.coverage.unknown_launch_times)}</td>)}</tr>)}</tbody>
          </table>
        </div>}
      </>}
    </section>
  );
}
