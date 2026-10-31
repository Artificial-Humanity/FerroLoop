import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import {
  ArtifactPreviewPane,
  clampZoom,
  diffLines,
  nextZoomStep,
  spreadsheetColumnLabel,
} from "../components/ArtifactPreviewPane";
import type {
  ArtifactPreviewResponse,
  ArtifactVersionInfo,
  OfficePreview,
} from "../types/api";

interface Fixture {
  preview: ArtifactPreviewResponse;
  office?: OfficePreview | { error: string; status: number };
  versions?: ArtifactVersionInfo[] | "missing";
  versionText?: Record<number, string>;
}

function preview(
  path: string,
  kind: ArtifactPreviewResponse["kind"],
  extra: Partial<ArtifactPreviewResponse> = {},
): ArtifactPreviewResponse {
  const name = path.split("/").pop() ?? path;
  return {
    path,
    name,
    ext: name.split(".").pop() ?? "",
    mime_type: "application/octet-stream",
    size_bytes: 2048,
    kind,
    content: null,
    raw_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(path)}&mode=raw`,
    download_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(path)}&download=1`,
    ...extra,
  };
}

function version(n: number, current: boolean, sha = "a".repeat(64)): ArtifactVersionInfo {
  return {
    version: n,
    sha256: n.toString(16).repeat(8) + sha.slice(8),
    size_bytes: 100 * n,
    recorded_at_ms: Date.UTC(2026, 0, n, 12, 0, 0),
    is_current: current,
    has_content: true,
  };
}

function makeClient(fixtures: Record<string, Fixture>) {
  const calls: string[] = [];
  const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    calls.push(url);
    const parsed = new URL(url, "http://127.0.0.1:8420");
    const path = parsed.searchParams.get("path") ?? "";
    const fx = fixtures[path];
    const json = (body: unknown, status = 200) =>
      new Response(JSON.stringify(body), {
        status,
        headers: { "Content-Type": "application/json" },
      });
    if (parsed.pathname === "/api/v1/artifacts/list") return json({ artifacts: [] });
    if (!fx) return json({ error: "artifact file not found" }, 404);
    switch (parsed.pathname) {
      case "/api/v1/artifacts/preview":
        return json(fx.preview);
      case "/api/v1/artifacts/office-preview":
        if (!fx.office) return json({ error: "no" }, 404);
        if ("error" in fx.office) return json({ error: fx.office.error }, fx.office.status);
        return json({ ok: true, path, ext: fx.preview.ext, preview: fx.office });
      case "/api/v1/artifacts/versions":
        if (!fx.versions || fx.versions === "missing") return json({ error: "not found" }, 404);
        return json({ ok: true, path, versions: fx.versions });
      case "/api/v1/artifacts/version-raw": {
        const n = Number(parsed.searchParams.get("version"));
        const text = fx.versionText?.[n];
        return text === undefined
          ? json({ error: "version not found" }, 404)
          : new Response(text, { status: 200 });
      }
      default:
        return json({ error: "unexpected" }, 404);
    }
  });
  const client = new HubApiClient({ baseUrl: "", fetchImpl: fetchImpl as unknown as typeof fetch });
  return { client, calls, fetchImpl };
}

const DOCX: OfficePreview = {
  kind: "docx",
  blocks: [
    { type: "heading", level: 1, text: "Quarterly Plan" },
    { type: "paragraph", text: "Intro paragraph." },
    { type: "heading", level: 3, text: "Budget" },
    {
      type: "table",
      rows: [
        ["Item", "Cost"],
        ["Servers", "1200"],
      ],
    },
  ],
};

const XLSX: OfficePreview = {
  kind: "xlsx",
  sheets: [
    { name: "Totals", rows: [["Region", "Sales"], ["EMEA", "42"]], row_count: 2, col_count: 2 },
    { name: "Notes", rows: [["only note"]], row_count: 1, col_count: 1 },
  ],
};

const PPTX: OfficePreview = {
  kind: "pptx",
  slides: [
    { index: 1, title: "Welcome", bullets: ["Hello", "World"], notes: "Say hi" },
    { index: 2, title: "Roadmap", bullets: ["Q1", "Q2"], notes: "" },
    { index: 3, title: "Thanks", bullets: [], notes: "Take questions" },
  ],
};

