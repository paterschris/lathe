import { beforeEach, describe, expect, it, vi } from "vitest";
import { pairPage } from "./pair-page";
import {
  OUTAGE_GRACE_MS, affectedDeviceIds, handleWebhook, type Kv,
} from "./entitlements";
import {
  STALE_SOCKET_MS, SWEEP_INTERVAL_MS,
  CLOSE_PAIRING_DONE, CLOSE_SUBSCRIPTION_REQUIRED, CLOSE_REPLACED, CLOSE_REVOKED, CLOSE_UNAUTHORIZED, MAX_DEVICES, PUSHES_PER_HOUR, Relay,
  base64ToBytes, bytesToBase64, idForPublicKey, type Attachment, type Role, type Socket,
} from "./relay";

class FakeSocket implements Socket {
  sent: Record<string, unknown>[] = [];
  closed?: { code?: number; reason?: string };
  // When the runtime last answered this socket's ping.
  lastPing: number | null = null;
  private attachment: unknown = null;
  constructor(public role: Role) {}
  send(message: string) { this.sent.push(JSON.parse(message)); }
  close(code?: number, reason?: string) { this.closed = { code, reason }; }
  serializeAttachment(value: unknown) { this.attachment = structuredClone(value); }
  deserializeAttachment() { return this.attachment; }
  last() { return this.sent.at(-1); }
  types() { return this.sent.map((message) => message.type); }
}

class FakeState {
  sockets: FakeSocket[] = [];
  data = new Map<string, unknown>();
  alarm: number | null = null;
  getWebSockets(tag?: string) {
    return this.sockets.filter((socket) => !socket.closed && (!tag || socket.role === tag));
  }
  getWebSocketAutoResponseTimestamp(socket: Socket) {
    const ping = (socket as FakeSocket).lastPing;
    return ping === null ? null : new Date(ping);
  }
  storage = {
    getAlarm: async () => this.alarm,
    setAlarm: async (time: number) => { this.alarm = time; },
    get: async <T>(key: string) => structuredClone(this.data.get(key)) as T | undefined,
    put: async (key: string, value: unknown) => { this.data.set(key, structuredClone(value)); },
    delete: async (key: string) => this.data.delete(key),
    list: async <T>({ prefix }: { prefix: string }) =>
      new Map([...this.data].filter(([key]) => key.startsWith(prefix)) as [string, T][]),
  };
}

interface Identity { publicKey: string; id: string; sign(nonce: string): Promise<string> }

async function identity(): Promise<Identity> {
  const pair = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"])) as CryptoKeyPair;
  const raw = new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey));
  return {
    publicKey: bytesToBase64(raw),
    id: await idForPublicKey(raw),
    async sign(nonce) {
      const data = base64ToBytes(nonce);
      return bytesToBase64(new Uint8Array(await crypto.subtle.sign({ name: "Ed25519" }, pair.privateKey, data)));
    },
  };
}

let state: FakeState;
let relay: Relay;
let desktop: Identity;
let now: number;
let fetcher: ReturnType<typeof vi.fn>;

function connect(role: Role, id: string): FakeSocket {
  const socket = new FakeSocket(role);
  state.sockets.push(socket);
  relay.open(socket, role, id);
  return socket;
}

async function say(socket: FakeSocket, message: Record<string, unknown>) {
  await relay.message(socket, JSON.stringify(message));
}

function nonceOf(socket: FakeSocket): string {
  return (socket.deserializeAttachment() as Attachment).nonce;
}

async function connectDesktop(): Promise<FakeSocket> {
  const socket = connect("desktop", desktop.id);
  await say(socket, { type: "auth", public_key: desktop.publicKey, signature: await desktop.sign(nonceOf(socket)) });
  return socket;
}

// Phone ids are app-chosen, not derived from the key.
let nextPhone = 0;
async function pairPhone(desktopSocket: FakeSocket, name = "Pixel"): Promise<Identity> {
  const phone = { ...(await identity()), id: `phone${String(++nextPhone).padStart(4, "0")}` };
  await say(desktopSocket, { type: "authorize_device", device_id: phone.id, sign_pk: phone.publicKey, name });
  return phone;
}

async function connectPhone(phone: Identity): Promise<FakeSocket> {
  const socket = connect("phone", phone.id);
  await say(socket, { type: "auth", signature: await phone.sign(nonceOf(socket)) });
  return socket;
}

const frame = { type: "frame", seq: 1, nonce: "bm9uY2U=", ciphertext: "c2VjcmV0" };

beforeEach(async () => {
  state = new FakeState();
  desktop = await identity();
  now = 1_800_000_000_000;
  fetcher = vi.fn();
  // The routing tests run with the subscription gate off, as local
  // development does. The gate has its own tests below.
  relay = new Relay(state, { REVENUECAT_DISABLED: "1" }, desktop.id, () => now, fetcher as unknown as typeof fetch);
});

