import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { RichMarkdown } from "../components/RichMarkdown";

describe("RichMarkdown inline code and links", () => {
  it("renders inline code borderless with the soft code colors", () => {
    const { container } = render(<RichMarkdown text="Run `fl status` first." />);

    const code = container.querySelector("code");
    expect(code).not.toBeNull();
    expect(code).toHaveTextContent("fl status");
    // jsdom normalizes `border: none` away; assert the pill outline is gone
    // and the soft theme colors apply instead.
    const el = code as HTMLElement;
    expect(el.style.cssText).toContain("var(--code-inline-bg)");
    expect(el.style.cssText).toContain("var(--code-inline-text)");
    expect(el.style.cssText).not.toContain("1px solid");
  });

  it("linkifies bare, parenthesized, angle-bracketed, and backticked URLs", () => {
    render(
      <RichMarkdown
        text={[
          "Bare https://example.com/a and parens (https://example.com/b).",
          "Angle <https://example.com/c> plus code `https://example.com/d` done.",
        ].join("\n")}
      />,
    );

    for (const path of ["a", "b", "c", "d"]) {
      const link = screen.getByRole("link", {
        name: `https://example.com/${path}`,
      }) as HTMLAnchorElement;
      expect(link.href).toBe(`https://example.com/${path}`);
      expect(link.target).toBe("_blank");
      expect(link.rel).toContain("noopener");
    }
  });

  it("keeps balanced parentheses inside a bare URL path", () => {
    render(
      <RichMarkdown text="See https://en.wikipedia.org/wiki/Foo_(bar) for detail." />,
    );

    const link = screen.getByRole("link", {
      name: "https://en.wikipedia.org/wiki/Foo_(bar)",
    }) as HTMLAnchorElement;
    expect(link.href).toBe("https://en.wikipedia.org/wiki/Foo_(bar)");
  });
});
