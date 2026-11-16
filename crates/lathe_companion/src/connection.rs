//! The desktop's socket to the companion relay: authenticates, keeps the
//! connection alive, and reconnects with backoff. Runs on the Tokio runtime and
//! talks to the foreground through channels.

use crate::crypto::Identity;
use anyhow::{Context as _, Result, anyhow, bail};
use async_tungstenite::tungstenite::{
    Message, client::IntoClientRequest as _, protocol::frame::coding::CloseCode,
};
use futures::{
    FutureExt as _, StreamExt as _,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use gpui::{App, Task};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use url::Url;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
// Two missed heartbeats plus slack: the relay answers every ping, so silence
// this long means the connection is dead even if TCP hasn't noticed.
const READ_TIMEOUT: Duration = Duration::from_secs(75);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
// Must match the text the relay's auto-response is registered for exactly.
const PING: &str = r#"{"type":"ping"}"#;

#[derive(Debug)]
pub enum RelayEvent {
    Connected,
    Disconnected {
        error: Option<String>,
    },
    /// The relay refused this desktop for a reason retrying won't fix (another
    /// Lathe took over the identity, or the key was rejected).
    Stopped {
        reason: String,
    },
    Message(Value),
}

/// Handle to a running relay connection. Dropping it closes the socket and stops
/// reconnecting.
pub struct RelayConnection {
    outgoing: UnboundedSender<String>,
    _task: Task<Result<()>>,
}

impl RelayConnection {
    pub fn start(
        relay_url: &str,
        identity: Arc<Identity>,
        cx: &App,
    ) -> Result<(Self, UnboundedReceiver<RelayEvent>)> {
        let url = connect_url(relay_url, &identity.desktop_id())?;
        let (outgoing_tx, outgoing_rx) = unbounded();
        let (events_tx, events_rx) = unbounded();
        let task = gpui_tokio::Tokio::spawn_result(cx, async move {
            run(url, identity, outgoing_rx, events_tx).await;
            Ok(())
        });
        Ok((
            Self {
                outgoing: outgoing_tx,
                _task: task,
            },
            events_rx,
        ))
    }

    #[cfg(test)]
    pub fn for_test() -> (Self, UnboundedReceiver<String>) {
        let (outgoing, receiver) = unbounded();
        (
            Self {
                outgoing,
                _task: Task::ready(Ok(())),
            },
            receiver,
        )
    }

    /// Queues a message for the relay. Messages queued while disconnected are
    /// dropped on reconnect, since frames are only meaningful to the session
    /// they were built for.
    pub fn send(&self, message: &Value) {
        self.outgoing.unbounded_send(message.to_string()).ok();
    }
}

pub fn connect_url(relay_url: &str, desktop_id: &str) -> Result<Url> {
    let mut url = Url::parse(relay_url).context("invalid companion relay URL")?;
    match url.scheme() {
        "wss" | "ws" => {}
        scheme => bail!("companion relay URL must use wss://, not {scheme}://"),
    }
    url.set_path("/v1/connect");
    url.query_pairs_mut()
        .clear()
        .append_pair("role", "desktop")
        .append_pair("desktop_id", desktop_id);
    Ok(url)
}

async fn run(
    url: Url,
    identity: Arc<Identity>,
    mut outgoing: UnboundedReceiver<String>,
    events: UnboundedSender<RelayEvent>,
) {
    let mut backoff = MIN_BACKOFF;
    loop {
        while outgoing.try_recv().is_ok() {}

        let outcome = session(&url, &identity, &mut outgoing, &events, &mut backoff).await;
        match outcome {
            Outcome::Stop(reason) => {
                log::warn!("companion: relay stopped this desktop: {reason}");
                events.unbounded_send(RelayEvent::Stopped { reason }).ok();
                return;
            }
            Outcome::Retry(error) => {
                log::info!(
                    "companion: relay connection ended ({error:#}), retrying in {}s",
                    backoff.as_secs()
                );
                if events
                    .unbounded_send(RelayEvent::Disconnected {
                        error: Some(format!("{error:#}")),
                    })
                    .is_err()
                {
                    return;
                }
            }
            Outcome::Closed => return,
        }

        tokio::time::sleep(jittered(backoff)).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

enum Outcome {
    Retry(anyhow::Error),
    Stop(String),
    /// The foreground dropped its handle.
    Closed,
}

async fn session(
    url: &Url,
    identity: &Identity,
    outgoing: &mut UnboundedReceiver<String>,
    events: &UnboundedSender<RelayEvent>,
    backoff: &mut Duration,
) -> Outcome {
    let mut socket = match tokio::time::timeout(HANDSHAKE_TIMEOUT, connect(url, identity)).await {
        Ok(Ok(socket)) => socket,
        Ok(Err(error)) => return classify(error),
        Err(_) => return Outcome::Retry(anyhow!("timed out connecting to the relay")),
    };
    *backoff = MIN_BACKOFF;
    if events.unbounded_send(RelayEvent::Connected).is_err() {
        return Outcome::Closed;
    }

    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await;
    let mut last_heard = tokio::time::Instant::now();
    loop {
        let deadline = tokio::time::sleep_until(last_heard + READ_TIMEOUT);
        futures::select_biased! {
            message = socket.next().fuse() => match message {
                Some(Ok(Message::Text(text))) => {
                    last_heard = tokio::time::Instant::now();
                    let Ok(value) = serde_json::from_str::<Value>(&text) else {
                        log::warn!("companion: relay sent invalid JSON");
                        continue;
                    };
                    if value["type"] == "pong" {
                        continue;
                    }
                    if events.unbounded_send(RelayEvent::Message(value)).is_err() {
                        return Outcome::Closed;
                    }
                }
                Some(Ok(Message::Close(frame))) => return close_outcome(frame.map(|frame| (frame.code, frame.reason.to_string()))),
                Some(Ok(_)) => last_heard = tokio::time::Instant::now(),
                Some(Err(error)) => return Outcome::Retry(error.into()),
                None => return Outcome::Retry(anyhow!("relay closed the connection")),
            },
            message = outgoing.next().fuse() => match message {
                Some(text) => {
                    if let Err(error) = socket.send(Message::text(text)).await {
                        return Outcome::Retry(error.into());
                    }
                }
                None => {
                    socket.close(None).await.ok();
                    return Outcome::Closed;
                }
            },
            _ = heartbeat.tick().fuse() => {
                if let Err(error) = socket.send(Message::text(PING)).await {
                    return Outcome::Retry(error.into());
                }
            },
            _ = deadline.fuse() => return Outcome::Retry(anyhow!("relay stopped responding")),
        }
    }
}

type Socket = async_tungstenite::WebSocketStream<
    async_tungstenite::tokio::ClientStream<tokio::net::TcpStream>,
>;

#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

async fn connect(url: &Url, identity: &Identity) -> Result<Socket> {
    let host = url.host_str().context("relay URL has no host")?;
    let port = url
        .port_or_known_default()
        .context("relay URL has no port")?;
    let stream = tokio::net::TcpStream::connect((host, port))
        .await
        .context("couldn't reach the relay")?;
    let request = url.as_str().into_client_request()?;
    let (mut socket, _) = async_tungstenite::tokio::client_async_tls_with_connector_and_config(
        request,
        stream,
        Some(Arc::new(http_client_tls::tls_config()).into()),
        None,
    )
    .await
    .context("relay handshake failed")?;

    let challenge = next_json(&mut socket).await?;
    if challenge["type"] != "challenge" {
        bail!("relay sent {} instead of a challenge", challenge["type"]);
    }
    let nonce = challenge["nonce"]
        .as_str()
        .context("relay challenge has no nonce")?;
    let auth = serde_json::json!({
        "type": "auth",
        "public_key": identity.signing_public_key(),
        "signature": identity.sign_challenge(nonce)?,
    });
    socket.send(Message::text(auth.to_string())).await?;

    let auth_ok = next_json(&mut socket).await?;
    if auth_ok["type"] != "auth_ok" {
        bail!("relay sent {} instead of auth_ok", auth_ok["type"]);
    }
    Ok(socket)
}

async fn next_json(socket: &mut Socket) -> Result<Value> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return Ok(serde_json::from_str(&text)?),
            Some(Ok(Message::Close(frame))) => {
                let (code, reason) = frame
                    .map(|frame| (u16::from(frame.code), frame.reason.to_string()))
                    .unwrap_or((1005, String::new()));
                if is_permanent(code) {
                    return Err(Refused(format!("{reason} ({code})")).into());
                }
                bail!("relay closed the connection: {reason} ({code})");
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error.into()),
            None => bail!("relay closed the connection"),
        }
    }
}

