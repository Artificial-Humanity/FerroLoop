import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import { App } from "../App";
import {
  ArtifactPreviewPane,
  isUserFacingArtifactPath,
} from "../components/ArtifactPreviewPane";
import { OPEN_ARTIFACT_EVENT, dispatchOpenArtifact } from "../components/RichMarkdown";
import type { ArtifactPreviewResponse } from "../types/api";

function makeApiClient(
  previewResponses: Record<string, ArtifactPreviewResponse>,
): HubApiClient {
  const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    const json = (body: unknown, status = 200) =>
      new Response(JSON.stringify(body), {
        status,
        headers: { "Content-Type": "application/json" },
      });

    if (url.startsWith("/api/v1/artifacts/preview")) {
      const parsed = new URL(url, "http://127.0.0.1:8420");
      const pathParam = parsed.searchParams.get("path") ?? "";
      const hit = previewResponses[pathParam];
      if (hit) {
        return json(hit);
      }
      return json({ error: "artifact file not found" }, 404);
    }
    if (url.startsWith("/api/v1/artifacts/list")) {
      return json({
        artifacts: Object.values(previewResponses).map((item) => ({
          path: item.path,
          name: item.name,
          ext: item.ext,
          kind: item.kind,
          size_bytes: item.size_bytes,
        })),
      });
    }
    if (url.includes("/api/v1/auth/session")) {
      return json({
        authenticated: true,
        authority: { tier: "owner", login: "lmcfarlin" },
      });
    }
    if (url.includes("/transcript")) {
      return json({
        session_id: "sess-ferris",
        turns: [
          {
            index: 0,
            role: "assistant",
            content:
              "Review the spec at [spec.md](file:///home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/docs/spec.md).",
            thinking: null,
            tool_calls: [],
          },
        ],
        is_busy: false,
        queue: [],
      });
    }
    if (url.includes("/api/v1/agents")) {
      return json({ agents: [], areas: [], routes: [] });
    }
    if (url.includes("/api/v1/sessions")) {
      return json({
        sessions: [
          {
            id: "sess-ferris",
            source: "route",
            adapter: "antigravity",
            persona: "ferris",
            status: "active",
          },
        ],
      });
    }
    return json({});
  });

  return new HubApiClient({
    fetchImpl: fetchImpl as unknown as typeof fetch,
  });
}