// The app backgrounded: the phone keeps its push token but has no socket.
function disconnect(socket: FakeSocket) {
  socket.close(1000, "app backgrounded");
  relay.close(socket);
}

describe("authentication", () => {
  it("accepts a desktop that signs the challenge with the key behind its id", async () => {
    const socket = await connectDesktop();
    expect(socket.types()).toEqual(["challenge", "auth_ok"]);
    expect(socket.closed).toBeUndefined();
  });

  it("rejects a desktop key that does not hash to the desktop id", async () => {
    const impostor = await identity();
    const socket = connect("desktop", desktop.id);
    await say(socket, { type: "auth", public_key: impostor.publicKey, signature: await impostor.sign(nonceOf(socket)) });
    expect(socket.closed?.code).toBe(CLOSE_UNAUTHORIZED);
  });

  it("rejects a signature over a different nonce", async () => {
    const socket = connect("desktop", desktop.id);
    await say(socket, { type: "auth", public_key: desktop.publicKey, signature: await desktop.sign(bytesToBase64(new Uint8Array(32))) });
    expect(socket.closed?.code).toBe(CLOSE_UNAUTHORIZED);
  });

  it("rejects phones that were never paired", async () => {
    const stranger = await identity();
    const socket = connect("phone", "stranger1");
    await say(socket, { type: "auth", signature: await stranger.sign(nonceOf(socket)) });
    expect(socket.closed?.code).toBe(CLOSE_UNAUTHORIZED);
  });

  it("requires authentication before anything else", async () => {
    const socket = connect("desktop", desktop.id);
    await say(socket, frame);
    expect(socket.closed?.code).toBe(CLOSE_UNAUTHORIZED);
  });

  it("verifies the app's Ed25519 test vector", async () => {
    // From src/crypto/test-vectors.json in the app repo: the signature covers
    // the decoded nonce bytes.
    const publicKey = "TjDlN9oNdK3AZCssMw2ncymUxGBYXjmnCh3VEaYIxHs=";
    const nonce = "BwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSY=";
    const signature = "p8xLfysH/XDe+9UAfP/IW0t+cYeg1CLmZlovDbSluDGFkSeYaXmaki1hZwOhJx4jY5wehdLkX1RcympKrCDdCw==";
    const desktopSocket = await connectDesktop();
    await say(desktopSocket, { type: "authorize_device", device_id: "vectorphone", sign_pk: publicKey, name: "Vector" });
    const socket = connect("phone", "vectorphone");
    socket.serializeAttachment({ ...(socket.deserializeAttachment() as Attachment), nonce });
    await say(socket, { type: "auth", signature });
    expect(socket.types()).toEqual(["challenge", "auth_ok", "desktop_status"]);
  });

  it("replaces an older desktop connection", async () => {
    const first = await connectDesktop();
    const second = await connectDesktop();
    expect(first.closed?.code).toBe(CLOSE_REPLACED);
    expect(second.closed).toBeUndefined();
  });
});

describe("routing", () => {
  it("forwards frames both ways and tags phone frames with the sender", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    const phoneSocket = await connectPhone(phone);
    expect(phoneSocket.types()).toEqual(["challenge", "auth_ok", "desktop_status"]);
    expect(phoneSocket.last()).toEqual({ type: "desktop_status", online: true });
    expect(desktopSocket.last()).toEqual({ type: "presence", device_id: phone.id, online: true });

    await say(phoneSocket, frame);
    expect(desktopSocket.last()).toEqual({ ...frame, device_id: phone.id });

    await say(desktopSocket, { ...frame, device_id: phone.id });
    expect(phoneSocket.last()).toEqual(frame);
  });

  it("strips unknown fields from frames", async () => {
    const desktopSocket = await connectDesktop();
    const phoneSocket = await connectPhone(await pairPhone(desktopSocket));
    await say(phoneSocket, { ...frame, device_id: "spoofed", extra: true });
    expect(desktopSocket.last()).toEqual({ ...frame, device_id: (phoneSocket.deserializeAttachment() as Attachment).id });
  });

  it("tells each side when the other is offline", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    await say(desktopSocket, { ...frame, device_id: phone.id });
    expect(desktopSocket.last()).toEqual({ type: "error", code: "recipient_offline", device_id: phone.id });

    const phoneSocket = await connectPhone(phone);
    desktopSocket.close();
    relay.close(desktopSocket);
    expect(phoneSocket.last()).toEqual({ type: "desktop_status", online: false });
    await say(phoneSocket, frame);
    expect(phoneSocket.last()).toEqual({ type: "error", code: "desktop_offline" });
  });

  it("reports a phone going offline only when its last socket closes", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    const first = await connectPhone(phone);
    await connectPhone(phone);
    first.close();
    relay.close(first);
    expect(desktopSocket.last()).toEqual({ type: "presence", device_id: phone.id, online: true });
  });
});