describe("HubApiClient artifact studio methods", () => {
  it("requests versions, version content and office previews with encoded paths", async () => {
    const seen: Array<{ url: string; auth: string | null }> = [];
    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      seen.push({ url, auth: new Headers(init?.headers).get("Authorization") });
      if (url.includes("/version-raw")) return new Response("# raw text", { status: 200 });
      if (url.includes("/versions")) {
        return new Response(JSON.stringify({ ok: true, path: "/a b.md", versions: [] }), {
          status: 200,
        });
      }
      return new Response(
        JSON.stringify({ ok: true, path: "/a b.docx", preview: { kind: "docx", blocks: [] } }),
        { status: 200 },
      );
    });
    const client = new HubApiClient({
      baseUrl: "http://hub",
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    client.setToken("tok-1");
    const v = await client.getArtifactVersions("/a b.md");
    expect(v.versions).toEqual([]);
    expect(seen[0].url).toBe("http://hub/api/v1/artifacts/versions?path=%2Fa+b.md");
    expect(await client.getArtifactVersionContent("/a b.md", 3)).toBe("# raw text");
    expect(seen[1].url).toBe("http://hub/api/v1/artifacts/version-raw?path=%2Fa+b.md&version=3");
    expect(seen[1].auth).toBe("Bearer tok-1");
    const o = await client.getOfficePreview("/a b.docx");
    expect(o.preview.kind).toBe("docx");
    expect(seen[2].url).toBe("http://hub/api/v1/artifacts/office-preview?path=%2Fa+b.docx");
    expect(client.getArtifactVersionRawUrl("/x.png", 2)).toBe(
      "http://hub/api/v1/artifacts/version-raw?path=%2Fx.png&version=2&token=tok-1",
    );
  });

  it("surfaces the hub's error message when a version cannot be read", async () => {
    const fetchImpl = vi.fn(
      async () => new Response(JSON.stringify({ error: "version not found" }), { status: 404 }),
    );
    const client = new HubApiClient({ baseUrl: "", fetchImpl: fetchImpl as unknown as typeof fetch });
    await expect(client.getArtifactVersionContent("/a.md", 9)).rejects.toThrow("version not found");
    const plain = vi.fn(async () => new Response("boom", { status: 500 }));
    const c2 = new HubApiClient({ baseUrl: "", fetchImpl: plain as unknown as typeof fetch });
    await expect(c2.getArtifactVersionContent("/a.md", 1)).rejects.toThrow("HTTP 500");
  });
});

describe("helpers", () => {
  it("labels spreadsheet columns and steps and clamps the zoom", () => {
    expect([0, 1, 25, 26, 27, 701, 702].map(spreadsheetColumnLabel)).toEqual([
      "A", "B", "Z", "AA", "AB", "ZZ", "AAA",
    ]);
    expect(clampZoom(10)).toBe(25);
    expect(clampZoom(9000)).toBe(400);
    expect(clampZoom(Number.NaN)).toBe(100);
    expect(nextZoomStep(100, 1)).toBe(125);
    expect(nextZoomStep(100, -1)).toBe(75);
    expect(nextZoomStep(400, 1)).toBe(400);
    expect(nextZoomStep(25, -1)).toBe(25);
    expect(nextZoomStep(110, -1)).toBe(100);
  });

  it("diffs lines into aligned rows", () => {
    expect(diffLines("a\nb", "a\nb").every((r) => r.kind === "same")).toBe(true);
    expect(diffLines("", "")).toEqual([]);
    const rows = diffLines("one\ntwo\nthree\n", "one\n2\nthree\nfour\n");
    expect(rows.map((r) => r.kind)).toEqual(["same", "change", "same", "add"]);
    expect(rows[1]).toMatchObject({ left: "two", right: "2", leftNo: 2, rightNo: 2 });
    expect(rows[3]).toMatchObject({ left: null, right: "four", leftNo: null, rightNo: 4 });
    const removed = diffLines("x\ny\nz", "x\nz");
    expect(removed.map((r) => r.kind)).toEqual(["same", "del", "same"]);
    expect(removed[1]).toMatchObject({ left: "y", right: null, leftNo: 2 });
    expect(diffLines("a\r\nb", "a\nb").every((r) => r.kind === "same")).toBe(true);
    // Huge unrelated inputs fall back to a bounded delete-then-add diff.
    const big = Array.from({ length: 2500 }, (_, i) => `a${i}`).join("\n");
    const other = Array.from({ length: 2500 }, (_, i) => `b${i}`).join("\n");
    expect(diffLines(big, other).length).toBe(2500);
  });
});

