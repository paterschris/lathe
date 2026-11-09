// Routing core of the companion relay, kept free of Cloudflare runtime imports
// so it runs under plain vitest. `index.ts` wires it into a Durable Object.
//
// The relay never sees plaintext: frames are end-to-end encrypted between the
// desktop and each phone. It only authenticates sockets, forwards frames, and
// sends push notifications that carry a thread or terminal title at most,
// never message content.

import { DEVICE_ID_PATTERN, DesktopIndex, Entitlements, type EntitlementEnv } from "./entitlements";

export { DEVICE_ID_PATTERN };

export const MAX_MESSAGE_BYTES = 256 * 1024;
export const MAX_DEVICES = 10;
export const MAX_PAIRING_SECONDS = 300;
// Clients ping every 30 seconds. Two missed pings plus slack means the socket
// is dead even if no close frame ever arrived, which is how a suspended or
// killed phone app usually leaves.
export const STALE_SOCKET_MS = 75_000;
export const SWEEP_INTERVAL_MS = 60_000;
export const PUSHES_PER_HOUR = 60;
const MESSAGES_PER_SECOND = 50;
const MESSAGE_BURST = 200;
const MAX_PAIRING_MESSAGES = 3;
const EXPO_PUSH_URL = "https://exp.host/--/api/v2/push/send";

// Close codes, as the app's protocol doc defines them.
export const CLOSE_PAIRING_DONE = 4000;
export const CLOSE_UNAUTHORIZED = 4401;
export const CLOSE_SUBSCRIPTION_REQUIRED = 4402;
export const CLOSE_REVOKED = 4403;
// Desktop only: a newer connection with the same identity took over.
export const CLOSE_REPLACED = 4409;

export const PUSH_TEXT: Record<string, string> = {
  needs_authorization: "An agent needs your approval",
  needs_input: "An agent is waiting for your answer",
  finished: "An agent finished",
  error: "An agent stopped with an error",
  terminal_input: "A terminal is waiting for input",
};

// The body under a thread or terminal title, which already says which one.
export const TITLED_PUSH_TEXT: Record<string, string> = {
  needs_authorization: "Needs your approval",
  needs_input: "Waiting for your answer",
  finished: "Finished",
  error: "Stopped with an error",
  terminal_input: "Waiting for input",
};

// Lathe caps titles at 100 characters; anything longer is cut, not rejected.
const MAX_PUSH_TITLE = 100;

// A terminal push names a terminal; every other kind names a thread.
const TERMINAL_KINDS = new Set(["terminal_input"]);

export type Role = "desktop" | "phone" | "pairing";

export interface Attachment {
  role: Role;
  // Device id for phones and pairing sockets, the desktop id for the desktop.
  id: string;
  nonce: string;
  authed: boolean;
  connectedAt: number;
  pairingMessages?: number;
}

export interface Socket {
  // WebSocket.READY_STATE_OPEN (1) once open; a socket the relay closed stays
  // listed while it waits for the peer's close frame.
  readyState?: number;
  send(message: string): void;
  close(code?: number, reason?: string): void;
  serializeAttachment(value: unknown): void;
  deserializeAttachment(): unknown;
}

export interface Storage {
  get<T>(key: string): Promise<T | undefined>;
  put(key: string, value: unknown): Promise<void>;
  delete(key: string): Promise<boolean>;
  list<T>(options: { prefix: string }): Promise<Map<string, T>>;
  getAlarm(): Promise<number | null>;
  setAlarm(scheduledTime: number): Promise<void>;
}

export interface RelayState {
  getWebSockets(tag?: string): Socket[];
  // When the runtime last answered this socket's ping on the relay's behalf.
  getWebSocketAutoResponseTimestamp(socket: Socket): Date | null;
  storage: Storage;
}

export interface RelayEnv extends EntitlementEnv {
  EXPO_ACCESS_TOKEN?: string;
}

interface DeviceRecord {
  device_id: string;
  sign_pk: string;
  name: string;
  push_token?: string;
  // Omitted means every kind.
  push_kinds?: string[];
}