describe("pairing", () => {
  it("forwards a sealed request only while pairing is open", async () => {
    const desktopSocket = await connectDesktop();
    const closed = connect("pairing", "phone0001");
    await say(closed, { type: "sealed", ciphertext: "YWJj" });
    expect(closed.closed?.code).toBe(CLOSE_PAIRING_DONE);

    await say(desktopSocket, { type: "pairing_open", ttl_seconds: 60 });
    const pairing = connect("pairing", "phone0002");
    expect(pairing.sent).toEqual([]);
    await say(pairing, { type: "sealed", ciphertext: "YWJj" });
    expect(desktopSocket.last()).toEqual({ type: "sealed", device_id: "phone0002", ciphertext: "YWJj" });

    await say(desktopSocket, { ...frame, device_id: "phone0002" });
    expect(pairing.last()).toEqual(frame);
    expect(pairing.closed?.code).toBe(CLOSE_PAIRING_DONE);

    now += 61_000;
    expect(await relay.pairingOpen()).toBe(false);
  });

  it("caps the pairing window", async () => {
    const desktopSocket = await connectDesktop();
    await say(desktopSocket, { type: "pairing_open", ttl_seconds: 86_400 });
    now += 301_000;
    expect(await relay.pairingOpen()).toBe(false);
  });

  it("only lets pairing sockets send sealed requests", async () => {
    await connectDesktop();
    const pairing = connect("pairing", "phone0001");
    await say(pairing, frame);
    expect(pairing.closed?.code).toBe(CLOSE_UNAUTHORIZED);
  });

  it("rejects malformed devices", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await identity();
    await say(desktopSocket, { type: "authorize_device", device_id: "short", sign_pk: phone.publicKey });
    expect(desktopSocket.last()).toEqual({ type: "error", code: "invalid_device" });
    await say(desktopSocket, { type: "authorize_device", device_id: "phone0001", sign_pk: "YWJj" });
    expect(desktopSocket.last()).toEqual({ type: "error", code: "invalid_device" });
  });

  it("limits the number of paired devices", async () => {
    const desktopSocket = await connectDesktop();
    for (let index = 0; index < MAX_DEVICES; index++) await pairPhone(desktopSocket);
    await pairPhone(desktopSocket);
    expect(desktopSocket.last()).toEqual({ type: "error", code: "too_many_devices" });
  });

  it("revoking a device disconnects it and blocks it from reconnecting", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    const phoneSocket = await connectPhone(phone);
    await say(desktopSocket, { type: "revoke_device", device_id: phone.id });
    expect(phoneSocket.closed?.code).toBe(CLOSE_REVOKED);
    const again = await connectPhone(phone);
    expect(again.closed?.code).toBe(CLOSE_UNAUTHORIZED);
  });

  it("lists devices with presence and push state", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket, "iPhone");
    const phoneSocket = await connectPhone(phone);
    await say(phoneSocket, { type: "register_push", expo_push_token: "ExponentPushToken[abc]" });
    await say(desktopSocket, { type: "list_devices" });
    expect(desktopSocket.last()).toEqual({
      type: "devices",
      list: [{ device_id: phone.id, name: "iPhone", online: true, has_push: true }],
    });
  });
});

