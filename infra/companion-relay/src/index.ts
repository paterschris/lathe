import { DurableObject } from "cloudflare:workers";
import { handleWebhook, type Kv } from "./entitlements";
import { pairPage } from "./pair-page";
import { CLOSE_PAIRING_DONE, DEVICE_ID_PATTERN, ID_PATTERN, Relay, type Role } from "./relay";

interface Env {
  DESKTOP_RELAY: DurableObjectNamespace<DesktopRelay>;
  SUBSCRIPTIONS: KVNamespace;
  EXPO_ACCESS_TOKEN?: string;
  REVENUECAT_SECRET_KEY?: string;
  REVENUECAT_WEBHOOK_AUTH?: string;
  REVENUECAT_DISABLED?: string;
}

// Only the Worker reaches this path: it forwards nothing but `/v1/connect`
// from outside.
const EXPIRE_PATH = "/internal/expire";

export class DesktopRelay extends DurableObject<Env> {
  private relay?: Relay;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // Answered by the runtime without waking the object, so idle connections
    // cost no Durable Object duration. Clients must send exactly this text.
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair('{"type":"ping"}', '{"type":"pong"}'));
    // Waking from hibernation keeps the sockets; a reset (a crash, an eviction,
    // a deploy) drops them all. Zero here while a desktop was connected means the
    // object restarted under it.
    const desktops = ctx.getWebSockets("desktop").length;
    const phones = ctx.getWebSockets("phone").length;
    console.log(`companion relay: object started with ${desktops} desktop and ${phones} phone socket(s)`);
  }

  private core(desktopId: string): Relay {
    this.relay ??= new Relay(this.ctx, { ...this.env, SUBSCRIPTIONS: this.env.SUBSCRIPTIONS as Kv }, desktopId);
    return this.relay;
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === EXPIRE_PATH) {
      (await this.wakeCore()).expireDevice(url.searchParams.get("device_id") ?? "");
      return new Response("ok");
    }
    const desktopId = url.searchParams.get("desktop_id")!;
    const relay = this.core(desktopId);
    // Writes are the scarce Durable Object resource on the Free plan (100,000
    // rows a day across the account), and every reconnect lands here.
    if ((await this.ctx.storage.get<string>("desktop_id")) !== desktopId) {
      await this.ctx.storage.put("desktop_id", desktopId);
    }

    const role = url.searchParams.get("role") as Role;
    const id = role === "desktop" ? desktopId : url.searchParams.get("device_id")!;
    const [client, server] = Object.values(new WebSocketPair());
    if (role === "pairing" && !(await relay.pairingOpen())) {
      console.log(`companion relay: pairing connection from ${id} refused, pairing isn't open`);
      // A close code tells the app the code is stale, where a refused upgrade
      // would look like a network error worth retrying.
      server.accept();
      server.close(CLOSE_PAIRING_DONE, "pairing is not open");
      return new Response(null, { status: 101, webSocket: client });
    }

    this.ctx.acceptWebSocket(server, [role]);
    relay.open(server, role, id);
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(socket: WebSocket, message: string | ArrayBuffer): Promise<void> {
    await (await this.wakeCore()).message(socket, message);
  }

  async alarm(): Promise<void> {
    await (await this.wakeCore()).sweep();
  }

  async webSocketClose(socket: WebSocket, code: number, reason: string, wasClean: boolean): Promise<void> {
    (await this.wakeCore()).close(socket, { code, reason, wasClean });
  }

  async webSocketError(socket: WebSocket, error: unknown): Promise<void> {
    (await this.wakeCore()).close(socket, { error: error instanceof Error ? error.message : String(error) });
  }

  // After hibernation the object is rebuilt without the request that created
  // it, so the desktop id comes back from storage.
  private async wakeCore(): Promise<Relay> {
    if (this.relay) return this.relay;
    return this.core((await this.ctx.storage.get<string>("desktop_id"))!);
  }
}

export default {
  async fetch(request: Request, env: Env, context: ExecutionContext): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/health") return new Response("ok");
    if (url.pathname === "/pair" && request.method === "GET") return pairPage();
    if (url.pathname === "/v1/revenuecat") {
      return handleWebhook(request, {
        env: { ...env, SUBSCRIPTIONS: env.SUBSCRIPTIONS as Kv },
        waitUntil: (promise) => context.waitUntil(promise),
        async expireOnDesktop(desktopId, deviceId) {
          const stub = env.DESKTOP_RELAY.get(env.DESKTOP_RELAY.idFromName(desktopId));
          const target = new URL(`https://relay${EXPIRE_PATH}`);
          target.searchParams.set("device_id", deviceId);
          await stub.fetch(target, { method: "POST" });
        },
      });
    }
    if (url.pathname !== "/v1/connect") return new Response("not found", { status: 404 });
    if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
      return new Response("expected a websocket upgrade", { status: 426 });
    }

    const desktopId = url.searchParams.get("desktop_id") ?? "";
    const role = url.searchParams.get("role");
    const deviceId = url.searchParams.get("device_id") ?? "";
    const pairing = url.searchParams.get("pairing") === "1";
    if (!ID_PATTERN.test(desktopId) || (role !== "desktop" && role !== "phone")) {
      return new Response("invalid connection parameters", { status: 400 });
    }
    if (role === "phone" && !DEVICE_ID_PATTERN.test(deviceId)) {
      return new Response("invalid device id", { status: 400 });
    }

    const forwarded = new URL(request.url);
    forwarded.searchParams.set("role", role === "phone" && pairing ? "pairing" : role);
    const stub = env.DESKTOP_RELAY.get(env.DESKTOP_RELAY.idFromName(desktopId));
    return stub.fetch(new Request(forwarded, request));
  },
};