describe("Office document renderer", () => {
  it("renders a .docx as a styled document with headings, paragraphs and tables", async () => {
    const path = "/w/plan.docx";
    const { client } = makeClient({
      [path]: { preview: preview(path, "binary"), office: DOCX },
    });
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    const doc = await screen.findByTestId("artifact-office-docx");
    const headings = within(doc).getAllByTestId("artifact-docx-heading");
    expect(headings.map((h) => h.tagName)).toEqual(["H1", "H3"]);
    expect(headings[0]).toHaveTextContent("Quarterly Plan");
    expect(within(doc).getByTestId("artifact-docx-paragraph")).toHaveTextContent("Intro paragraph.");
    const table = within(doc).getByTestId("artifact-docx-table");
    expect(table).toHaveTextContent("Servers");
    expect(table.querySelectorAll("tr")).toHaveLength(2);
    expect(screen.getByTestId("artifact-kind-badge")).toHaveTextContent("Word");
    expect(screen.queryByTestId("artifact-binary-card")).toBeNull();
    // The download control stays available.
    expect(screen.getByTestId("artifact-download-btn")).toBeInTheDocument();
  });

  it("renders an .xlsx as a tabbed grid with row and column headers", async () => {
    const path = "/w/book.xlsx";
    const { client } = makeClient({ [path]: { preview: preview(path, "binary"), office: XLSX } });
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    const sheet = await screen.findByTestId("artifact-office-xlsx");
    expect(screen.getByTestId("artifact-kind-badge")).toHaveTextContent("Excel");
    expect(within(sheet).getByTestId("artifact-xlsx-tab-0")).toHaveTextContent("Totals");
    expect(within(sheet).getByTestId("artifact-xlsx-tab-1")).toHaveTextContent("Notes");
    expect(
      within(sheet).getAllByTestId("artifact-xlsx-col-header").map((h) => h.textContent),
    ).toEqual(["A", "B"]);
    expect(
      within(sheet).getAllByTestId("artifact-xlsx-row-header").map((h) => h.textContent),
    ).toEqual(["1", "2"]);
    expect(within(sheet).getByTestId("artifact-xlsx-grid")).toHaveTextContent("EMEA");
    fireEvent.click(within(sheet).getByTestId("artifact-xlsx-tab-1"));
    expect(within(sheet).getByTestId("artifact-xlsx-grid")).toHaveTextContent("only note");
    expect(within(sheet).getByTestId("artifact-xlsx-grid")).not.toHaveTextContent("EMEA");
    expect(within(sheet).getByTestId("artifact-xlsx-tab-1").className).toContain("fd-btn-active");
  });

  it("renders a .pptx as a slide deck with thumbnails, prev/next and speaker notes", async () => {
    const path = "/w/deck.pptx";
    const { client } = makeClient({ [path]: { preview: preview(path, "binary"), office: PPTX } });
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    await screen.findByTestId("artifact-office-pptx");
    expect(screen.getByTestId("artifact-kind-badge")).toHaveTextContent("PowerPoint");
    expect(screen.getByTestId("artifact-pptx-title")).toHaveTextContent("Welcome");
    expect(screen.getAllByTestId("artifact-pptx-bullet").map((b) => b.textContent)).toEqual([
      "Hello",
      "World",
    ]);
    expect(screen.getByTestId("artifact-pptx-counter")).toHaveTextContent("Slide 1 / 3");
    expect(screen.getByTestId("artifact-pptx-notes")).toHaveTextContent("Say hi");
    expect(screen.getByTestId("artifact-pptx-prev")).toBeDisabled();
    fireEvent.click(screen.getByTestId("artifact-pptx-next"));
    expect(screen.getByTestId("artifact-pptx-title")).toHaveTextContent("Roadmap");
    expect(screen.getByTestId("artifact-pptx-notes")).toHaveTextContent("No notes.");
    fireEvent.click(screen.getByTestId("artifact-pptx-thumb-2"));
    expect(screen.getByTestId("artifact-pptx-counter")).toHaveTextContent("Slide 3 / 3");
    expect(screen.getByTestId("artifact-pptx-notes")).toHaveTextContent("Take questions");
    expect(screen.getByTestId("artifact-pptx-next")).toBeDisabled();
    fireEvent.click(screen.getByTestId("artifact-pptx-prev"));
    expect(screen.getByTestId("artifact-pptx-counter")).toHaveTextContent("Slide 2 / 3");
    expect(screen.getAllByTestId(/artifact-pptx-thumb-/)).toHaveLength(3);
  });

  it("renders a directly supplied preview without asking the hub for one", async () => {
    const path = "/w/direct.docx";
    const { client, calls } = makeClient({ [path]: { preview: preview(path, "binary") } });
    render(<ArtifactPreviewPane apiClient={client} activePath={path} officePreview={DOCX} />);
    expect(await screen.findByTestId("artifact-office-docx")).toHaveTextContent("Quarterly Plan");
    expect(calls.some((c) => c.includes("office-preview"))).toBe(false);
  });

  it("uses an office_preview carried in the preview response", async () => {
    const path = "/w/inline.pptx";
    const { client, calls } = makeClient({
      [path]: { preview: preview(path, "binary", { office_preview: PPTX }) },
    });
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    expect(await screen.findByTestId("artifact-office-pptx")).toBeInTheDocument();
    expect(calls.some((c) => c.includes("office-preview"))).toBe(false);
  });

  it("shows the error and the download card when the document cannot be previewed", async () => {
    const path = "/w/broken.xlsx";
    const { client } = makeClient({
      [path]: {
        preview: preview(path, "binary"),
        office: { error: "encrypted ZIP entries are not supported", status: 422 },
      },
    });
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    expect(await screen.findByTestId("artifact-office-error")).toHaveTextContent("encrypted ZIP");
    expect(screen.getByTestId("artifact-binary-card")).toBeInTheDocument();
  });

  it("renders an empty workbook and an empty deck without crashing", async () => {
    const path = "/w/empty.pptx";
    const { client } = makeClient({
      [path]: { preview: preview(path, "binary"), office: { kind: "pptx", slides: [] } },
    });
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    expect(await screen.findByTestId("artifact-office-pptx")).toHaveTextContent("no slides");
  });
});