type Message = Record<string, unknown> & { type?: unknown };

export function base64ToBytes(value: string): Uint8Array {
  const normalized = value.replace(/-/g, "+").replace(/_/g, "/");
  const padded = normalized + "=".repeat((4 - (normalized.length % 4)) % 4);
  return Uint8Array.from(atob(padded), (char) => char.charCodeAt(0));
}

export function bytesToBase64(bytes: Uint8Array, url = false): string {
  const encoded = btoa(String.fromCharCode(...bytes));
  return url ? encoded.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "") : encoded;
}

// Ids are derived from the signing key, so a desktop needs no registration:
// whoever proves possession of the key behind an id owns it.
export async function idForPublicKey(publicKey: Uint8Array): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", publicKey));
  return bytesToBase64(digest.slice(0, 16), true);
}

export const ID_PATTERN = /^[A-Za-z0-9_-]{22}$/;

// The signature covers the decoded nonce bytes.
async function verifySignature(publicKey: string, nonce: string, signature: string): Promise<boolean> {
  try {
    const key = await crypto.subtle.importKey("raw", base64ToBytes(publicKey), { name: "Ed25519" }, false, ["verify"]);
    return await crypto.subtle.verify({ name: "Ed25519" }, key, base64ToBytes(signature), base64ToBytes(nonce));
  } catch {
    return false;
  }
}

function text(message: Message, key: string, limit = 4096): string | undefined {
  const value = message[key];
  return typeof value === "string" && value.length <= limit ? value : undefined;
}

// Sockets that don't report a state (the tests' fakes) count as open.
function isOpen(socket: Socket): boolean {
  return socket.readyState === undefined || socket.readyState === 1;
}

function send(socket: Socket, message: Record<string, unknown>): void {
  try {
    socket.send(JSON.stringify(message));
  } catch {
    // The peer went away between lookup and send; its close handler cleans up.
  }
}

export class Relay {
  private buckets = new WeakMap<Socket, { tokens: number; at: number }>();
  private entitlements: Entitlements;
  private index: DesktopIndex;

  constructor(
    private state: RelayState,
    private env: RelayEnv,
    private desktopId: string,
    private now: () => number = Date.now,
    private fetcher: typeof fetch = (...args) => fetch(...args),
  ) {
    this.entitlements = new Entitlements(env, now, fetcher);
    this.index = new DesktopIndex(env.SUBSCRIPTIONS);
  }

  // The subscription ended: the device's sockets close the same way a
  // connect without a subscription does.
  expireDevice(deviceId: string): void {
    for (const phone of this.state.getWebSockets("phone")) {
      if (this.attachment(phone)?.id === deviceId) phone.close(CLOSE_SUBSCRIPTION_REQUIRED, "subscription required");
    }
  }

  // Called once the Durable Object has accepted the socket.
  open(socket: Socket, role: Role, id: string): void {
    const nonce = bytesToBase64(crypto.getRandomValues(new Uint8Array(32)));
    const attachment: Attachment = { role, id, nonce, authed: false, connectedAt: this.now() };
    if (role === "pairing") attachment.pairingMessages = 0;
    socket.serializeAttachment(attachment);
    if (role === "pairing") return;
    send(socket, { type: "challenge", nonce: attachment.nonce });
  }

  async pairingOpen(): Promise<boolean> {
    const until = await this.state.storage.get<number>("pairing_until");
    return until !== undefined && until > this.now();
  }

  async message(socket: Socket, raw: string | ArrayBuffer): Promise<void> {
    if (typeof raw !== "string" || raw.length > MAX_MESSAGE_BYTES) {
      socket.close(1009, "message too large");
      return;
    }
    if (!this.takeToken(socket)) {
      socket.close(1008, "rate limited");
      return;
    }
    let message: Message;
    try {
      message = JSON.parse(raw);
    } catch {
      socket.close(1007, "invalid json");
      return;
    }
    if (typeof message !== "object" || message === null) {
      socket.close(1007, "invalid message");
      return;
    }
    const attachment = socket.deserializeAttachment() as Attachment;
    if (attachment.role === "pairing") return this.pairingMessage(socket, attachment, message);
    if (!attachment.authed) return this.authenticate(socket, attachment, message);
    if (attachment.role === "desktop") return this.desktopMessage(socket, message);
    return this.phoneMessage(socket, attachment, message);
  }

