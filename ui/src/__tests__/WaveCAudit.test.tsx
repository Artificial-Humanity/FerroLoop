// Regression tests from the 2026-10-30 Wave C audit (docs/audits/2026-10-30-wave-c-cloud-audit.md).
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  HubApiClient,
  LAST_PASSKEY_ID_STORAGE_KEY,
  PASSKEY_KEYSTORE_STORAGE_KEY,
  base64UrlToBytes,
  bytesToBase64Url,
  generatePasskeyCredentialId,
  getLastRegisteredPasskeyId,
  p1363ToDer,
  resolvePasskeyOrigin,
} from "../api/client";
import { SettingsModal } from "../components/SettingsModal";

const hex = (h: string): Uint8Array =>
  new Uint8Array(h.match(/../g)?.map((b) => parseInt(b, 16)) ?? []);

/** The SEC1 uncompressed point as a WebCrypto verify key. */
function importPublicKey(raw: Uint8Array): Promise<CryptoKey> {
  return crypto.subtle.importKey(
    "jwk",
    {
      kty: "EC",
      crv: "P-256",
      x: bytesToBase64Url(raw.subarray(1, 33)),
      y: bytesToBase64Url(raw.subarray(33, 65)),
    },
    { name: "ECDSA", namedCurve: "P-256" },
    false,
    ["verify"],
  );
}

const concat = (...parts: Uint8Array[]): Uint8Array => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
};

const sha256 = async (data: Uint8Array): Promise<Uint8Array> =>
  new Uint8Array(await crypto.subtle.digest("SHA-256", data as BufferSource));

/**
 * A strict DER ECDSA signature reader, as strict as ring's: SEQUENCE of two minimal positive
 * INTEGERs, short-form lengths, nothing after. Returns the 64-byte r||s WebCrypto wants, or
 * null for anything the hub would refuse.
 */
function strictDerToP1363(der: Uint8Array): Uint8Array | null {
  if (der.length < 8 || der[0] !== 0x30 || der[1] >= 0x80 || der[1] !== der.length - 2) return null;
  let at = 2;
  const ints: Uint8Array[] = [];
  for (let i = 0; i < 2; i++) {
    if (der[at] !== 0x02) return null;
    const len = der[at + 1];
    if (len === 0 || len >= 0x80 || at + 2 + len > der.length) return null;
    const body = der.subarray(at + 2, at + 2 + len);
    if (body[0] & 0x80) return null; // negative
    if (body.length > 1 && body[0] === 0 && !(body[1] & 0x80)) return null; // not minimal
    const digits = body[0] === 0 ? body.subarray(1) : body;
    if (digits.length > 32) return null;
    const padded = new Uint8Array(32);
    padded.set(digits, 32 - digits.length);
    ints.push(padded);
    at += 2 + len;
  }
  return at === der.length ? concat(ints[0], ints[1]) : null;
}

async function verifyDer(key: CryptoKey, der: Uint8Array, signed: Uint8Array): Promise<boolean> {
  const raw = strictDerToP1363(der);
  return raw !== null && crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, key, raw as BufferSource, signed as BufferSource);
}

type Call = { url: string; init: RequestInit; body: Record<string, unknown> };

function recordingFetch(
  handler: (url: string, body: Record<string, unknown>) => Response,
) {
  const calls: Call[] = [];
  const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const body = init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {};
    calls.push({ url, init: init ?? {}, body });
    return handler(url, body);
  });
  return { calls, fetchImpl: fetchImpl as unknown as typeof fetch };
}

const ok = (body: unknown = { ok: true }) =>
  new Response(JSON.stringify(body), { status: 200 });

beforeEach(() => {
  window.localStorage.clear();
});
afterEach(() => {
  vi.restoreAllMocks();
});

