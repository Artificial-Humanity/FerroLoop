import { useEffect, useState } from "react";

/** First viewport width (px) that is no longer a phone. The only place this number lives. */
export const PHONE_MAX_VIEWPORT_PX = 768;
/** First viewport width (px) that is a desktop. The only place this number lives. */
export const DESKTOP_MIN_VIEWPORT_PX = 1024;

export type DeviceLayout = "phone" | "tablet" | "desktop";
export type DeviceOrientation = "portrait" | "landscape";

export interface DeviceLayoutState {
  layout: DeviceLayout;
  orientation: DeviceOrientation;
  width: number;
}

const SSR_WIDTH = 1200;
const SSR_HEIGHT = 800;

export function resolveDeviceLayout(width: number): DeviceLayout {
  if (width < PHONE_MAX_VIEWPORT_PX) return "phone";
  if (width < DESKTOP_MIN_VIEWPORT_PX) return "tablet";
  return "desktop";
}

export function resolveOrientation(width: number, height: number): DeviceOrientation {
  return height > width ? "portrait" : "landscape";
}

function readDeviceLayout(): DeviceLayoutState {
  const width = typeof window !== "undefined" ? window.innerWidth : SSR_WIDTH;
  const height = typeof window !== "undefined" ? window.innerHeight : SSR_HEIGHT;
  return {
    layout: resolveDeviceLayout(width),
    orientation: resolveOrientation(width, height),
    width,
  };
}

export function useDeviceLayout(): DeviceLayoutState {
  const [state, setState] = useState<DeviceLayoutState>(readDeviceLayout);

  useEffect(() => {
    const update = () => {
      const next = readDeviceLayout();
      setState((prev) =>
        prev.layout === next.layout &&
        prev.orientation === next.orientation &&
        prev.width === next.width
          ? prev
          : next,
      );
    };
    update();
    window.addEventListener("resize", update);
    window.addEventListener("orientationchange", update);
    return () => {
      window.removeEventListener("resize", update);
      window.removeEventListener("orientationchange", update);
    };
  }, []);

  return state;
}
