import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import worker, { cleanHeader } from "./index";

const body = '[ { "tag_name": "v1.0.0", "assets": [] } ]\n';
const upstreamUrl = "https://api.github.com/repos/paterschris/lathe/releases?per_page=30";
let stored: Response | undefined;
let pending: Promise<unknown>[];
let fetchMock: ReturnType<typeof vi.fn>;
let writeDataPoint: ReturnType<typeof vi.fn>;
let cachePut: ReturnType<typeof vi.fn>;

beforeEach(() => {
  stored = undefined;
  pending = [];
  fetchMock = vi.fn().mockResolvedValue(new Response(body, {
    headers: { "Content-Type": "application/json" },
  }));
  writeDataPoint = vi.fn();
  cachePut = vi.fn(async (_key: Request, response: Response) => { stored = response; });
  vi.stubGlobal("fetch", fetchMock);
  vi.stubGlobal("caches", { default: {
    match: vi.fn(async () => stored?.clone()),
    put: cachePut,
  } });
});
afterEach(() => { vi.unstubAllGlobals(); });

async function check(headers: Record<string, string> = {}, token?: string) {
  const request = new Request("https://lathe.example/releases?ignored=1", { headers });
  Object.defineProperty(request, "cf", { value: { country: "US" } });
  const response = await worker.fetch(request as never, {
    LATHE_PINGS: { writeDataPoint }, GITHUB_TOKEN: token,
  } as never, { waitUntil: (promise: Promise<unknown>) => pending.push(promise) } as never);
  await Promise.all(pending);
  return response;
}

function stale() {
  stored = new Response(body, { headers: {
    "x-lathe-proxy-fetched-at": String(Date.now() - 301000),
    "Content-Type": "application/json",
  } });
}