describe("ArtifactPreviewPane", () => {
  it("renders Markdown with embedded Mermaid diagram and Download button", async () => {
    const mdPath =
      "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/docs/spec.md";
    const apiClient = makeApiClient({
      [mdPath]: {
        path: mdPath,
        name: "spec.md",
        ext: "md",
        mime_type: "text/plain; charset=utf-8",
        size_bytes: 184,
        kind: "markdown",
        content: [
          "# Tactical Artifact Side Panel",
          "",
          "- Supports **Markdown** and diagrams.",
          "",
          "```mermaid",
          "flowchart LR",
          '  Chat["Chat Pane"] --> Artifacts["Artifact Preview"]',
          "```",
        ].join("\n"),
        raw_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(mdPath)}&mode=raw`,
        download_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(mdPath)}&download=1`,
      },
    });

    render(
      <ArtifactPreviewPane
        apiClient={apiClient}
        activePath={mdPath}
        recentPaths={[mdPath]}
        onSelectPath={vi.fn()}
      />,
    );

    const mdView = await screen.findByTestId("artifact-markdown-body");
    expect(mdView).toHaveTextContent("Tactical Artifact Side Panel");
    expect(screen.getByTestId("mermaid-diagram-card")).toBeInTheDocument();
    expect(screen.getByTestId("mermaid-svg")).toBeInTheDocument();

    const downloadBtn = screen.getByTestId(
      "artifact-download-btn",
    ) as HTMLAnchorElement;
    expect(downloadBtn).toHaveAttribute("download", "spec.md");
    expect(downloadBtn.getAttribute("href")).toContain("download=1");
  });

  it("renders PDF iframe, zoomable image canvas, and numbered code viewer", async () => {
    const pdfPath = "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/docs/report.pdf";
    const imgPath = "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/docs/arch.png";
    const codePath = "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/src/main.rs";

    const apiClient = makeApiClient({
      [pdfPath]: {
        path: pdfPath,
        name: "report.pdf",
        ext: "pdf",
        mime_type: "application/pdf",
        size_bytes: 4096,
        kind: "pdf",
        content: null,
        raw_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(pdfPath)}&mode=raw`,
        download_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(pdfPath)}&download=1`,
      },
      [imgPath]: {
        path: imgPath,
        name: "arch.png",
        ext: "png",
        mime_type: "image/png",
        size_bytes: 2048,
        kind: "image",
        content: null,
        raw_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(imgPath)}&mode=raw`,
        download_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(imgPath)}&download=1`,
      },
      [codePath]: {
        path: codePath,
        name: "main.rs",
        ext: "rs",
        mime_type: "text/plain; charset=utf-8",
        size_bytes: 64,
        kind: "code",
        content: "fn main() {\n    println!(\"hello\");\n}",
        raw_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(codePath)}&mode=raw`,
        download_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(codePath)}&download=1`,
      },
    });

    const { rerender } = render(
      <ArtifactPreviewPane apiClient={apiClient} activePath={pdfPath} />,
    );

    const pdfFrame = await screen.findByTestId("artifact-pdf-frame");
    expect(pdfFrame.getAttribute("src")).toContain("mode=raw");

    rerender(<ArtifactPreviewPane apiClient={apiClient} activePath={imgPath} />);
    const imgView = await screen.findByTestId("artifact-image-preview");
    expect(imgView).toBeInTheDocument();
    const zoom150 = screen.getByTestId("artifact-zoom-150");
    fireEvent.click(zoom150);
    expect(zoom150.className).toContain("fd-btn-active");

    rerender(<ArtifactPreviewPane apiClient={apiClient} activePath={codePath} />);
    const codeView = await screen.findByTestId("artifact-code-viewer");
    expect(codeView).toHaveTextContent("println!");
  });

  it("opens Split View on the Artifacts tab when clicking a file-reference-chip in Chat", async () => {
    const mdPath =
      "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/docs/spec.md";
    const apiClient = makeApiClient({
      [mdPath]: {
        path: mdPath,
        name: "spec.md",
        ext: "md",
        mime_type: "text/plain; charset=utf-8",
        size_bytes: 64,
        kind: "markdown",
        content: "# Opened From Chat Chip",
        raw_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(mdPath)}&mode=raw`,
        download_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(mdPath)}&download=1`,
      },
    });
    const wsStub = {
      setToken: vi.fn(),
      connect: vi.fn(),
      disconnect: vi.fn(),
      onStatusChange: () => () => {},
      subscribe: () => () => {},
    } as unknown as HubWsClient;

    const { unmount } = render(<App apiClient={apiClient} wsClient={wsStub} />);

    const chip = await screen.findByTestId("file-reference-chip");
    expect(chip).toHaveTextContent("spec.md");

    fireEvent.click(chip);

    await waitFor(() => {
      expect(screen.getByTestId("cockpit-workspace").className).toContain(
        "is-split",
      );
      expect(screen.getByTestId("artifact-preview-pane")).toBeInTheDocument();
    });

    expect(await screen.findByTestId("artifact-markdown-body")).toHaveTextContent(
      "Opened From Chat Chip",
    );

    unmount();
  });

  it("preserves local path selection without reverting to activePath, handles malformed mermaid:// URIs, and avoids duplicate window events", async () => {
    const firstPath = "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/docs/first.md";
    const secondPath = "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/docs/second.md";
    const apiClient = makeApiClient({
      [firstPath]: {
        path: firstPath,
        name: "first.md",
        ext: "md",
        mime_type: "text/plain; charset=utf-8",
        size_bytes: 32,
        kind: "markdown",
        content: "# First Document",
        raw_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(firstPath)}&mode=raw`,
        download_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(firstPath)}&download=1`,
      },
      [secondPath]: {
        path: secondPath,
        name: "second.md",
        ext: "md",
        mime_type: "text/plain; charset=utf-8",
        size_bytes: 32,
        kind: "markdown",
        content: "# Second Document",
        raw_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(secondPath)}&mode=raw`,
        download_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(secondPath)}&download=1`,
      },
    });

    const { rerender } = render(
      <ArtifactPreviewPane apiClient={apiClient} activePath={firstPath} />,
    );
    expect(await screen.findByTestId("artifact-markdown-body")).toHaveTextContent(
      "First Document",
    );

    const input = screen.getByTestId("artifact-path-input");
    fireEvent.change(input, { target: { value: secondPath } });
    fireEvent.submit(input.closest("form")!);

    expect(await screen.findByTestId("artifact-markdown-body")).toHaveTextContent(
      "Second Document",
    );

    // Malformed percent sequence in mermaid:// must not throw URIError
    rerender(
      <ArtifactPreviewPane
        apiClient={apiClient}
        activePath="mermaid://graph TD%E0%A4%A"
      />,
    );
    expect(await screen.findByTestId("mermaid-diagram-card")).toBeInTheDocument();

    // dispatchOpenArtifact only dispatches window event when callback is omitted
    const windowSpy = vi.fn();
    window.addEventListener(OPEN_ARTIFACT_EVENT, windowSpy);
    try {
      const cb = vi.fn();
      dispatchOpenArtifact("file:///tmp/test.md", cb);
      expect(cb).toHaveBeenCalledWith("/tmp/test.md");
      expect(windowSpy).not.toHaveBeenCalled();

      dispatchOpenArtifact("file:///tmp/test2.md");
      expect(windowSpy).toHaveBeenCalledTimes(1);
    } finally {
      window.removeEventListener(OPEN_ARTIFACT_EVENT, windowSpy);
    }
  });

  it("filters quick-switch artifacts to User Documents & Media by default and reveals source files when switched to All Files", async () => {
    expect(isUserFacingArtifactPath("docs/spec.md")).toBe(true);
    expect(isUserFacingArtifactPath("assets/hero.png")).toBe(true);
    expect(isUserFacingArtifactPath("docs/report.pdf")).toBe(true);
    expect(isUserFacingArtifactPath("src/main.rs")).toBe(false);
    expect(isUserFacingArtifactPath("ui/src/App.tsx")).toBe(false);

    const mdPath = "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/docs/spec.md";
    const rsPath = "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop/src/main.rs";
    const apiClient = makeApiClient({
      [mdPath]: {
        path: mdPath,
        name: "spec.md",
        ext: "md",
        mime_type: "text/plain; charset=utf-8",
        size_bytes: 32,
        kind: "markdown",
        content: "# Spec Doc",
        raw_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(mdPath)}&mode=raw`,
        download_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(mdPath)}&download=1`,
      },
      [rsPath]: {
        path: rsPath,
        name: "main.rs",
        ext: "rs",
        mime_type: "text/plain; charset=utf-8",
        size_bytes: 32,
        kind: "code",
        content: "fn main() {}",
        raw_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(rsPath)}&mode=raw`,
        download_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(rsPath)}&download=1`,
      },
    });

    render(
      <ArtifactPreviewPane
        apiClient={apiClient}
        activePath={mdPath}
        recentPaths={[mdPath, rsPath]}
      />,
    );

    const filterSelect = screen.getByTestId("artifact-filter-select") as HTMLSelectElement;
    expect(filterSelect.value).toBe("user-docs");

    const switcher = screen.getByTestId("artifact-quick-switcher") as HTMLSelectElement;
    const defaultOptions = Array.from(switcher.querySelectorAll("option")).map(
      (opt) => opt.value,
    );
    expect(defaultOptions).toContain(mdPath);
    expect(defaultOptions).not.toContain(rsPath);

    // Switch filter to "all" -> includes main.rs
    fireEvent.change(filterSelect, { target: { value: "all" } });
    const allOptions = Array.from(switcher.querySelectorAll("option")).map(
      (opt) => opt.value,
    );
    expect(allOptions).toContain(mdPath);
    expect(allOptions).toContain(rsPath);
  });
});
