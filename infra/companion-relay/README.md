# Lathe Companion relay

Cloudflare Worker that connects desktop Lathe to paired phones running the Lathe Companion app. Each desktop gets one Durable Object, which holds the desktop's socket and its phones' sockets and forwards frames between them.

The relay never sees message content. Frames are end-to-end encrypted between the desktop and each phone, and push notifications carry fixed text plus routing data (`desktop_id`, `thread_id` or `terminal_id`, `kind`). Lathe adds the thread's or terminal's title, which the notification shows as its title; the relay passes it to Expo and never stores it.

## Cost

Built to fit the Workers Free plan for personal use:

- The Durable Object class is SQLite-backed (`new_sqlite_classes`), the only kind Free allows.
- Sockets use the WebSocket Hibernation API, and `{"type":"ping"}` is answered by `setWebSocketAutoResponse` without waking the object, so idle connections cost no Durable Object duration.
- WebSocket messages are billed at 20:1 against Durable Object requests.
- The subscription gate uses Workers KV, whose Free limit that matters is about 1,000 writes a day. A subscribed device costs roughly one write an hour while it is in use (the 1-hour cache), plus one when its subscription renews. That fits a few dozen active devices; past that, Workers Paid lifts the limit.

## Subscription gate

Pairing is free. Every other phone connection needs the `pro` entitlement in RevenueCat, checked for the phone's `device_id` (the app's RevenueCat app user id) after its signature verifies and before `auth_ok`. A device without it is closed with `4402`. The full rules are in the Subscription section of `src/protocol/README.md` in the `lathe-companion-app` repo.

- **Cache:** active results are kept in KV until `min(expires_date, now + 1h)`. Inactive results are never cached, so a purchase is seen on the next connect.
- **Lifetime access:** an entitlement with no `expires_date` counts as active. During a billing retry, `grace_period_expires_date` counts too.
- **Outages:** if RevenueCat can't be reached, or rejects the secret key, a device is let in when its last active result expired less than 24 hours ago. Otherwise it gets `4402`, never `4401`.
- **Pushes:** skip devices without an active entitlement (answered from the cache for active devices) and devices with an open socket.
- **Webhook:** `POST /v1/revenuecat`. Any event clears the cache for every id it names (`app_user_id`, `original_app_user_id`, `aliases`, and `transferred_from`/`transferred_to` on `TRANSFER`). `EXPIRATION` also drops the outage record and closes the device's open sockets with `4402` on every desktop it is paired with, found through a KV index written at pairing and cleaned up on revoke.

KV keys: `active:<device_id>` (cache), `last:<device_id>` (last known active-until, kept 24 hours past it), `desktops:<device_id>` (index for the webhook).

## Deploy

One-time setup, in this order, because the gate refuses every phone until the secret key is in place:

1. Create the KV namespace and put the `id` it prints into the `[[kv_namespaces]]` block in `wrangler.toml`:

   ```sh
   npx wrangler kv namespace create SUBSCRIPTIONS --config wrangler.toml
   ```

2. Store the RevenueCat secret API key (Project settings, API keys, a v1 secret key starting with `sk_`):

   ```sh
   npx wrangler secret put REVENUECAT_SECRET_KEY --config wrangler.toml
   ```

3. Choose a long random value for the webhook, for example `Bearer $(openssl rand -hex 32)`, and store the whole header value, including `Bearer `:

   ```sh
   npx wrangler secret put REVENUECAT_WEBHOOK_AUTH --config wrangler.toml
   ```

4. Deploy:

   ```sh
   cd infra/companion-relay
   npm ci
   npm test
   npx wrangler login
   npx wrangler deploy --config wrangler.toml
   ```

5. In the RevenueCat dashboard, add a webhook (Integrations, Webhooks) with the URL `https://companion.lathe-ide.com/v1/revenuecat` and set its Authorization header to exactly the value stored in step 3.

The route in `wrangler.toml` serves the Worker at `companion.lathe-ide.com` as a custom domain on the same Cloudflare zone as `updates.lathe-ide.com`. Wrangler creates the DNS record; don't replace records that belong to other apps on the zone.

Optional: if push security is enabled on the Expo project, store its access token:

```sh
npx wrangler secret put EXPO_ACCESS_TOKEN --config wrangler.toml
```

Rollback: `npx wrangler rollback --config wrangler.toml`.

## Local development

To run without RevenueCat, create `.dev.vars` (gitignored) next to `wrangler.toml`:

```sh
REVENUECAT_DISABLED="1"
```

With it set, every device counts as subscribed. `npx wrangler dev` then works without the secret key, and a local KV namespace stands in for the real one. Add `REVENUECAT_WEBHOOK_AUTH="Bearer something"` to try the webhook locally.

