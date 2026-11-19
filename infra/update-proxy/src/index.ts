interface Env {
  LATHE_PINGS: AnalyticsEngineDataset;
  GITHUB_TOKEN?: string;
}

const UPSTREAM = "https://api.github.com/repos/paterschris/lathe/releases?per_page=30";
const REGISTRY_UPSTREAM = "https://paterschris.github.io/lathe-extensions/index.json";
const FRESH_SECONDS = 300;
const RETAIN_SECONDS = 86400;
const FETCHED_AT = "x-lathe-proxy-fetched-at";

export function cleanHeader(value: string | null, limit = 64): string {
  return (value ?? "").replace(/[^A-Za-z0-9._-]/g, "").slice(0, limit);
}

type PingKind = "update" | "registry";

function recordPing(request: Request, env: Env, kind: PingKind): void {
  const installation = cleanHeader(request.headers.get("x-lathe-install"));
  if (!installation) return;
  env.LATHE_PINGS.writeDataPoint({
    indexes: [installation],
    blobs: [
      cleanHeader(request.headers.get("x-lathe-version")),
      cleanHeader(request.headers.get("x-lathe-channel")),
      cleanHeader(request.headers.get("x-lathe-os")),
      cleanHeader(request.headers.get("x-lathe-arch")),
      cleanHeader(typeof request.cf?.country === "string" ? request.cf.country : null, 2),
      kind,
    ],
    doubles: [1],
  });
}

// GitHub Pages already serves the registry from a CDN, so this route only
// counts the fetch and passes the index (and its ETag revalidation) through.
async function serveRegistry(request: Request, env: Env, context: ExecutionContext): Promise<Response> {
  context.waitUntil(Promise.resolve().then(() => recordPing(request, env, "registry")).catch(() => {}));

  const headers = new Headers({ "User-Agent": "lathe-update-proxy" });
  const ifNoneMatch = request.headers.get("If-None-Match");
  if (ifNoneMatch) headers.set("If-None-Match", ifNoneMatch);

  try {
    const upstream = await fetch(REGISTRY_UPSTREAM, { headers, signal: AbortSignal.timeout(10000) });
    const responseHeaders = new Headers({ "Cache-Control": "no-store" });
    for (const name of ["Content-Type", "ETag"]) {
      const value = upstream.headers.get(name);
      if (value) responseHeaders.set(name, value);
    }
    const body = upstream.status === 304 ? null : await upstream.arrayBuffer();
    return new Response(body, { status: upstream.status, headers: responseHeaders });
  } catch {
    return new Response("Registry unavailable", { status: 502 });
  }
}

function publicResponse(response: Response): Response {
  const headers = new Headers(response.headers);
  headers.delete(FETCHED_AT);
  // Each update check must reach the Worker even when its release body is cached.
  headers.set("Cache-Control", "no-store");
  return new Response(response.body, { status: response.status, headers });
}

export default {
  async fetch(request: Request, env: Env, context: ExecutionContext): Promise<Response> {
    const pathname = new URL(request.url).pathname;
    if (pathname !== "/releases" && pathname !== "/extensions/index.json") {
      return new Response("Not found", { status: 404 });
    }
    if (request.method !== "GET") {
      return new Response("Method not allowed", { status: 405, headers: { Allow: "GET" } });
    }
    if (pathname === "/extensions/index.json") return serveRegistry(request, env, context);

    context.waitUntil(Promise.resolve().then(() => recordPing(request, env, "update")).catch(() => {}));

    const cache = caches.default;
    const cacheKey = new Request(new URL("/releases", request.url).toString());
    let cached: Response | undefined;
    try {
      cached = await cache.match(cacheKey);
    } catch {
      // Cache failures must not stop the upstream update check.
    }
    const fetchedAt = Number(cached?.headers.get(FETCHED_AT) ?? 0);
    if (cached && Date.now() - fetchedAt < FRESH_SECONDS * 1000) {
      return publicResponse(cached);
    }

    const headers = new Headers({
      Accept: "application/vnd.github+json",
      "User-Agent": "lathe-update-proxy",
      "X-GitHub-Api-Version": "2022-11-28",
    });
    if (env.GITHUB_TOKEN) headers.set("Authorization", `Bearer ${env.GITHUB_TOKEN}`);

    try {
      const upstream = await fetch(UPSTREAM, { headers, signal: AbortSignal.timeout(10000) });
      if (!upstream.ok) return publicResponse(cached ?? upstream);

      // Buffer before caching so a truncated network response cannot poison the cache.
      const body = await upstream.arrayBuffer();
      const responseHeaders = new Headers({
        "Content-Type": upstream.headers.get("Content-Type") ?? "application/json; charset=utf-8",
        "Cache-Control": `public, max-age=${RETAIN_SECONDS}`,
        [FETCHED_AT]: String(Date.now()),
      });
      const response = new Response(body, { status: upstream.status, headers: responseHeaders });
      // Keep a stale copy for outages; freshness is tracked independently of cache expiry.
      context.waitUntil(cache.put(cacheKey, response.clone()).catch(() => {}));
      return publicResponse(response);
    } catch {
      return cached ? publicResponse(cached) : new Response("GitHub unavailable", { status: 502 });
    }
  },
} satisfies ExportedHandler<Env>;