describe("push", () => {
  async function pushReady() {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    const phoneSocket = await connectPhone(phone);
    await say(phoneSocket, { type: "register_push", expo_push_token: "ExponentPushToken[abc]" });
    disconnect(phoneSocket);
    return { desktopSocket, phone };
  }

  it("sends generic text and routing data only", async () => {
    const { desktopSocket } = await pushReady();
    fetcher.mockResolvedValue(Response.json({ data: [{ status: "ok" }] }));
    await say(desktopSocket, { type: "push", kind: "needs_authorization", thread_id: "t1", body: "rm -rf ~" });
    const [, init] = fetcher.mock.calls[0];
    expect(JSON.parse(init.body)).toEqual([{
      to: "ExponentPushToken[abc]",
      body: "An agent needs your approval",
      sound: "default",
      priority: "high",
      data: { desktop_id: desktop.id, thread_id: "t1", kind: "needs_authorization" },
    }]);
  });

  it("skips devices with an authenticated socket", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    const phoneSocket = await connectPhone(phone);
    await say(phoneSocket, { type: "register_push", expo_push_token: "ExponentPushToken[abc]" });
    fetcher.mockImplementation(async () => Response.json({ data: [{ status: "ok" }] }));
    await say(desktopSocket, { type: "push", kind: "finished", thread_id: "t1" });
    expect(fetcher).not.toHaveBeenCalled();
    disconnect(phoneSocket);
    await say(desktopSocket, { type: "push", kind: "finished", thread_id: "t1" });
    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  it("pushes a terminal waiting for input with its terminal id", async () => {
    const { desktopSocket } = await pushReady();
    fetcher.mockImplementation(async () => Response.json({ data: [{ status: "ok" }] }));
    await say(desktopSocket, { type: "push", kind: "terminal_input", terminal_id: "term1" });
    const [, init] = fetcher.mock.calls.at(-1)!;
    const [sent] = JSON.parse(init.body);
    expect(sent.body).toBe("A terminal is waiting for input");
    expect(sent.data).toEqual({ desktop_id: desktop.id, kind: "terminal_input", terminal_id: "term1" });
    // A thread id doesn't stand in for the terminal.
    await say(desktopSocket, { type: "push", kind: "terminal_input", thread_id: "t1" });
    expect(desktopSocket.last()).toEqual({ type: "error", code: "invalid_push_terminal" });
  });

  it("puts the title Lathe sent above a shorter body", async () => {
    const { desktopSocket } = await pushReady();
    fetcher.mockImplementation(async () => Response.json({ data: [{ status: "ok" }] }));
    await say(desktopSocket, { type: "push", kind: "finished", thread_id: "t1", title: "  Fix the login redirect  " });
    let [sent] = JSON.parse(fetcher.mock.calls.at(-1)![1].body);
    expect(sent.title).toBe("Fix the login redirect");
    expect(sent.body).toBe("Finished");
    expect(sent.data).toEqual({ desktop_id: desktop.id, kind: "finished", thread_id: "t1" });

    await say(desktopSocket, { type: "push", kind: "terminal_input", terminal_id: "term1", title: "x".repeat(300) });
    [sent] = JSON.parse(fetcher.mock.calls.at(-1)![1].body);
    expect(sent.title).toHaveLength(100);
    expect(sent.body).toBe("Waiting for input");

    // No title (or an older Lathe): the generic text, with no title.
    await say(desktopSocket, { type: "push", kind: "finished", thread_id: "t1", title: "   " });
    [sent] = JSON.parse(fetcher.mock.calls.at(-1)![1].body);
    expect(sent.title).toBeUndefined();
    expect(sent.body).toBe("An agent finished");
  });

  it("rejects unknown kinds", async () => {
    const { desktopSocket } = await pushReady();
    await say(desktopSocket, { type: "push", kind: "anything", thread_id: "t1" });
    expect(desktopSocket.last()).toEqual({ type: "error", code: "invalid_push_kind" });
    expect(fetcher).not.toHaveBeenCalled();
  });

  it("only pushes the kinds a phone asked for", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    let phoneSocket = await connectPhone(phone);
    await say(phoneSocket, { type: "register_push", expo_push_token: "ExponentPushToken[abc]", kinds: ["needs_authorization"] });
    disconnect(phoneSocket);
    fetcher.mockImplementation(async () => Response.json({ data: [{ status: "ok" }] }));
    await say(desktopSocket, { type: "push", kind: "finished", thread_id: "t1" });
    expect(fetcher).not.toHaveBeenCalled();
    await say(desktopSocket, { type: "push", kind: "needs_authorization", thread_id: "t1" });
    expect(fetcher).toHaveBeenCalledTimes(1);

    // Registering again without kinds means every kind.
    phoneSocket = await connectPhone(phone);
    await say(phoneSocket, { type: "register_push", expo_push_token: "ExponentPushToken[abc]" });
    disconnect(phoneSocket);
    await say(desktopSocket, { type: "push", kind: "finished", thread_id: "t1" });
    expect(fetcher).toHaveBeenCalledTimes(2);

    phoneSocket = await connectPhone(phone);

    await say(phoneSocket, { type: "register_push", expo_push_token: "ExponentPushToken[abc]", kinds: ["bogus"] });
    expect(phoneSocket.last()).toEqual({ type: "error", code: "invalid_push_kinds" });
  });

  it("drops tokens Expo reports as unregistered", async () => {
    const { desktopSocket, phone } = await pushReady();
    fetcher.mockResolvedValue(Response.json({ data: [{ status: "error", details: { error: "DeviceNotRegistered" } }] }));
    await say(desktopSocket, { type: "push", kind: "finished", thread_id: "t1" });
    expect(state.data.get(`device:${phone.id}`)).not.toHaveProperty("push_token");
  });

  it("rate limits pushes per hour", async () => {
    const { desktopSocket } = await pushReady();
    fetcher.mockImplementation(async () => Response.json({ data: [{ status: "ok" }] }));
    for (let index = 0; index < PUSHES_PER_HOUR; index++) {
      await say(desktopSocket, { type: "push", kind: "finished", thread_id: "t1" });
    }
    await say(desktopSocket, { type: "push", kind: "finished", thread_id: "t1" });
    expect(desktopSocket.last()).toEqual({ type: "error", code: "push_rate_limited" });
    expect(fetcher).toHaveBeenCalledTimes(PUSHES_PER_HOUR);
  });
});