describe("p1363ToDer — WebCrypto r||s to ASN.1 DER", () => {
  const der = (r: string, s: string) => p1363ToDer(hex(r + s));
  const pad = (v: string) => v.padStart(64, "0");

  it("encodes small, high-bit and zero-led integers minimally", () => {
    // r = 1, s = 1.
    expect(Array.from(der(pad("01"), pad("01")))).toEqual([0x30, 6, 2, 1, 1, 2, 1, 1]);
    // High bit set: a 0x00 is prepended so the integer stays positive.
    const hi = der("ff".repeat(32), "80" + "00".repeat(31));
    expect(hi[0]).toBe(0x30);
    expect(hi[1]).toBe(hi.length - 2);
    expect(Array.from(hi.subarray(2, 5))).toEqual([0x02, 33, 0x00]);
    // Leading zero bytes are stripped to the minimal form.
    const lead = der("0000" + "7f".repeat(30), "00" + "01".repeat(31));
    expect(Array.from(lead.subarray(2, 4))).toEqual([0x02, 30]);
    expect(lead[4]).toBe(0x7f);
    // The longest form: both integers 33 bytes, total 70, still short-form length.
    const longest = der("80" + "11".repeat(31), "80" + "22".repeat(31));
    expect(longest.length).toBe(72);
    expect(longest[1]).toBe(70);
  });

  it("returns a value that is not 64 bytes unchanged, which the hub then refuses", () => {
    for (const len of [0, 1, 63, 65, 70, 72]) {
      const input = new Uint8Array(len).fill(7);
      expect(p1363ToDer(input)).toBe(input);
    }
  });

  it("round-trips real WebCrypto signatures through a strict DER verifier", async () => {
    const pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, [
      "sign",
      "verify",
    ]);
    const raw = new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey));
    const key = await importPublicKey(raw);
    const lengths = new Set<number>();
    for (let i = 0; i < 120; i++) {
      const msg = new TextEncoder().encode(`message ${i}`);
      const sig = new Uint8Array(
        await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, pair.privateKey, msg),
      );
      const derSig = p1363ToDer(sig);
      lengths.add(derSig.length);
      expect(await verifyDer(key, derSig, msg)).toBe(true);
      // And the strict reader inverts the encoder exactly.
      expect(Array.from(strictDerToP1363(derSig)!)).toEqual(Array.from(sig));
      // The raw form is not DER.
      expect(await verifyDer(key, sig, msg)).toBe(false);
    }
    // Different lengths occurred, so the high-bit and short-integer branches ran.
    expect(lengths.size).toBeGreaterThan(1);
  });
});

describe("base64url helpers", () => {
  it("round-trips every byte value and the empty input", () => {
    const all = new Uint8Array(256).map((_, i) => i);
    expect(Array.from(base64UrlToBytes(bytesToBase64Url(all)))).toEqual(Array.from(all));
    expect(bytesToBase64Url(new Uint8Array())).toBe("");
    expect(base64UrlToBytes("").length).toBe(0);
    const out = bytesToBase64Url(new Uint8Array([0xfb, 0xff, 0xfe]));
    expect(out).toBe("-__-");
    expect(out).not.toMatch(/[+/=]/);
  });

  it("accepts padded and standard-alphabet input and rejects garbage", () => {
    expect(Array.from(base64UrlToBytes("-__-"))).toEqual([0xfb, 0xff, 0xfe]);
    expect(Array.from(base64UrlToBytes("+//+"))).toEqual([0xfb, 0xff, 0xfe]);
    expect(Array.from(base64UrlToBytes("AQ=="))).toEqual([1]);
    expect(Array.from(base64UrlToBytes(" AQ "))).toEqual([1]);
    expect(() => base64UrlToBytes("!!!!")).toThrow();
  });
});

describe("generatePasskeyCredentialId", () => {
  it("is 32 unguessable characters the hub accepts, and never repeats", () => {
    const seen = new Set<string>();
    for (let i = 0; i < 200; i++) {
      const id = generatePasskeyCredentialId();
      expect(id).toMatch(/^[A-Za-z0-9_-]{32}$/);
      seen.add(id);
    }
    expect(seen.size).toBe(200);
  });
});

describe("HubApiClient.request — URL building", () => {
  it("never lets a path become a URL for another host", async () => {
    const { calls, fetchImpl } = recordingFetch(() => ok({}));
    window.localStorage.setItem("fl_hub_session_token", "tok-secret");
    const same = new HubApiClient({ fetchImpl });
    const cross = new HubApiClient({ fetchImpl, baseUrl: "http://hub.test:8420/" });
    for (const [path, sameWant, crossWant] of [
      ["/api/v1/health", "/api/v1/health", "http://hub.test:8420/api/v1/health"],
      ["api/v1/health", "/api/v1/health", "http://hub.test:8420/api/v1/health"],
      ["//evil.test/x", "/evil.test/x", "http://hub.test:8420/evil.test/x"],
      ["///evil.test/x", "/evil.test/x", "http://hub.test:8420/evil.test/x"],
      ["/\\evil.test/x", "/evil.test/x", "http://hub.test:8420/evil.test/x"],
      ["\\\\evil.test/x", "/evil.test/x", "http://hub.test:8420/evil.test/x"],
    ] as const) {
      await same.request(path);
      await cross.request(path);
      expect(calls[calls.length - 2].url).toBe(sameWant);
      expect(calls[calls.length - 1].url).toBe(crossWant);
    }
    // The session token only ever travels to the configured base.
    for (const call of calls) {
      expect(call.url.startsWith("/") || call.url.startsWith("http://hub.test:8420/")).toBe(true);
      expect(call.url.startsWith("//")).toBe(false);
    }
  });

  it("trims trailing slashes from baseUrl and keeps a base path", async () => {
    const { calls, fetchImpl } = recordingFetch(() => ok({}));
    await new HubApiClient({ fetchImpl, baseUrl: "https://h.test/prefix///" }).request("/x");
    expect(calls[0].url).toBe("https://h.test/prefix/x");
  });

  it("sends same-origin credentials by default, so a cross-origin base gets no ambient cookie", async () => {
    const { calls, fetchImpl } = recordingFetch(() => ok({}));
    await new HubApiClient({ fetchImpl, baseUrl: "https://h.test" }).request("/x");
    expect(calls[0].init.credentials).toBe("same-origin");
  });
});

