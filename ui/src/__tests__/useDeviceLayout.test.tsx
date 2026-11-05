import { act, fireEvent, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  DESKTOP_MIN_VIEWPORT_PX,
  PHONE_MAX_VIEWPORT_PX,
  resolveDeviceLayout,
  resolveOrientation,
  useDeviceLayout,
} from "../hooks/useDeviceLayout";

function setViewport(width: number, height: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, writable: true, value: width });
  Object.defineProperty(window, "innerHeight", { configurable: true, writable: true, value: height });
}

describe("resolveDeviceLayout", () => {
  it("exposes the shared thresholds", () => {
    expect(PHONE_MAX_VIEWPORT_PX).toBe(768);
    expect(DESKTOP_MIN_VIEWPORT_PX).toBe(1024);
  });

  it("splits phone and tablet at 767/768", () => {
    expect(resolveDeviceLayout(320)).toBe("phone");
    expect(resolveDeviceLayout(767)).toBe("phone");
    expect(resolveDeviceLayout(768)).toBe("tablet");
  });

  it("splits tablet and desktop at 1023/1024", () => {
    expect(resolveDeviceLayout(1023)).toBe("tablet");
    expect(resolveDeviceLayout(1024)).toBe("desktop");
    expect(resolveDeviceLayout(1920)).toBe("desktop");
  });
});

describe("resolveOrientation", () => {
  it("is portrait only when height exceeds width", () => {
    expect(resolveOrientation(768, 1024)).toBe("portrait");
    expect(resolveOrientation(1024, 768)).toBe("landscape");
    expect(resolveOrientation(800, 800)).toBe("landscape");
  });
});

describe("useDeviceLayout", () => {
  const originalWidth = window.innerWidth;
  const originalHeight = window.innerHeight;

  afterEach(() => {
    setViewport(originalWidth, originalHeight);
    vi.restoreAllMocks();
  });

  it("reads the initial viewport", () => {
    setViewport(800, 1200);
    const { result } = renderHook(() => useDeviceLayout());
    expect(result.current).toEqual({ layout: "tablet", orientation: "portrait", width: 800 });
  });

  it("updates on resize across both boundaries", () => {
    setViewport(1280, 800);
    const { result } = renderHook(() => useDeviceLayout());
    expect(result.current.layout).toBe("desktop");

    act(() => {
      setViewport(1023, 800);
      fireEvent(window, new Event("resize"));
    });
    expect(result.current.layout).toBe("tablet");
    expect(result.current.orientation).toBe("landscape");

    act(() => {
      setViewport(767, 800);
      fireEvent(window, new Event("resize"));
    });
    expect(result.current.layout).toBe("phone");
    expect(result.current.width).toBe(767);
  });

  it("updates on orientationchange", () => {
    setViewport(1024, 768);
    const { result } = renderHook(() => useDeviceLayout());
    expect(result.current.orientation).toBe("landscape");

    act(() => {
      setViewport(768, 1024);
      fireEvent(window, new Event("orientationchange"));
    });
    expect(result.current).toEqual({ layout: "tablet", orientation: "portrait", width: 768 });
  });

  it("removes its listeners on unmount", () => {
    const add = vi.spyOn(window, "addEventListener");
    const remove = vi.spyOn(window, "removeEventListener");
    const { unmount } = renderHook(() => useDeviceLayout());

    const added = add.mock.calls.filter(([type]) => type === "resize" || type === "orientationchange");
    expect(added.map(([type]) => type).sort()).toEqual(["orientationchange", "resize"]);

    unmount();
    for (const [type, handler] of added) {
      expect(remove).toHaveBeenCalledWith(type, handler);
    }
  });
});