describe("limits", () => {
  it("closes sockets that send oversized or malformed messages", async () => {
    const big = connect("desktop", desktop.id);
    await relay.message(big, "x".repeat(300 * 1024));
    expect(big.closed?.code).toBe(1009);
    const garbled = connect("desktop", desktop.id);
    await relay.message(garbled, "{not json");
    expect(garbled.closed?.code).toBe(1007);
  });

  it("closes sockets that flood messages", async () => {
    const socket = await connectDesktop();
    for (let index = 0; index < 250 && !socket.closed; index++) await say(socket, { type: "list_devices" });
    expect(socket.closed?.code).toBe(1008);
  });
});

describe("subscription gate", () => {
  const HOUR = 60 * 60 * 1000;
  const REVENUECAT = "https://api.revenuecat.com/v1/subscribers/";

  class FakeKv implements Kv {
    data = new Map<string, { value: string; expiresAt?: number }>();
    async get(key: string) {
      const entry = this.data.get(key);
      if (!entry || (entry.expiresAt !== undefined && entry.expiresAt <= now)) return null;
      return JSON.parse(entry.value);
    }
    async put(key: string, value: string, options?: { expirationTtl?: number }) {
      this.data.set(key, { value, expiresAt: options?.expirationTtl ? now + options.expirationTtl * 1000 : undefined });
    }
    async delete(key: string) {
      this.data.delete(key);
    }
  }

  let kv: FakeKv;
  // What RevenueCat says for each device: an expiry, "none", or a failure.
  let subscriptions: Map<string, string | null | "none" | "down" | "error">;
  let revenueCatCalls: string[];

  const env = () => ({ SUBSCRIPTIONS: kv, REVENUECAT_SECRET_KEY: "sk_test", REVENUECAT_WEBHOOK_AUTH: "Bearer hook-secret" });

  beforeEach(() => {
    kv = new FakeKv();
    subscriptions = new Map();
    revenueCatCalls = [];
    fetcher.mockImplementation(async (url: string, init?: RequestInit) => {
      if (!url.startsWith(REVENUECAT)) return Response.json({ data: [{ status: "ok" }] });
      expect(new Headers(init?.headers).get("Authorization")).toBe("Bearer sk_test");
      const deviceId = decodeURIComponent(url.slice(REVENUECAT.length));
      revenueCatCalls.push(deviceId);
      const state = subscriptions.has(deviceId) ? subscriptions.get(deviceId)! : "none";
      if (state === "down") throw new TypeError("network error");
      if (state === "error") return new Response("unavailable", { status: 503 });
      const entitlements = state === "none" ? {} : { lathe_companion_pro: { expires_date: state } };
      return Response.json({ subscriber: { entitlements } });
    });
    relay = new Relay(state, env(), desktop.id, () => now, fetcher as unknown as typeof fetch);
  });

  const inHours = (hours: number) => new Date(now + hours * HOUR).toISOString();

  it("lets an active subscription in and closes an inactive one with 4402", async () => {
    const desktopSocket = await connectDesktop();
    const subscribed = await pairPhone(desktopSocket);
    const unsubscribed = await pairPhone(desktopSocket);
    subscriptions.set(subscribed.id, inHours(24));

    const allowed = await connectPhone(subscribed);
    expect(allowed.types()).toEqual(["challenge", "auth_ok", "desktop_status"]);

    const refused = await connectPhone(unsubscribed);
    expect(refused.closed).toEqual({ code: CLOSE_SUBSCRIPTION_REQUIRED, reason: "subscription required" });
    expect(refused.types()).toEqual(["challenge"]);
    // A refused phone gets nothing forwarded and the desktop doesn't see it.
    await say(refused, frame);
    expect(desktopSocket.sent.filter((message) => message.device_id === unsubscribed.id)).toEqual([]);
  });

  it("checks the signature before the subscription", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    const socket = connect("phone", phone.id);
    await say(socket, { type: "auth", signature: await (await identity()).sign(nonceOf(socket)) });
    expect(socket.closed?.code).toBe(CLOSE_UNAUTHORIZED);
    expect(revenueCatCalls).toEqual([]);
  });

  it("treats a missing expiry as lifetime access", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    subscriptions.set(phone.id, null);
    expect((await connectPhone(phone)).types()).toContain("auth_ok");
  });

  it("doesn't gate pairing", async () => {
    const desktopSocket = await connectDesktop();
    await say(desktopSocket, { type: "pairing_open", ttl_seconds: 60 });
    const pairing = connect("pairing", "newphone1");
    await say(pairing, { type: "sealed", ciphertext: "YWJj" });
    expect(desktopSocket.last()).toEqual({ type: "sealed", device_id: "newphone1", ciphertext: "YWJj" });
    expect(revenueCatCalls).toEqual([]);
  });

  it("caches active results for an hour and never caches inactive ones", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);

    await connectPhone(phone);
    await connectPhone(phone);
    expect(revenueCatCalls).toEqual([phone.id, phone.id]);

    // A purchase right after a 4402 is seen on the next attempt.
    subscriptions.set(phone.id, inHours(24 * 30));
    expect((await connectPhone(phone)).types()).toContain("auth_ok");
    expect((await connectPhone(phone)).types()).toContain("auth_ok");
    expect(revenueCatCalls).toHaveLength(3);

    now += HOUR + 1000;
    await connectPhone(phone);
    expect(revenueCatCalls).toHaveLength(4);
  });

  it("only rewrites the outage record when the expiry moves", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    subscriptions.set(phone.id, inHours(24 * 30));
    const writes = vi.spyOn(kv, "put");
    await connectPhone(phone);
    now += HOUR + 1000;
    await connectPhone(phone);
    expect(writes.mock.calls.map(([key]) => key)).toEqual([`last:${phone.id}`, `active:${phone.id}`, `active:${phone.id}`]);
  });

  it("doesn't cache past the expiry", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    subscriptions.set(phone.id, inHours(0.25));
    await connectPhone(phone);
    now += 0.5 * HOUR;
    subscriptions.set(phone.id, "none");
    expect((await connectPhone(phone)).closed?.code).toBe(CLOSE_SUBSCRIPTION_REQUIRED);
  });

  describe("outages", () => {
    it("lets a device in within 24 hours of its last active result", async () => {
      const desktopSocket = await connectDesktop();
      const phone = await pairPhone(desktopSocket);
      subscriptions.set(phone.id, inHours(2));
      await connectPhone(phone);

      // The subscription lapsed an hour ago as far as the relay last knew,
      // and RevenueCat is down.
      now += 3 * HOUR;
      subscriptions.set(phone.id, "down");
      expect((await connectPhone(phone)).types()).toContain("auth_ok");
      subscriptions.set(phone.id, "error");
      expect((await connectPhone(phone)).types()).toContain("auth_ok");
    });

    it("closes with 4402, never 4401, once the grace is over", async () => {
      const desktopSocket = await connectDesktop();
      const phone = await pairPhone(desktopSocket);
      subscriptions.set(phone.id, inHours(2));
      await connectPhone(phone);

      now += 2 * HOUR + OUTAGE_GRACE_MS + 1000;
      subscriptions.set(phone.id, "down");
      expect((await connectPhone(phone)).closed?.code).toBe(CLOSE_SUBSCRIPTION_REQUIRED);
    });

    it("closes a device RevenueCat never confirmed", async () => {
      const desktopSocket = await connectDesktop();
      const phone = await pairPhone(desktopSocket);
      subscriptions.set(phone.id, "down");
      expect((await connectPhone(phone)).closed?.code).toBe(CLOSE_SUBSCRIPTION_REQUIRED);
    });

    it("forgets the grace once RevenueCat says the subscription ended", async () => {
      const desktopSocket = await connectDesktop();
      const phone = await pairPhone(desktopSocket);
      subscriptions.set(phone.id, inHours(2));
      await connectPhone(phone);
      now += 3 * HOUR;
      subscriptions.set(phone.id, "none");
      await connectPhone(phone);
      subscriptions.set(phone.id, "down");
      expect((await connectPhone(phone)).closed?.code).toBe(CLOSE_SUBSCRIPTION_REQUIRED);
    });
  });

  describe("webhook", () => {
    let pending: Promise<unknown>[];
    let expired: [string, string][];

    function deliver(body: unknown, authorization = "Bearer hook-secret") {
      pending = [];
      return handleWebhook(
        new Request("https://relay/v1/revenuecat", {
          method: "POST",
          headers: { Authorization: authorization, "Content-Type": "application/json" },
          body: JSON.stringify(body),
        }),
        {
          env: env(),
          waitUntil: (promise) => pending.push(promise),
          async expireOnDesktop(desktopId, deviceId) {
            expired.push([desktopId, deviceId]);
            if (desktopId === desktop.id) relay.expireDevice(deviceId);
          },
        },
      );
    }

    beforeEach(() => {
      expired = [];
    });

    it("rejects a bad Authorization header", async () => {
      expect((await deliver({ event: { type: "EXPIRATION", app_user_id: "phone0001" } }, "Bearer wrong")).status).toBe(401);
      expect((await deliver({ event: {} }, "")).status).toBe(401);
    });

    it("clears the cache on any event, so the next connect asks RevenueCat", async () => {
      const desktopSocket = await connectDesktop();
      const phone = await pairPhone(desktopSocket);
      subscriptions.set(phone.id, inHours(24 * 30));
      await connectPhone(phone);
      expect(revenueCatCalls).toHaveLength(1);

      const response = await deliver({ event: { type: "CANCELLATION", app_user_id: phone.id } });
      expect(response.status).toBe(200);
      await Promise.all(pending);
      await connectPhone(phone);
      expect(revenueCatCalls).toHaveLength(2);
    });

    it("closes the device's open sockets with 4402 on EXPIRATION", async () => {
      const desktopSocket = await connectDesktop();
      const phone = await pairPhone(desktopSocket);
      const other = await pairPhone(desktopSocket);
      subscriptions.set(phone.id, inHours(1));
      subscriptions.set(other.id, inHours(1));
      const phoneSocket = await connectPhone(phone);
      const otherSocket = await connectPhone(other);

      subscriptions.set(phone.id, "none");
      expect((await deliver({ event: { type: "EXPIRATION", app_user_id: phone.id } })).status).toBe(200);
      await Promise.all(pending);
      expect(expired).toEqual([[desktop.id, phone.id]]);
      expect(phoneSocket.closed).toEqual({ code: CLOSE_SUBSCRIPTION_REQUIRED, reason: "subscription required" });
      expect(otherSocket.closed).toBeUndefined();
      // The outage grace doesn't bring an expired device back.
      subscriptions.set(phone.id, "down");
      expect((await connectPhone(phone)).closed?.code).toBe(CLOSE_SUBSCRIPTION_REQUIRED);
    });

    it("stops reaching a desktop once the device is revoked there", async () => {
      const desktopSocket = await connectDesktop();
      const phone = await pairPhone(desktopSocket);
      await say(desktopSocket, { type: "revoke_device", device_id: phone.id });
      await deliver({ event: { type: "EXPIRATION", app_user_id: phone.id } });
      await Promise.all(pending);
      expect(expired).toEqual([]);
    });

    it("collects every id a TRANSFER touches", () => {
      expect(
        affectedDeviceIds({
          type: "TRANSFER",
          app_user_id: "phone0001",
          original_app_user_id: "phone0002",
          aliases: ["phone0001", "phone0003", "$RCAnonymousID:abc"],
          transferred_from: ["phone0004"],
          transferred_to: ["phone0005"],
        }).sort(),
      ).toEqual(["phone0001", "phone0002", "phone0003", "phone0004", "phone0005"]);
    });
  });

  describe("push", () => {
    it("skips devices without an active subscription", async () => {
      const desktopSocket = await connectDesktop();
      const subscribed = await pairPhone(desktopSocket);
      const lapsed = await pairPhone(desktopSocket);
      subscriptions.set(subscribed.id, inHours(24));
      subscriptions.set(lapsed.id, inHours(24));
      for (const phone of [subscribed, lapsed]) {
        const socket = await connectPhone(phone);
        await say(socket, { type: "register_push", expo_push_token: `ExponentPushToken[${phone.id}]` });
        disconnect(socket);
      }
      subscriptions.set(lapsed.id, "none");
      await relay["entitlements"].forget([lapsed.id], true);
      revenueCatCalls = [];

      await say(desktopSocket, { type: "push", kind: "finished", thread_id: "t1" });
      const expo = fetcher.mock.calls.filter(([url]) => !String(url).startsWith(REVENUECAT));
      expect(JSON.parse(expo[0][1].body).map((message: { to: string }) => message.to)).toEqual([`ExponentPushToken[${subscribed.id}]`]);
      // The subscribed device came from the cache; only the lapsed one was looked up.
      expect(revenueCatCalls).toEqual([lapsed.id]);
    });
  });
});