describe("passkey keystore", () => {
  const registerOk = () =>
    recordingFetch((url) =>
      url.endsWith("/passkey/register") ? ok({ ok: true }) : ok({}),
    );

  it("keeps the private key only after the hub accepted the registration", async () => {
    const refused = recordingFetch((url) =>
      url.endsWith("/passkey/register")
        ? new Response(JSON.stringify({ error: "that credential is already registered" }), {
            status: 409,
          })
        : ok({}),
    );
    const client = new HubApiClient({ fetchImpl: refused.fetchImpl });
    await expect(
      client.registerPasskey({ credential_id: "orphan-credential-0001", label: "x" }),
    ).rejects.toThrow(/already registered/);
    expect(window.localStorage.getItem(PASSKEY_KEYSTORE_STORAGE_KEY)).toBeNull();
    expect(window.localStorage.getItem(LAST_PASSKEY_ID_STORAGE_KEY)).toBeNull();

    const accepted = registerOk();
    const good = new HubApiClient({ fetchImpl: accepted.fetchImpl });
    await good.registerPasskey({ credential_id: "good-credential-0000001", label: "x" });
    expect(getLastRegisteredPasskeyId()).toBe("good-credential-0000001");

    // A later refusal does not displace the credential that works.
    const again = new HubApiClient({ fetchImpl: refused.fetchImpl });
    await expect(
      again.registerPasskey({ credential_id: "orphan-credential-0002", label: "y" }),
    ).rejects.toThrow();
    expect(getLastRegisteredPasskeyId()).toBe("good-credential-0000001");
    const keystore = JSON.parse(window.localStorage.getItem(PASSKEY_KEYSTORE_STORAGE_KEY) ?? "{}");
    expect(Object.keys(keystore)).toEqual(["good-credential-0000001"]);
  });

  it("pins the same origin at registration that it asserts at login", async () => {
    const { calls, fetchImpl } = recordingFetch((url) => {
      if (url.endsWith("/passkey/challenge")) return ok({ challenge: "c".repeat(43), expires_in_secs: 300 });
      if (url.endsWith("/auth/login")) return ok({ ok: true, session_token: "s" });
      return ok({ ok: true });
    });
    // A hub on another origin than the page, which is the case where the hub's `Host` guess differs.
    const client = new HubApiClient({ fetchImpl, baseUrl: "https://hub.example.test:8443" });
    const id = generatePasskeyCredentialId();
    await client.registerPasskey({ credential_id: id, label: "k" });
    await client.loginWithPasskey({ credential_id: id });
    const register = calls.find((c) => c.url.endsWith("/passkey/register"))!;
    const login = calls.find((c) => c.url.endsWith("/auth/login"))!;
    const clientData = JSON.parse(
      new TextDecoder().decode(base64UrlToBytes(String(login.body.client_data_json_b64))),
    );
    expect(register.body.origin).toBe(window.location.origin);
    expect(clientData.origin).toBe(register.body.origin);
    expect(resolvePasskeyOrigin("https://hub.example.test:8443").origin).toBe(window.location.origin);
  });

  it("falls back to baseUrl for the origin only when there is no page origin", () => {
    const real = window.location;
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { origin: "null", hostname: "" },
    });
    try {
      expect(resolvePasskeyOrigin("https://Hub.Example.test:8443/path")).toEqual({
        origin: "https://hub.example.test:8443",
        hostname: "hub.example.test",
      });
      expect(resolvePasskeyOrigin("not a url")).toEqual({
        origin: "http://localhost:9000",
        hostname: "localhost",
      });
      expect(resolvePasskeyOrigin("")).toEqual({
        origin: "http://localhost:9000",
        hostname: "localhost",
      });
    } finally {
      Object.defineProperty(window, "location", { configurable: true, value: real });
    }
  });

  it("signs an assertion the hub's checks would accept: DER signature over authData || SHA-256(clientData)", async () => {
    const { calls, fetchImpl } = recordingFetch((url) => {
      if (url.endsWith("/passkey/challenge")) return ok({ challenge: "n".repeat(43), expires_in_secs: 300 });
      if (url.endsWith("/auth/login")) return ok({ ok: true, session_token: "s" });
      return ok({ ok: true });
    });
    const client = new HubApiClient({ fetchImpl });
    const id = generatePasskeyCredentialId();
    await client.registerPasskey({ credential_id: id, label: "k" });
    const publicKey = base64UrlToBytes(
      String(calls.find((c) => c.url.endsWith("/passkey/register"))!.body.public_key_b64),
    );
    expect(publicKey.length).toBe(65);
    expect(publicKey[0]).toBe(0x04);
    const key = await importPublicKey(publicKey);

    const counters: number[] = [];
    for (let i = 0; i < 3; i++) {
      await client.loginWithPasskey({ credential_id: id });
      const body = calls.filter((c) => c.url.endsWith("/auth/login"))[i].body;
      const clientData = base64UrlToBytes(String(body.client_data_json_b64));
      const authData = base64UrlToBytes(String(body.authenticator_data_b64));
      const signature = base64UrlToBytes(String(body.signature_b64));
      expect(body.credential_id).toBe(id);
      // authenticatorData: rpIdHash(32) || flags(1) || signCount(4, big-endian).
      expect(authData.length).toBe(37);
      expect(Array.from(authData.subarray(0, 32))).toEqual(
        Array.from(await sha256(new TextEncoder().encode(window.location.hostname))),
      );
      expect(authData[32]).toBe(0x01);
      counters.push(new DataView(authData.buffer, authData.byteOffset).getUint32(33, false));
      const signed = concat(authData, await sha256(clientData));
      expect(await verifyDer(key, signature, signed)).toBe(true);
      const cd = JSON.parse(new TextDecoder().decode(clientData));
      expect(cd).toEqual({
        type: "webauthn.get",
        challenge: "n".repeat(43),
        origin: window.location.origin,
        crossOrigin: false,
      });
    }
    // Strictly increasing, as the hub requires, and persisted across a new client.
    expect(counters).toEqual([1, 2, 3]);
    const second = new HubApiClient({ fetchImpl });
    await second.loginWithPasskey({ credential_id: id });
    const last = calls.filter((c) => c.url.endsWith("/auth/login")).pop()!.body;
    const lastAuth = base64UrlToBytes(String(last.authenticator_data_b64));
    expect(new DataView(lastAuth.buffer, lastAuth.byteOffset).getUint32(33, false)).toBe(4);
  });

  it("names the credential whose key signed when the requested one is unknown", async () => {
    const { calls, fetchImpl } = recordingFetch((url) => {
      if (url.endsWith("/passkey/challenge")) return ok({ challenge: "n".repeat(43), expires_in_secs: 300 });
      return ok({ ok: true, session_token: "s" });
    });
    const client = new HubApiClient({ fetchImpl });
    await client.registerPasskey({ credential_id: "the-one-registered-key-01", label: "k" });
    await client.loginWithPasskey({ credential_id: "some-other-credential-id-02" });
    const login = calls.filter((c) => c.url.endsWith("/auth/login")).pop()!;
    expect(login.body.credential_id).toBe("the-one-registered-key-01");
  });

  it("survives a corrupt, hostile or unavailable keystore without throwing or polluting prototypes", async () => {
    // A fresh copy of the module: keys registered by earlier tests live in its in-memory map.
    vi.resetModules();
    const fresh = await import("../api/client");
    const { calls, fetchImpl } = recordingFetch(() => ok({ ok: true }));
    const client = new fresh.HubApiClient({ fetchImpl });
    const id = "unregistered-credential-00001";
    for (const raw of [
      "{not json",
      "null",
      "[]",
      "42",
      '"str"',
      JSON.stringify({ [id]: "not an object" }),
      JSON.stringify({ [id]: { credential_id: id } }),
      JSON.stringify({ [id]: { private_jwk: null } }),
      '{"__proto__":{"private_jwk":{"kty":"EC"},"credential_id":"x"}}',
      '{"constructor":{"private_jwk":{"kty":"EC"}}}',
    ]) {
      window.localStorage.setItem(PASSKEY_KEYSTORE_STORAGE_KEY, raw);
      calls.length = 0;
      await expect(client.loginWithPasskey({ credential_id: id })).resolves.toBeTruthy();
      // No usable key: the request goes through unsigned, and the hub refuses it.
      expect(calls.map((c) => c.url)).toEqual(["/api/v1/auth/login"]);
      expect(calls[0].body).toEqual({ mode: "passkey", credential_id: id });
      expect(({} as Record<string, unknown>).private_jwk).toBeUndefined();
    }
    // A throwing localStorage is the sandboxed-iframe case.
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("denied");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("denied");
    });
    expect(fresh.getLastRegisteredPasskeyId().length).toBeGreaterThanOrEqual(16);
    await client.registerPasskey({ credential_id: "sandboxed-credential-0001", label: "k" });
    calls.length = 0;
    await expect(client.loginWithPasskey({ credential_id: "sandboxed-credential-0001" })).resolves.toBeTruthy();
    // The key lived in memory for this page only, so the login was signed.
    expect(calls[calls.length - 1].body.signature_b64).toBeTypeOf("string");
    vi.resetModules();
  });

  it("falls back to a sixteen-plus character default id when nothing is stored", () => {
    expect(getLastRegisteredPasskeyId().length).toBeGreaterThanOrEqual(16);
    window.localStorage.setItem(LAST_PASSKEY_ID_STORAGE_KEY, "short");
    expect(getLastRegisteredPasskeyId().length).toBeGreaterThanOrEqual(16);
    window.localStorage.setItem(LAST_PASSKEY_ID_STORAGE_KEY, "  a-valid-credential-id-001  ");
    expect(getLastRegisteredPasskeyId()).toBe("a-valid-credential-id-001");
  });
});