  close(socket: Socket): void {
    const attachment = socket.deserializeAttachment() as Attachment | null;
    if (!attachment?.authed) return;
    if (attachment.role === "phone") {
      this.toDesktop({ type: "presence", device_id: attachment.id, online: this.phoneOnline(attachment.id, socket) });
    } else if (attachment.role === "desktop" && !this.desktopOnline(socket)) {
      for (const phone of this.authedSockets("phone")) send(phone, { type: "desktop_status", online: false });
    }
  }

  // Runs from the Durable Object alarm. A dead socket never sends a close
  // frame, so without this its device would stay online forever.
  async sweep(): Promise<void> {
    for (const role of ["phone", "desktop"] as const) {
      for (const socket of this.authedSockets(role)) {
        if (this.isLive(socket)) continue;
        const pinged = this.state.getWebSocketAutoResponseTimestamp(socket)?.getTime() ?? null;
        console.log(
          `companion relay: closing a silent ${role} socket (${this.attachment(socket)?.id}), connected at ${this.attachment(socket)?.connectedAt}, last ping ${pinged}, now ${this.now()}`,
        );
        try {
          socket.close(1001, "connection timed out");
        } catch {
          // Already gone.
        }
        // The runtime doesn't report a close the relay started itself.
        this.close(socket);
        // A peer that vanished never answers the close, so the runtime keeps
        // listing the socket; marking it unauthenticated keeps later sweeps
        // and routing from treating it as a connection again.
        try {
          socket.serializeAttachment({ ...this.attachment(socket), authed: false });
        } catch {
          // Already gone.
        }
      }
    }
    await this.scheduleSweep();
  }

  private async scheduleSweep(): Promise<void> {
    const hasSockets = this.authedSockets("phone").length > 0 || this.authedSockets("desktop").length > 0;
    if (!hasSockets) return;
    if ((await this.state.storage.getAlarm()) !== null) return;
    await this.state.storage.setAlarm(this.now() + SWEEP_INTERVAL_MS);
  }

  private isLive(socket: Socket): boolean {
    const pinged = this.state.getWebSocketAutoResponseTimestamp(socket)?.getTime() ?? 0;
    const lastHeard = Math.max(pinged, this.attachment(socket)?.connectedAt ?? 0);
    return this.now() - lastHeard < STALE_SOCKET_MS;
  }

  private async authenticate(socket: Socket, attachment: Attachment, message: Message): Promise<void> {
    const signature = text(message, "signature", 256);
    if (message.type !== "auth" || !signature) {
      socket.close(CLOSE_UNAUTHORIZED, "authentication required");
      return;
    }
    let publicKey: string | undefined;
    if (attachment.role === "desktop") {
      publicKey = text(message, "public_key", 128);
      if (!publicKey || (await idForPublicKey(base64ToBytes(publicKey))) !== this.desktopId) {
        socket.close(CLOSE_UNAUTHORIZED, "key does not match desktop id");
        return;
      }
    } else {
      publicKey = (await this.state.storage.get<DeviceRecord>(`device:${attachment.id}`))?.sign_pk;
      if (!publicKey) {
        socket.close(CLOSE_UNAUTHORIZED, "device not paired");
        return;
      }
    }
    if (!(await verifySignature(publicKey, attachment.nonce, signature))) {
      socket.close(CLOSE_UNAUTHORIZED, "bad signature");
      return;
    }
    // Pairing is free; everything after it needs a subscription.
    if (attachment.role === "phone" && !(await this.entitlements.allows(attachment.id))) {
      socket.close(CLOSE_SUBSCRIPTION_REQUIRED, "subscription required");
      return;
    }

    attachment.authed = true;
    socket.serializeAttachment(attachment);
    await this.scheduleSweep();
    if (attachment.role === "desktop") {
      for (const other of this.authedSockets("desktop")) {
        if (other !== socket) other.close(CLOSE_REPLACED, "replaced by a newer connection");
      }
      send(socket, { type: "auth_ok" });
      for (const phone of this.authedSockets("phone")) send(phone, { type: "desktop_status", online: true });
      const livePhones = this.authedSockets("phone").filter((phone) => this.isLive(phone));
      for (const device_id of new Set(livePhones.map((phone) => this.attachment(phone).id))) {
        send(socket, { type: "presence", device_id, online: true });
      }
    } else {
      send(socket, { type: "auth_ok" });
      send(socket, { type: "desktop_status", online: this.desktopOnline() });
      this.toDesktop({ type: "presence", device_id: attachment.id, online: true });
    }
  }