describe("Zoom and pan canvas", () => {
  const svgPath = "/w/logo.svg";
  const load = async () => {
    const { client } = makeClient({
      [svgPath]: {
        preview: preview(svgPath, "svg", { content: "<svg viewBox='0 0 4 4'></svg>" }),
      },
    });
    render(<ArtifactPreviewPane apiClient={client} activePath={svgPath} />);
    return screen.findByTestId("artifact-zoom-canvas");
  };

  it("zooms in and out between 25% and 400% and shows the percentage", async () => {
    const canvas = await load();
    const badge = screen.getByTestId("artifact-zoom-badge");
    expect(badge).toHaveTextContent("100%");
    fireEvent.click(screen.getByRole("button", { name: "Zoom In" }));
    expect(badge).toHaveTextContent("125%");
    expect(canvas.getAttribute("data-scale")).toBe("125");
    for (let i = 0; i < 10; i++) fireEvent.click(screen.getByRole("button", { name: "Zoom In" }));
    expect(badge).toHaveTextContent("400%");
    expect(screen.getByRole("button", { name: "Zoom In" })).toBeDisabled();
    for (let i = 0; i < 20; i++) fireEvent.click(screen.getByRole("button", { name: "Zoom Out" }));
    expect(badge).toHaveTextContent("25%");
    expect(screen.getByRole("button", { name: "Zoom Out" })).toBeDisabled();
    fireEvent.click(screen.getByTestId("artifact-zoom-150"));
    expect(badge).toHaveTextContent("150%");
    expect(screen.getByTestId("artifact-zoom-150").className).toContain("fd-btn-active");
    fireEvent.click(screen.getByTestId("artifact-zoom-100"));
    expect(badge).toHaveTextContent("100%");
    expect(screen.getByTestId("artifact-zoom-content").style.transform).toContain("scale(1)");
  });

  it("zooms with ctrl+wheel only", async () => {
    const canvas = await load();
    fireEvent.wheel(canvas, { deltaY: -100 });
    expect(canvas.getAttribute("data-scale")).toBe("100");
    fireEvent.wheel(canvas, { deltaY: -100, ctrlKey: true });
    expect(canvas.getAttribute("data-scale")).toBe("125");
    fireEvent.wheel(canvas, { deltaY: 100, ctrlKey: true });
    expect(canvas.getAttribute("data-scale")).toBe("100");
  });

  it("pans by click-drag and Fit resets zoom and position", async () => {
    const canvas = await load();
    fireEvent.click(screen.getByTestId("artifact-zoom-200"));
    fireEvent.mouseDown(canvas, { clientX: 100, clientY: 100, button: 0 });
    fireEvent.mouseMove(canvas, { clientX: 140, clientY: 70 });
    expect(canvas.getAttribute("data-offset-x")).toBe("40");
    expect(canvas.getAttribute("data-offset-y")).toBe("-30");
    expect(screen.getByTestId("artifact-zoom-content").style.transform).toContain(
      "translate(40px, -30px)",
    );
    fireEvent.mouseUp(canvas);
    // After release, moving does not pan.
    fireEvent.mouseMove(canvas, { clientX: 500, clientY: 500 });
    expect(canvas.getAttribute("data-offset-x")).toBe("40");
    // A second drag continues from where the first stopped.
    fireEvent.mouseDown(canvas, { clientX: 0, clientY: 0, button: 0 });
    fireEvent.mouseMove(canvas, { clientX: 10, clientY: 10 });
    expect(canvas.getAttribute("data-offset-x")).toBe("50");
    fireEvent.mouseLeave(canvas);
    fireEvent.mouseMove(canvas, { clientX: 99, clientY: 99 });
    expect(canvas.getAttribute("data-offset-x")).toBe("50");
    fireEvent.click(screen.getByTestId("artifact-zoom-fit"));
    expect(canvas.getAttribute("data-scale")).toBe("100");
    expect(canvas.getAttribute("data-offset-x")).toBe("0");
    expect(canvas.getAttribute("data-offset-y")).toBe("0");
  });

  it("is offered for raster images and resets when another image is opened", async () => {
    const a = "/w/a.png";
    const b = "/w/b.png";
    const { client } = makeClient({
      [a]: { preview: preview(a, "image") },
      [b]: { preview: preview(b, "image") },
    });
    const { rerender } = render(<ArtifactPreviewPane apiClient={client} activePath={a} />);
    await screen.findByTestId("artifact-image-preview");
    fireEvent.click(screen.getByRole("button", { name: "Zoom In" }));
    expect(screen.getByTestId("artifact-zoom-badge")).toHaveTextContent("125%");
    rerender(<ArtifactPreviewPane apiClient={client} activePath={b} />);
    await waitFor(() =>
      expect(screen.getByTestId("artifact-image-preview").getAttribute("alt")).toBe("b.png"),
    );
    expect(screen.getByTestId("artifact-zoom-badge")).toHaveTextContent("100%");
  });

  it("wraps Mermaid diagrams too", async () => {
    const path = "/w/flow.mmd";
    const { client } = makeClient({
      [path]: { preview: preview(path, "mermaid", { content: "graph TD\n  A-->B" }) },
    });
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    const canvas = await screen.findByTestId("artifact-zoom-canvas");
    expect(canvas).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Zoom In" }));
    expect(screen.getByTestId("artifact-zoom-badge")).toHaveTextContent("125%");
  });

  it("is not shown for text artifacts", async () => {
    const path = "/w/readme.md";
    const { client } = makeClient({
      [path]: { preview: preview(path, "markdown", { content: "# Hi" }) },
    });
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    await screen.findByTestId("artifact-markdown-body");
    expect(screen.queryByTestId("artifact-zoom-canvas")).toBeNull();
  });
});