describe("stale sockets", () => {
  // The runtime answers a ping without waking the relay; it only records when.
  const ping = (socket: FakeSocket) => { socket.lastPing = now; };

  it("reports a phone offline when its live socket closes and only a dead one remains", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    // iOS suspended the app before this socket's close frame went out.
    await connectPhone(phone);
    now += STALE_SOCKET_MS + 1000;
    const current = await connectPhone(phone);
    expect(desktopSocket.last()).toEqual({ type: "presence", device_id: phone.id, online: true });

    disconnect(current);
    expect(desktopSocket.last()).toEqual({ type: "presence", device_id: phone.id, online: false });
  });

  it("keeps a phone online while its sockets keep pinging", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    const first = await connectPhone(phone);
    const second = await connectPhone(phone);
    now += STALE_SOCKET_MS - 1000;
    ping(first);
    now += 2000;
    disconnect(second);
    expect(desktopSocket.last()).toEqual({ type: "presence", device_id: phone.id, online: true });
  });

  it("sweeps a dead socket and tells the desktop the phone left", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    const phoneSocket = await connectPhone(phone);
    expect(state.alarm).toBe(now + SWEEP_INTERVAL_MS);

    // The desktop keeps pinging; the phone went silent without closing.
    now += STALE_SOCKET_MS + 1000;
    ping(desktopSocket);
    state.alarm = null;
    await relay.sweep();
    expect(phoneSocket.closed).toEqual({ code: 1001, reason: "connection timed out" });
    expect(desktopSocket.last()).toEqual({ type: "presence", device_id: phone.id, online: false });
    expect(desktopSocket.closed).toBeUndefined();
    // The desktop is still connected, so the sweep keeps running.
    expect(state.alarm).toBe(now + SWEEP_INTERVAL_MS);
  });

  it("tells phones a silent desktop is gone", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    const phoneSocket = await connectPhone(phone);
    now += STALE_SOCKET_MS + 1000;
    ping(phoneSocket);
    await relay.sweep();
    expect(desktopSocket.closed?.code).toBe(1001);
    expect(phoneSocket.last()).toEqual({ type: "desktop_status", online: false });
  });

  it("handles a dead socket once, even while the runtime still lists it", async () => {
    const desktopSocket = await connectDesktop();
    const phone = await pairPhone(desktopSocket);
    const phoneSocket = await connectPhone(phone);
    now += STALE_SOCKET_MS + 1000;
    ping(desktopSocket);
    await relay.sweep();
    // The peer never answered, so the runtime keeps listing it.
    phoneSocket.closed = undefined;
    desktopSocket.sent = [];
    now += SWEEP_INTERVAL_MS;
    ping(desktopSocket);
    await relay.sweep();
    expect(desktopSocket.sent).toEqual([]);
  });

  it("stops sweeping once nothing is connected", async () => {
    const desktopSocket = await connectDesktop();
    now += STALE_SOCKET_MS + 1000;
    state.alarm = null;
    await relay.sweep();
    expect(desktopSocket.closed?.code).toBe(1001);
    expect(state.alarm).toBeNull();
  });

  it("only lists live phones to a desktop that reconnects", async () => {
    const desktopSocket = await connectDesktop();
    const stale = await pairPhone(desktopSocket);
    const live = await pairPhone(desktopSocket);
    await connectPhone(stale);
    now += STALE_SOCKET_MS + 1000;
    await connectPhone(live);
    const reconnected = await connectDesktop();
    const presence = reconnected.sent.filter((message) => message.type === "presence");
    expect(presence).toEqual([{ type: "presence", device_id: live.id, online: true }]);
  });
});