  private async desktopMessage(socket: Socket, message: Message): Promise<void> {
    const storage = this.state.storage;
    switch (message.type) {
      case "frame": {
        const frame = this.frame(message);
        if (!frame) return this.reject(socket, "invalid_frame");
        const deviceId = text(message, "device_id", 64);
        if (!deviceId) return this.reject(socket, "invalid_frame");
        const phones = this.authedSockets("phone").filter((phone) => this.attachment(phone).id === deviceId);
        const pairings = this.state.getWebSockets("pairing").filter((pairing) => this.attachment(pairing).id === deviceId);
        if (phones.length === 0 && pairings.length === 0) {
          return this.reject(socket, "recipient_offline", { device_id: deviceId });
        }
        for (const target of [...phones, ...pairings]) send(target, frame);
        // A frame to a pairing socket is the desktop's answer, which ends it.
        for (const pairing of pairings) pairing.close(CLOSE_PAIRING_DONE, "pairing finished");
        return;
      }
      case "pairing_open": {
        const seconds = Math.min(Math.max(Number(message.ttl_seconds) || 0, 0), MAX_PAIRING_SECONDS);
        await storage.put("pairing_until", this.now() + seconds * 1000);
        console.log(`companion relay: pairing opened for ${seconds}s`);
        return;
      }
      case "pairing_close": {
        await storage.delete("pairing_until");
        console.log("companion relay: pairing closed by the desktop");
        for (const pairing of this.state.getWebSockets("pairing")) pairing.close(CLOSE_PAIRING_DONE, "pairing closed");
        return;
      }
      case "authorize_device": {
        const deviceId = text(message, "device_id", 64);
        const signPk = text(message, "sign_pk", 128);
        const name = text(message, "name", 100) ?? "Phone";
        if (!deviceId || !DEVICE_ID_PATTERN.test(deviceId) || !signPk || base64ToBytes(signPk).length !== 32) {
          return this.reject(socket, "invalid_device");
        }
        const devices = await storage.list<DeviceRecord>({ prefix: "device:" });
        if (!devices.has(`device:${deviceId}`) && devices.size >= MAX_DEVICES) {
          return this.reject(socket, "too_many_devices");
        }
        await storage.put(`device:${deviceId}`, { device_id: deviceId, sign_pk: signPk, name });
        await this.index.add(deviceId, this.desktopId);
        return;
      }
      case "revoke_device": {
        const deviceId = text(message, "device_id", 64);
        if (!deviceId) return this.reject(socket, "invalid_device");
        await storage.delete(`device:${deviceId}`);
        await this.index.remove(deviceId, this.desktopId);
        for (const phone of this.state.getWebSockets("phone")) {
          if (this.attachment(phone).id === deviceId) phone.close(CLOSE_REVOKED, "device revoked");
        }
        return;
      }
      case "list_devices": {
        const devices = await storage.list<DeviceRecord>({ prefix: "device:" });
        send(socket, {
          type: "devices",
          list: [...devices.values()].map((device) => ({
            device_id: device.device_id,
            name: device.name,
            online: this.phoneOnline(device.device_id),
            has_push: device.push_token !== undefined,
          })),
        });
        return;
      }
      case "push":
        return this.push(socket, message);
      default:
        return this.reject(socket, "unknown_type");
    }
  }