describe("SettingsModal — TOTP and passkey enrolment trust the hub, not a default", () => {
  async function open(client: HubApiClient) {
    vi.spyOn(client, "listOperators").mockResolvedValue({ operators: [] });
    render(<SettingsModal isOpen={true} onClose={vi.fn()} apiClient={client} />);
    return screen.getByTestId("settings-operators-section");
  }

  it("shows no secret when the hub returns none, and never a hard-coded one", async () => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    const section = await open(client);
    for (const bad of [{}, { secret_base32: "" , otpauth_uri: "x" }, { secret_base32: "ABC" }]) {
      vi.spyOn(client, "enrollTotp").mockResolvedValue(bad as never);
      fireEvent.click(within(section).getByTestId("enroll-totp-btn"));
      await waitFor(() => {
        expect(screen.getByText(/did not return a TOTP secret/i)).toBeInTheDocument();
      });
      expect(within(section).queryByTestId("totp-secret-display")).not.toBeInTheDocument();
      expect(document.body.textContent).not.toContain("JBSWY3DPEHPK3PXP");
      vi.restoreAllMocks();
      vi.spyOn(client, "listOperators").mockResolvedValue({ operators: [] });
    }
  });

  it("does not mark TOTP verified unless the hub said ok:true", async () => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    const section = await open(client);
    vi.spyOn(client, "enrollTotp").mockResolvedValue({
      secret_base32: "NB2W45DFOIZANB2W45DFOIZA",
      otpauth_uri: "otpauth://totp/FerroLoop:x?secret=NB2W45DFOIZANB2W45DFOIZA",
    });
    fireEvent.click(within(section).getByTestId("enroll-totp-btn"));
    await within(section).findByTestId("totp-secret-display");
    const verify = vi.spyOn(client, "verifyTotp").mockResolvedValue({} as never);
    fireEvent.change(within(section).getByTestId("totp-verify-input"), { target: { value: "123456" } });
    fireEvent.click(within(section).getByTestId("totp-verify-btn"));
    await waitFor(() => expect(verify).toHaveBeenCalledWith("123456"));
    await screen.findByText(/did not confirm the code/i);
    expect(within(section).queryByTestId("totp-verified-badge")).not.toBeInTheDocument();
  });

  it("registers passkeys under random credential ids", async () => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    const section = await open(client);
    const register = vi.spyOn(client, "registerPasskey").mockResolvedValue({ ok: true });
    for (let i = 0; i < 3; i++) {
      fireEvent.click(within(section).getByTestId("register-passkey-btn"));
      await waitFor(() => expect(register).toHaveBeenCalledTimes(i + 1));
    }
    const ids = register.mock.calls.map(([req]) => req.credential_id);
    expect(new Set(ids).size).toBe(3);
    for (const id of ids) {
      expect(id).toMatch(/^[A-Za-z0-9_-]{32}$/);
      expect(id).not.toMatch(/^passkey-\d+$/);
    }
  });
});