describe("release proxy", () => {
  it("preserves body bytes and records sanitized metadata without forwarding client headers", async () => {
    const response = await check({
      "x-lathe-install": "a".repeat(64), "x-lathe-version": "1.0+bad!",
      "x-lathe-channel": "stable", "x-lathe-os": "macos", "x-lathe-arch": "aarch64",
    }, "test-secret");
    expect(await response.text()).toBe(body);
    expect(response.headers.get("Content-Type")).toBe("application/json");
    expect(response.headers.get("Cache-Control")).toBe("no-store");
    expect(response.headers.has("x-lathe-proxy-fetched-at")).toBe(false);
    expect(writeDataPoint).toHaveBeenCalledWith({
      indexes: ["a".repeat(64)], blobs: ["1.0bad", "stable", "macos", "aarch64", "US", "update"], doubles: [1],
    });
    expect(fetchMock.mock.calls[0][0]).toBe(upstreamUrl);
    const headers = fetchMock.mock.calls[0][1].headers as Headers;
    expect(headers.get("Authorization")).toBe("Bearer test-secret");
    expect(headers.has("x-lathe-install")).toBe(false);
    expect(cachePut).toHaveBeenCalledOnce();
  });

  it("skips analytics without an install ID and reuses a fresh cache", async () => {
    await check();
    const response = await check();
    expect(await response.text()).toBe(body);
    expect(fetchMock).toHaveBeenCalledOnce();
    expect(writeDataPoint).not.toHaveBeenCalled();
    expect(fetchMock.mock.calls[0][1].headers.has("Authorization")).toBe(false);
  });

  it("records each opted-in check even on a cache hit", async () => {
    await check({ "x-lathe-install": "anonymous" });
    await check({ "x-lathe-install": "anonymous" });
    expect(fetchMock).toHaveBeenCalledOnce();
    expect(writeDataPoint).toHaveBeenCalledTimes(2);
  });

  it("uses stale data when the upstream response body fails", async () => {
    stale();
    fetchMock.mockResolvedValue(new Response(new ReadableStream({
      start(controller) { controller.error(new Error("truncated")); },
    })));
    expect(await (await check()).text()).toBe(body);
    expect(cachePut).not.toHaveBeenCalled();
  });

  it.each([403, 500, 503])("serves stale data on upstream status %s", async (status) => {
    stale();
    fetchMock.mockResolvedValue(new Response("error", { status }));
    const response = await check();
    expect(response.status).toBe(200);
    expect(await response.text()).toBe(body);
  });

  it("refreshes stale data after five minutes", async () => {
    stale();
    fetchMock.mockResolvedValue(new Response("[]"));
    expect(await (await check()).text()).toBe("[]");
    expect(cachePut).toHaveBeenCalledOnce();
  });

  it("serves stale data after network failure", async () => {
    stale();
    fetchMock.mockRejectedValue(new Error("offline"));
    expect(await (await check()).text()).toBe(body);
  });

  it("passes upstream error status through without a cache", async () => {
    fetchMock.mockResolvedValue(new Response("rate limited", { status: 403 }));
    const response = await check();
    expect(response.status).toBe(403);
    expect(await response.text()).toBe("rate limited");
  });

  it("returns 502 on network failure without a cache", async () => {
    fetchMock.mockRejectedValue(new Error("offline"));
    expect((await check()).status).toBe(502);
  });

  it("serves releases despite analytics and cache write failures", async () => {
    writeDataPoint.mockImplementation(() => { throw new Error("quota"); });
    cachePut.mockRejectedValue(new Error("cache unavailable"));
    expect(await (await check({ "x-lathe-install": "anonymous" })).text()).toBe(body);
  });

  it("serves releases despite cache read failures", async () => {
    caches.default.match = vi.fn().mockRejectedValue(new Error("cache unavailable"));
    expect(await (await check()).text()).toBe(body);
  });

  it("rejects unsupported paths and methods without analytics", async () => {
    const context = { waitUntil: vi.fn() };
    for (const [url, method, expected] of [
      ["https://lathe.example/other", "GET", 404],
      ["https://lathe.example/releases", "POST", 405],
    ] as const) {
      const response = await worker.fetch(new Request(url, { method }) as never, {} as never, context as never);
      expect(response.status).toBe(expected);
    }
    expect(context.waitUntil).not.toHaveBeenCalled();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("rejects non-GET registry requests without analytics", async () => {
    const context = { waitUntil: vi.fn() };
    const response = await worker.fetch(
      new Request("https://lathe.example/extensions/index.json", { method: "POST" }) as never,
      {} as never, context as never,
    );
    expect(response.status).toBe(405);
    expect(context.waitUntil).not.toHaveBeenCalled();
  });

  it("filters and bounds untrusted headers", () => {
    expect(cleanHeader("abc /@é._-123")).toBe("abc._-123");
    expect(cleanHeader("a".repeat(1000))).toHaveLength(64);
    expect(cleanHeader(null)).toBe("");
  });
});

describe("registry proxy", () => {
  const registryUrl = "https://paterschris.github.io/lathe-extensions/index.json";
  const index = '{ "schema": 1, "extensions": [] }';

  async function fetchRegistry(headers: Record<string, string> = {}) {
    const request = new Request("https://lathe.example/extensions/index.json", { headers });
    Object.defineProperty(request, "cf", { value: { country: "PT" } });
    const response = await worker.fetch(request as never, {
      LATHE_PINGS: { writeDataPoint },
    } as never, { waitUntil: (promise: Promise<unknown>) => pending.push(promise) } as never);
    await Promise.all(pending);
    return response;
  }

  it("passes the index through and records a registry ping", async () => {
    fetchMock.mockResolvedValue(new Response(index, {
      headers: { "Content-Type": "application/json", ETag: '"v1"' },
    }));
    const response = await fetchRegistry({ "x-lathe-install": "b".repeat(64) });
    expect(await response.text()).toBe(index);
    expect(response.headers.get("ETag")).toBe('"v1"');
    expect(response.headers.get("Cache-Control")).toBe("no-store");
    expect(fetchMock.mock.calls[0][0]).toBe(registryUrl);
    expect((fetchMock.mock.calls[0][1].headers as Headers).has("x-lathe-install")).toBe(false);
    expect(writeDataPoint).toHaveBeenCalledWith({
      indexes: ["b".repeat(64)], blobs: ["", "", "", "", "PT", "registry"], doubles: [1],
    });
    expect(cachePut).not.toHaveBeenCalled();
  });

  it("forwards If-None-Match and returns 304 without a body", async () => {
    fetchMock.mockResolvedValue(new Response(null, { status: 304, headers: { ETag: '"v1"' } }));
    const response = await fetchRegistry({ "If-None-Match": '"v1"' });
    expect(response.status).toBe(304);
    expect((fetchMock.mock.calls[0][1].headers as Headers).get("If-None-Match")).toBe('"v1"');
    expect(writeDataPoint).not.toHaveBeenCalled();
  });

  it("returns 502 when the registry cannot be reached", async () => {
    fetchMock.mockRejectedValue(new Error("offline"));
    expect((await fetchRegistry()).status).toBe(502);
  });
});