## Self-hosting

Anyone can deploy this Worker to their own Cloudflare account and point Lathe at it with the `companion.relay_url` setting. Remove the `[[routes]]` block to use the `workers.dev` address instead of a custom domain. A self-hosted relay that doesn't sell subscriptions should set `REVENUECAT_DISABLED` as a secret (`npx wrangler secret put REVENUECAT_DISABLED`, value `1`).

## Pairing link page

`GET /pair` serves the page behind Lathe's "Copy link". Android's Messages app doesn't make `lathe-companion://` links tappable and Chrome won't open one typed into its address bar, but both open an https link. Lathe copies `https://<relay host>/pair#<the QR code's query>`, and the page turns the fragment back into the app link: an `intent://` link on Android, `lathe-companion://` elsewhere. Browsers never send the fragment to the server, so the pairing token stays out of the relay's logs. The page loads nothing from outside and isn't cached.

## Protocol

The canonical spec is `src/protocol/README.md` in the `lathe-companion-app` repo. This section covers what the relay itself does, including the desktop-only messages the app doesn't see.

Connect with a WebSocket to `/v1/connect`:

| Client | Query |
|---|---|
| Desktop | `role=desktop&desktop_id=<id>` |
| Paired phone | `role=phone&desktop_id=<id>&device_id=<id>` |
| Phone pairing | `role=phone&desktop_id=<id>&device_id=<id>&pairing=1` |

The desktop id is the first 16 bytes of SHA-256 of the desktop's Ed25519 public key, base64url without padding (22 characters). The desktop needs no registration: proving the key behind its id is enough. Phone ids are chosen by the app (8 to 64 of `A-Za-z0-9_-`), and the desktop registers each phone's key.

### Authentication

Desktop and paired-phone sockets receive `{ "type": "challenge", "nonce": "<b64, 32 bytes>" }` and answer with an Ed25519 signature over the decoded nonce bytes:

- Desktop: `{ "type": "auth", "public_key": "<b64 raw 32 bytes>", "signature": "<b64>" }`
- Phone: `{ "type": "auth", "signature": "<b64>" }`

Success: `{ "type": "auth_ok" }`, then `desktop_status` for phones. Pairing sockets get no challenge; they send one `sealed` message.

### Close codes

| Code | Meaning |
|---|---|
| `4000` | Pairing connection finished: answered, closed by the desktop, or pairing wasn't open |
| `4401` | Not authorized: bad signature, unknown device, or a desktop key that doesn't match its id |
| `4402` | The device has no active subscription |
| `4403` | The desktop revoked this phone |
| `4409` | Desktop only: a newer connection with the same identity took over |

### Messages

Every client:

- `{"type":"ping"}` (exact text) is answered with `{"type":"pong"}` without waking the Durable Object. Send every 30 seconds.
- `{ "type": "frame", "seq", "nonce", "ciphertext" }` carries an encrypted message. `seq` is at least 1. The relay forwards only these fields, plus `device_id` on the desktop side.
- `{ "type": "error", "code", ... }` comes back on rejected requests.

Desktop to relay:

| Message | Effect |
|---|---|
| `frame` + `device_id` | Forward to that phone's sockets, or to its pairing socket (which is then closed with 4000) |
| `pairing_open { ttl_seconds }` | Allow pairing sockets for up to 300 seconds |
| `pairing_close` | Close the window and any pairing sockets |
| `authorize_device { device_id, sign_pk, name }` | Register a phone (max 10) |
| `revoke_device { device_id }` | Remove it and close its sockets with 4403 |
| `list_devices` | Reply `devices { list: [{ device_id, name, online, has_push }] }` |
| `push { kind, thread_id or terminal_id, device_id? }` | Notify phones that registered for `kind`, have no open socket, and have an active subscription (60 per hour per desktop) |

Push kinds: `needs_authorization`, `needs_input`, `finished`, `error` (each with `thread_id`), and `terminal_input` (with `terminal_id` instead).

Relay to desktop: `frame` with `device_id`, `sealed { device_id, ciphertext }`, `presence { device_id, online }`.

Phone to relay: `frame`, `register_push { expo_push_token, kinds? }`. Pairing sockets may only send `sealed { ciphertext }` (at most 3).

Relay to phone: `frame`, `desktop_status { online }`.

### Limits

Messages over 256 KiB, invalid JSON, and more than 50 messages per second sustained (burst 200) close the socket.

A socket that hasn't pinged for 75 seconds counts as gone, even if no close frame arrived (a suspended or killed phone app usually leaves this way). While anything is connected, a Durable Object alarm runs every 60 seconds, closes such sockets with `1001`, and tells the desktop the phone left (or the phones that the desktop left). Presence and `desktop_status` only count sockets that are still pinging.