fn classify(error: anyhow::Error) -> Outcome {
    match error.downcast_ref::<Refused>() {
        Some(refused) => Outcome::Stop(refused.0.clone()),
        None => Outcome::Retry(error),
    }
}

fn close_outcome(frame: Option<(CloseCode, String)>) -> Outcome {
    let Some((code, reason)) = frame else {
        return Outcome::Retry(anyhow!("relay closed the connection"));
    };
    let code = u16::from(code);
    if is_permanent(code) {
        Outcome::Stop(format!("{reason} ({code})"))
    } else {
        Outcome::Retry(anyhow!("relay closed the connection: {reason} ({code})"))
    }
}

/// 4401: the relay rejected the key. 4409: another connection with this
/// identity replaced ours, so reconnecting would just fight it.
fn is_permanent(code: u16) -> bool {
    matches!(code, 4401 | 4409)
}

/// Spreads reconnects over ±20% so many desktops don't retry in lockstep after
/// a relay deploy.
fn jittered(duration: Duration) -> Duration {
    let mut byte = [0u8; 1];
    crate::crypto::fill_random(&mut byte);
    let factor = 0.8 + 0.4 * (f64::from(byte[0]) / 255.0);
    duration.mul_f64(factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_connect_url() {
        let url = connect_url("wss://companion.lathe-ide.com", "abc").unwrap();
        assert_eq!(
            url.as_str(),
            "wss://companion.lathe-ide.com/v1/connect?role=desktop&desktop_id=abc"
        );
        let url = connect_url("ws://localhost:8799/ignored?x=1", "abc").unwrap();
        assert_eq!(
            url.as_str(),
            "ws://localhost:8799/v1/connect?role=desktop&desktop_id=abc"
        );
        assert!(connect_url("https://companion.lathe-ide.com", "abc").is_err());
    }

    #[test]
    fn only_identity_conflicts_stop_reconnecting() {
        assert!(is_permanent(4401));
        assert!(is_permanent(4409));
        assert!(!is_permanent(1001));
        assert!(!is_permanent(4000));
    }

    /// Run against a local relay: `npx wrangler dev --port 8799` in
    /// infra/companion-relay, then
    /// `LATHE_COMPANION_TEST_RELAY=ws://localhost:8799 cargo test -p lathe_companion -- --ignored`.
    #[test]
    #[ignore]
    fn authenticates_and_heartbeats_against_a_local_relay() {
        let Ok(relay_url) = std::env::var("LATHE_COMPANION_TEST_RELAY") else {
            return;
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let identity = Identity::generate();
            let url = connect_url(&relay_url, &identity.desktop_id()).unwrap();
            let mut socket = connect(&url, &identity).await.unwrap();
            socket.send(Message::text(PING)).await.unwrap();
            assert_eq!(next_json(&mut socket).await.unwrap()["type"], "pong");
            socket
                .send(Message::text(r#"{"type":"list_devices"}"#))
                .await
                .unwrap();
            let devices = next_json(&mut socket).await.unwrap();
            assert_eq!(
                devices,
                serde_json::json!({ "type": "devices", "list": [] })
            );

            // A second connection with the same identity takes over, and the
            // first must treat that as permanent rather than reconnect.
            let _second = connect(&url, &identity).await.unwrap();
            let error = next_json(&mut socket).await.unwrap_err();
            assert!(matches!(classify(error), Outcome::Stop(_)));
        });
    }

    /// The desktop half of a pairing with the phone app's own code, through a
    /// local relay. Start `npx wrangler dev --port 8799` in
    /// infra/companion-relay, run this with `LATHE_COMPANION_TEST_RELAY` and
    /// `LATHE_COMPANION_PAIR_URI_FILE` set, then run the app's pairing code
    /// against the URI it writes.
    #[test]
    #[ignore]
    fn pairs_with_the_app_through_a_local_relay() {
        use crate::crypto;
        use crypto_box::PublicKey;
        use serde_json::json;

        let (Ok(relay_url), Ok(uri_file)) = (
            std::env::var("LATHE_COMPANION_TEST_RELAY"),
            std::env::var("LATHE_COMPANION_PAIR_URI_FILE"),
        ) else {
            return;
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let identity = Identity::generate();
            let url = connect_url(&relay_url, &identity.desktop_id()).unwrap();
            let mut socket = connect(&url, &identity).await.unwrap();
            let send = async |socket: &mut Socket, message: serde_json::Value| {
                socket.send(Message::text(message.to_string())).await.unwrap()
            };
            send(&mut socket, json!({ "type": "pairing_open", "ttl_seconds": 120 })).await;

            let token = [7u8; 16];
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("v", "1")
                .append_pair("relay", &relay_url)
                .append_pair("desktop_id", &identity.desktop_id())
                .append_pair("desktop_pk", &crypto::encode_url_safe(identity.box_public_key().as_bytes()))
                .append_pair("token", &crypto::encode_url_safe(&token))
                .append_pair("name", "Lathe interop test")
                .finish();
            std::fs::write(&uri_file, format!("lathe-companion://pair?{query}")).unwrap();

            let next = async |socket: &mut Socket, kind: &str| loop {
                let message = tokio::time::timeout(Duration::from_secs(60), next_json(socket))
                    .await
                    .expect("timed out waiting for the phone")
                    .unwrap();
                if message["type"] == kind {
                    return message;
                }
            };

            let sealed = next(&mut socket, "sealed").await;
            let request: serde_json::Value =
                serde_json::from_slice(&identity.unseal(sealed["ciphertext"].as_str().unwrap()).unwrap())
                    .unwrap();
            assert_eq!(request["type"], "pair_request");
            assert_eq!(
                crypto::decode_either(request["token"].as_str().unwrap()).unwrap(),
                token
            );
            assert_eq!(request["device_id"], sealed["device_id"]);
            let device_id = request["device_id"].as_str().unwrap().to_string();
            let phone_box = PublicKey::from_bytes(
                crypto::decode_key(request["device_box_pk"].as_str().unwrap()).unwrap(),
            );
            send(&mut socket, json!({
                "type": "authorize_device",
                "device_id": device_id,
                "sign_pk": request["device_sign_pk"],
                "name": request["device_name"],
            }))
            .await;
            let accepted = json!({ "type": "pair_accepted", "desktop_name": "Lathe interop test", "channel": "dev" });
            let (nonce, ciphertext) = identity.encrypt_for(&phone_box, accepted.to_string().as_bytes()).unwrap();
            send(&mut socket, json!({ "type": "frame", "device_id": device_id, "seq": 1, "nonce": nonce, "ciphertext": ciphertext })).await;
            send(&mut socket, json!({ "type": "pairing_close" })).await;

            // The phone reconnects as a paired device and talks over frames.
            let presence = next(&mut socket, "presence").await;
            assert_eq!(presence["device_id"], json!(device_id));
            let hello = json!({
                "type": "hello",
                "protocol_version": 1,
                "lathe_version": "0.0.0",
                "channel": "dev",
                "desktop_name": "Lathe interop test",
            });
            let (nonce, ciphertext) = identity.encrypt_for(&phone_box, hello.to_string().as_bytes()).unwrap();
            send(&mut socket, json!({ "type": "frame", "device_id": device_id, "seq": 2, "nonce": nonce, "ciphertext": ciphertext })).await;

            let frame = next(&mut socket, "frame").await;
            let plaintext = identity
                .decrypt_from(&phone_box, frame["nonce"].as_str().unwrap(), frame["ciphertext"].as_str().unwrap())
                .unwrap();
            let message: serde_json::Value = serde_json::from_slice(&plaintext).unwrap();
            eprintln!("desktop received from the phone: {message}");
            assert!(message["type"].is_string());
        });
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        for _ in 0..100 {
            let jittered = jittered(Duration::from_secs(10));
            assert!(jittered >= Duration::from_secs(8) && jittered <= Duration::from_secs(12));
        }
    }
}