describe("Artifact version history", () => {
  const path = "/w/spec.md";
  const fixture = (versions: Fixture["versions"] = [version(1, false), version(2, false), version(3, true)]) => ({
    [path]: {
      preview: preview(path, "markdown", { content: "# Three" }),
      versions,
      versionText: { 1: "# One\nshared\nold line", 2: "# Two\nshared\nold line", 3: "# Three\nshared\nnew line\nextra" },
    } satisfies Fixture,
  });

  it("shows History (N) and a newest-first timeline with digest, size and time", async () => {
    const { client } = makeClient(fixture());
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    const toggle = await screen.findByTestId("artifact-history-toggle");
    await waitFor(() => expect(toggle).toHaveTextContent("History (3)"));
    expect(screen.queryByTestId("artifact-version-history")).toBeNull();
    fireEvent.click(toggle);
    const drawer = await screen.findByTestId("artifact-version-history");
    const rows = within(drawer).getAllByTestId(/artifact-version-row-/);
    expect(rows.map((r) => r.getAttribute("data-testid"))).toEqual([
      "artifact-version-row-3",
      "artifact-version-row-2",
      "artifact-version-row-1",
    ]);
    expect(rows[0]).toHaveTextContent("v3");
    expect(rows[0]).toHaveTextContent("Current");
    expect(rows[0]).toHaveTextContent("33333333");
    expect(rows[0]).toHaveTextContent("300 B");
    expect(rows[0]).toHaveTextContent("2026-01-03 12:00:00 UTC");
    expect(rows[1]).not.toHaveTextContent("Current");
    fireEvent.click(toggle);
    expect(screen.queryByTestId("artifact-version-history")).toBeNull();
  });

  it("inspects a historical version and switches back to Current", async () => {
    const { client } = makeClient(fixture());
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    fireEvent.click(await screen.findByTestId("artifact-history-toggle"));
    fireEvent.click(await screen.findByTestId("artifact-version-view-1"));
    const content = await screen.findByTestId("artifact-version-content");
    expect(content).toHaveTextContent("# One");
    expect(screen.getByTestId("artifact-version-banner")).toHaveTextContent("Viewing v1");
    expect(screen.queryByTestId("artifact-markdown-body")).toBeNull();
    fireEvent.click(screen.getByTestId("artifact-version-current-btn"));
    expect(screen.queryByTestId("artifact-version-panel")).toBeNull();
    expect(screen.getByTestId("artifact-markdown-body")).toHaveTextContent("Three");
    // Back to Current also works from the panel.
    fireEvent.click(screen.getByTestId("artifact-version-view-2"));
    await screen.findByTestId("artifact-version-content");
    fireEvent.click(screen.getByTestId("artifact-version-back-btn"));
    expect(screen.getByTestId("artifact-markdown-body")).toBeInTheDocument();
  });

  it("compares two versions side by side", async () => {
    const { client } = makeClient(fixture());
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    fireEvent.click(await screen.findByTestId("artifact-history-toggle"));
    const diffBtn = await screen.findByTestId("artifact-version-diff-btn");
    expect(diffBtn).toBeDisabled();
    // Pick in reverse order: the diff still runs older -> newer.
    fireEvent.click(screen.getByTestId("artifact-version-compare-3"));
    fireEvent.click(screen.getByTestId("artifact-version-compare-1"));
    expect(diffBtn).not.toBeDisabled();
    fireEvent.click(diffBtn);
    const view = await screen.findByTestId("artifact-diff-view");
    expect(screen.getByTestId("artifact-version-banner")).toHaveTextContent("Comparing v1 and v3");
    expect(within(view).getByTestId("artifact-diff-left-label")).toHaveTextContent("v1");
    expect(within(view).getByTestId("artifact-diff-right-label")).toHaveTextContent("v3");
    const kinds = within(view)
      .getAllByTestId("artifact-diff-row")
      .map((r) => r.getAttribute("data-kind"));
    expect(kinds).toEqual(["change", "same", "change", "add"]);
    expect(within(view).getByTestId("artifact-diff-summary")).toHaveTextContent("3 changed lines");
    fireEvent.click(screen.getByTestId("artifact-version-back-btn"));
    expect(screen.queryByTestId("artifact-diff-view")).toBeNull();
  });

  it("diffs a single selected version against the current one", async () => {
    const { client } = makeClient(fixture());
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    fireEvent.click(await screen.findByTestId("artifact-history-toggle"));
    const diffBtn = await screen.findByTestId("artifact-version-diff-btn");
    fireEvent.click(screen.getByTestId("artifact-version-compare-3"));
    expect(diffBtn).toBeDisabled();
    fireEvent.click(screen.getByTestId("artifact-version-compare-2"));
    fireEvent.click(screen.getByTestId("artifact-version-compare-3"));
    expect(diffBtn).not.toBeDisabled();
    fireEvent.click(diffBtn);
    await screen.findByTestId("artifact-diff-view");
    expect(screen.getByTestId("artifact-version-banner")).toHaveTextContent("Comparing v2 and v3");
  });

  it("reports a version that cannot be read", async () => {
    const fx = fixture();
    delete (fx[path].versionText as Record<number, string>)[2];
    const { client } = makeClient(fx);
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    fireEvent.click(await screen.findByTestId("artifact-history-toggle"));
    fireEvent.click(await screen.findByTestId("artifact-version-view-2"));
    expect(await screen.findByTestId("artifact-version-error")).toHaveTextContent("version not found");
  });

  it("has no History button when the hub keeps no history", async () => {
    const { client } = makeClient(fixture("missing"));
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    await screen.findByTestId("artifact-markdown-body");
    expect(screen.queryByTestId("artifact-history-toggle")).toBeNull();
  });

  it("shows History (0) with an empty message before anything is recorded", async () => {
    const { client } = makeClient(fixture([]));
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    const toggle = await screen.findByTestId("artifact-history-toggle");
    expect(toggle).toHaveTextContent("History (0)");
    fireEvent.click(toggle);
    expect(await screen.findByTestId("artifact-version-history")).toHaveTextContent(
      "No versions recorded yet.",
    );
    expect(screen.getByTestId("artifact-version-diff-btn")).toBeDisabled();
  });

  it("refreshes the timeline when the drawer opens", async () => {
    const versions = [version(1, true)];
    const { client, calls } = makeClient(fixture(versions));
    render(<ArtifactPreviewPane apiClient={client} activePath={path} />);
    const toggle = await screen.findByTestId("artifact-history-toggle");
    await waitFor(() => expect(toggle).toHaveTextContent("History (1)"));
    versions[0].is_current = false;
    versions.push(version(2, true));
    fireEvent.click(toggle);
    await waitFor(() => expect(toggle).toHaveTextContent("History (2)"));
    expect(calls.filter((c) => c.includes("/artifacts/versions")).length).toBe(2);
  });

  it("offers a raw link for non-text versions and a diff notice", async () => {
    const png = "/w/pic.png";
    const { client } = makeClient({
      [png]: {
        preview: preview(png, "image"),
        versions: [version(1, false), version(2, true)],
      },
    });
    render(<ArtifactPreviewPane apiClient={client} activePath={png} />);
    fireEvent.click(await screen.findByTestId("artifact-history-toggle"));
    fireEvent.click(await screen.findByTestId("artifact-version-view-1"));
    const content = await screen.findByTestId("artifact-version-content");
    expect(within(content).getByTestId("artifact-version-image").getAttribute("src")).toContain(
      "version-raw?path=%2Fw%2Fpic.png&version=1",
    );
    expect(within(content).getByRole("link", { name: "Open raw v1" })).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("artifact-version-compare-1"));
    fireEvent.click(screen.getByTestId("artifact-version-diff-btn"));
    expect(await screen.findByTestId("artifact-diff-unsupported")).toBeInTheDocument();
  });
});
