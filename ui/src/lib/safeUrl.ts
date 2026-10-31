/**
 * URL allow-listing for values that arrive from untrusted data (transcript
 * text written by an agent, artifact metadata) and end up in `href`, `src`
 * or `iframe` attributes.
 *
 * Everything here is an allow-list: anything it cannot positively classify is
 * rejected, so `javascript:`, `vbscript:`, `data:text/html`, `file:` and the
 * many whitespace/control-character spellings of those never pass.
 */

function stripControl(raw: string): string {
  // Browsers drop tab/newline/CR anywhere inside a URL and trim C0 controls
  // and spaces at the ends, so `java\tscript:` is still `javascript:`.
  // eslint-disable-next-line no-control-regex
  return raw.replace(/[\u0000- \u007f-\u009f]+/g, "");
}

/** `/path` only. `//host` and `/\host` are protocol-relative, i.e. another origin. */
function isSameOriginRelative(cleaned: string): boolean {
  return cleaned.startsWith("/") && !/^[/\\]{2}/.test(cleaned) && !cleaned.startsWith("/\\");
}

function schemeOf(cleaned: string): string | null {
  const match = /^([a-zA-Z][a-zA-Z0-9+.-]*):/.exec(cleaned);
  return match ? match[1].toLowerCase() : null;
}

/** Safe as a navigable link target: http(s) or a same-origin relative URL. */
export function isSafeLinkUrl(raw: string | null | undefined): boolean {
  if (!raw) return false;
  const cleaned = stripControl(raw);
  if (!cleaned) return false;
  const scheme = schemeOf(cleaned);
  if (scheme) return scheme === "http" || scheme === "https";
  return isSameOriginRelative(cleaned);
}

const DATA_IMAGE = /^data:image\/(?:png|jpe?g|gif|webp|svg\+xml)[;,]/i;

/**
 * Safe to load automatically as an inline image. Third-party http(s) images
 * are excluded on purpose: an agent that can write transcript text could
 * otherwise make the operator's browser request an attacker URL with
 * conversation data in the query string, with no click.
 */
export function isSafeInlineImageUrl(
  raw: string | null | undefined,
  currentOrigin?: string,
): boolean {
  if (!raw) return false;
  const cleaned = stripControl(raw);
  if (!cleaned) return false;
  const scheme = schemeOf(cleaned);
  if (scheme === "data") return DATA_IMAGE.test(cleaned);
  if (scheme === "blob") return true;
  if (scheme === "http" || scheme === "https") {
    const origin =
      currentOrigin ?? (typeof window !== "undefined" ? window.location?.origin : undefined);
    try {
      return Boolean(origin) && new URL(cleaned).origin === origin;
    } catch {
      return false;
    }
  }
  if (scheme) return false;
  return isSameOriginRelative(cleaned);
}

/**
 * Whether a bearer token may be appended to `url`. Only same-origin
 * (relative, or absolute to the hub origin) URLs qualify.
 */
export function canAttachTokenTo(
  raw: string,
  hubBaseUrl: string,
  currentOrigin?: string,
): boolean {
  const cleaned = stripControl(raw);
  if (!cleaned) return false;
  if (!schemeOf(cleaned)) return isSameOriginRelative(cleaned);
  const origins = new Set<string>();
  const here =
    currentOrigin ?? (typeof window !== "undefined" ? window.location?.origin : undefined);
  if (here) origins.add(here);
  try {
    if (hubBaseUrl) origins.add(new URL(hubBaseUrl, here).origin);
  } catch {
    // An unparsable base URL contributes no trusted origin.
  }
  try {
    return origins.has(new URL(cleaned).origin);
  } catch {
    return false;
  }
}