describe("desktop routing", () => {
  it("delivers to the newest open desktop socket, not one left over", async () => {
    // A replaced or dropped connection the runtime still lists as closing.
    const old = await connectDesktop();
    (old as unknown as { readyState: number }).readyState = 2;
    old.closed = undefined;
    now += 1000;
    const current = connect("desktop", desktop.id);
    (current as unknown as { readyState: number }).readyState = 1;
    await say(current, { type: "auth", public_key: desktop.publicKey, signature: await desktop.sign(nonceOf(current)) });
    old.closed = undefined;
    old.sent = [];

    await say(current, { type: "pairing_open", ttl_seconds: 60 });
    const pairing = connect("pairing", "newphone1");
    await say(pairing, { type: "sealed", ciphertext: "YWJj" });
    expect(current.last()).toEqual({ type: "sealed", device_id: "newphone1", ciphertext: "YWJj" });
    expect(old.sent).toEqual([]);
  });

  it("prefers the most recently connected of several open sockets", async () => {
    const first = await connectDesktop();
    now += 1000;
    const second = connect("desktop", desktop.id);
    await say(second, { type: "auth", public_key: desktop.publicKey, signature: await desktop.sign(nonceOf(second)) });
    // Undo the replacement, as if the old one hadn't been closed yet.
    first.closed = undefined;
    first.sent = [];
    second.sent = [];
    await say(second, { type: "pairing_open", ttl_seconds: 60 });
    const pairing = connect("pairing", "newphone2");
    await say(pairing, { type: "sealed", ciphertext: "YWJj" });
    expect(second.last()).toEqual({ type: "sealed", device_id: "newphone2", ciphertext: "YWJj" });
    expect(first.sent).toEqual([]);
  });
});

describe("pairing link page", () => {
  it("is served without caching, referrers or outside resources", async () => {
    const response = pairPage();
    expect(response.headers.get("Cache-Control")).toBe("no-store");
    expect(response.headers.get("Referrer-Policy")).toBe("no-referrer");
    expect(response.headers.get("Content-Security-Policy")).toContain("default-src 'none'");
    const html = await response.text();
    expect(html).toContain("intent://pair?");
    expect(html).toContain("scheme=lathe-companion;package=com.lathe.companion;end");
    expect(html).toContain("lathe-companion://pair?");
    // The pairing details come from the fragment, which never reaches the server.
    expect(html).toContain("location.hash");
    expect(html).not.toMatch(/https?:\/\//);
  });
});