  private async phoneMessage(socket: Socket, attachment: Attachment, message: Message): Promise<void> {
    switch (message.type) {
      case "frame": {
        const frame = this.frame(message);
        if (!frame) return this.reject(socket, "invalid_frame");
        if (!this.toDesktop({ ...frame, device_id: attachment.id })) this.reject(socket, "desktop_offline");
        return;
      }
      case "register_push": {
        const token = text(message, "expo_push_token", 200);
        if (!token || !/^Expo(nent)?PushToken\[[^\]]+\]$/.test(token)) return this.reject(socket, "invalid_push_token");
        const kinds = message.kinds;
        if (kinds !== undefined && (!Array.isArray(kinds) || kinds.some((kind) => typeof kind !== "string" || !(kind in PUSH_TEXT)))) {
          return this.reject(socket, "invalid_push_kinds");
        }
        const key = `device:${attachment.id}`;
        const device = await this.state.storage.get<DeviceRecord>(key);
        if (device) {
          const { push_kinds: _, ...rest } = device;
          await this.state.storage.put(key, { ...rest, push_token: token, ...(kinds ? { push_kinds: kinds as string[] } : {}) });
        }
        return;
      }
      default:
        return this.reject(socket, "unknown_type");
    }
  }

  private pairingMessage(socket: Socket, attachment: Attachment, message: Message): Promise<void> | void {
    attachment.pairingMessages = (attachment.pairingMessages ?? 0) + 1;
    socket.serializeAttachment(attachment);
    const ciphertext = text(message, "ciphertext", 8192);
    if (message.type !== "sealed" || !ciphertext || attachment.pairingMessages > MAX_PAIRING_MESSAGES) {
      socket.close(CLOSE_UNAUTHORIZED, "pairing sockets may only send a sealed request");
      return;
    }
    return this.pairingOpen().then((open) => {
      if (!open) {
        console.log(`companion relay: pairing request from ${attachment.id} arrived after pairing closed`);
        socket.close(CLOSE_PAIRING_DONE, "pairing is not open");
        return;
      }
      const desktops = this.authedSockets("desktop");
      console.log(
        `companion relay: pairing request from ${attachment.id}, ${desktops.length} desktop socket(s), open: ${desktops.filter(isOpen).length}`,
      );
      if (!this.toDesktop({ type: "sealed", device_id: attachment.id, ciphertext })) {
        this.reject(socket, "desktop_offline");
      }
    });
  }

  private async push(socket: Socket, message: Message): Promise<void> {
    const kind = text(message, "kind", 64);
    if (!kind || !(kind in PUSH_TEXT)) return this.reject(socket, "invalid_push_kind");
    const hour = Math.floor(this.now() / 3_600_000);
    const window = (await this.state.storage.get<{ hour: number; count: number }>("push_window")) ?? { hour, count: 0 };
    const count = window.hour === hour ? window.count : 0;
    if (count >= PUSHES_PER_HOUR) return this.reject(socket, "push_rate_limited");
    await this.state.storage.put("push_window", { hour, count: count + 1 });

    const isTerminal = TERMINAL_KINDS.has(kind);
    const target = isTerminal ? text(message, "terminal_id", 128) : text(message, "thread_id", 128);
    if (!target) return this.reject(socket, isTerminal ? "invalid_push_terminal" : "invalid_push_thread");
    const to = text(message, "device_id", 64);
    // The thread's or terminal's title, which Lathe chose to show on the lock screen.
    const title = typeof message.title === "string" ? message.title.trim().slice(0, MAX_PUSH_TITLE) : "";
    const online = new Set(this.authedSockets("phone").map((phone) => this.attachment(phone).id));
    const candidates = [...(await this.state.storage.list<DeviceRecord>({ prefix: "device:" })).values()]
      .filter((device) => device.push_token && (!to || device.device_id === to))
      .filter((device) => !device.push_kinds || device.push_kinds.includes(kind))
      // A connected app already shows the update.
      .filter((device) => !online.has(device.device_id));
    const allowed = await Promise.all(candidates.map((device) => this.entitlements.allows(device.device_id)));
    const devices = candidates.filter((_, index) => allowed[index]);
    if (devices.length === 0) return;

    const headers: Record<string, string> = { "Content-Type": "application/json", Accept: "application/json" };
    if (this.env.EXPO_ACCESS_TOKEN) headers.Authorization = `Bearer ${this.env.EXPO_ACCESS_TOKEN}`;
    const response = await this.fetcher(EXPO_PUSH_URL, {
      method: "POST",
      headers,
      body: JSON.stringify(devices.map((device) => ({
        to: device.push_token,
        ...(title ? { title, body: TITLED_PUSH_TEXT[kind] } : { body: PUSH_TEXT[kind] }),
        sound: "default",
        priority: "high",
        data: { desktop_id: this.desktopId, kind, ...(isTerminal ? { terminal_id: target } : { thread_id: target }) },
      }))),
    });
    if (!response.ok) return this.reject(socket, "push_failed", { status: response.status });

    // Tickets come back in request order. A token Expo no longer recognizes
    // belongs to an uninstalled app, so it is dropped rather than retried.
    const tickets = ((await response.json()) as { data?: { status?: string; details?: { error?: string } }[] }).data ?? [];
    for (const [index, ticket] of tickets.entries()) {
      const device = devices[index];
      if (device && ticket.status === "error" && ticket.details?.error === "DeviceNotRegistered") {
        const { push_token: _, ...rest } = device;
        await this.state.storage.put(`device:${device.device_id}`, rest);
      }
    }
  }

  private frame(message: Message): Record<string, unknown> | undefined {
    const nonce = text(message, "nonce", 64);
    const ciphertext = text(message, "ciphertext", MAX_MESSAGE_BYTES);
    const seq = message.seq;
    if (!nonce || !ciphertext || typeof seq !== "number" || !Number.isSafeInteger(seq) || seq < 1) return undefined;
    return { type: "frame", seq, nonce, ciphertext };
  }

  private takeToken(socket: Socket): boolean {
    const now = this.now();
    const bucket = this.buckets.get(socket) ?? { tokens: MESSAGE_BURST, at: now };
    bucket.tokens = Math.min(MESSAGE_BURST, bucket.tokens + ((now - bucket.at) / 1000) * MESSAGES_PER_SECOND);
    bucket.at = now;
    this.buckets.set(socket, bucket);
    if (bucket.tokens < 1) return false;
    bucket.tokens -= 1;
    return true;
  }

  private attachment(socket: Socket): Attachment {
    return socket.deserializeAttachment() as Attachment;
  }

  private authedSockets(role: Role): Socket[] {
    return this.state.getWebSockets(role).filter((socket) => this.attachment(socket)?.authed);
  }

  // The newest open desktop socket. Older ones linger in the runtime's list
  // after being replaced or dropping without a close frame, and anything sent
  // to them is lost, which is how a phone's pairing request went missing.
  private desktopSocket(excluding?: Socket): Socket | undefined {
    return this.authedSockets("desktop")
      .filter((socket) => socket !== excluding && isOpen(socket))
      .sort((left, right) => (this.attachment(right).connectedAt ?? 0) - (this.attachment(left).connectedAt ?? 0))[0];
  }

  private desktopOnline(excluding?: Socket): boolean {
    return this.authedSockets("desktop").some((socket) => socket !== excluding && isOpen(socket) && this.isLive(socket));
  }

  // Only live sockets count, so a stale socket left behind by an earlier
  // connection can't keep a phone that just left looking online.
  private phoneOnline(deviceId: string, excluding?: Socket): boolean {
    return this.authedSockets("phone").some(
      (socket) => socket !== excluding && this.attachment(socket).id === deviceId && isOpen(socket) && this.isLive(socket),
    );
  }

  private toDesktop(message: Record<string, unknown>): boolean {
    const desktop = this.desktopSocket();
    if (desktop) send(desktop, message);
    return desktop !== undefined;
  }

  private reject(socket: Socket, code: string, extra: Record<string, unknown> = {}): void {
    send(socket, { type: "error", code, ...extra });
  }
}
