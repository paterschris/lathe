//! Connection to the relay, paired phones, and the pairing handshake.

use crate::{
    CompanionSettings,
    connection::{RelayConnection, RelayEvent},
    crypto::{self, Identity},
};
use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use crypto_box::PublicKey;
use db::kvp::KeyValueStore;
use futures::{StreamExt as _, channel::mpsc::UnboundedReceiver};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, Task};
use release_channel::ReleaseChannel;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use settings::Settings as _;
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use util::ResultExt as _;

const PAIRING_WINDOW: Duration = Duration::from_secs(300);
/// The relay rejects messages over 256 KiB, and encryption plus base64 grows a
/// message by about a third, so plaintext has to stay well under that.
pub const MAX_FRAME_PLAINTEXT_BYTES: usize = 160 * 1024;
const DEVICES_NAMESPACE: &str = "lathe_companion_devices";

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionStatus {
    Off,
    Connecting,
    Connected,
    Reconnecting { error: Option<String> },
    Stopped { reason: String },
    Failed { error: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairedDevice {
    pub device_id: String,
    pub name: String,
    /// Base64 Ed25519 key the relay checks the phone's sign-in against. Kept
    /// locally so a relay that lost its storage can be re-taught.
    pub sign_public_key: String,
    /// Base64 X25519 key frames to this phone are encrypted to.
    pub box_public_key: String,
    /// Seconds since the Unix epoch.
    pub paired_at: u64,
    /// Workspaces this phone gets no pushes for, as it last sent with
    /// `set_muted_workspaces`. Kept with the device because pushes go out
    /// while the phone is offline, after Lathe may have restarted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub muted_workspaces: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PairingRequest {
    pub device_id: String,
    pub device_name: String,
    sign_public_key: String,
    box_public_key: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PairingState {
    WaitingForScan {
        qr_payload: String,
        /// The same pairing as an https link, for sharing to a phone: Android's
        /// Messages and Chrome don't open `lathe-companion://` links.
        web_link: Option<String>,
        expires_at: SystemTime,
    },
    AwaitingApproval(PairingRequest),
    Paired {
        device_name: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum CompanionEvent {
    Changed,
    DeviceOnline(String),
    DeviceOffline(String),
    DeviceRemoved(String),
    /// An authenticated, decrypted message from a paired phone.
    PhoneMessage {
        device_id: String,
        message: Value,
    },
}

#[derive(Deserialize)]
struct SealedPairRequest {
    #[serde(rename = "type")]
    kind: String,
    token: String,
    device_id: String,
    device_name: String,
    device_box_pk: String,
    device_sign_pk: String,
}

pub struct Companion {
    identity: Option<Arc<Identity>>,
    connection: Option<RelayConnection>,
    status: ConnectionStatus,
    devices: Vec<PairedDevice>,
    online: HashSet<String>,
    pairing: Option<PairingState>,
    pairing_token: Option<Vec<u8>>,
    last_sent_seq: u64,
    last_received_seq: HashMap<String, u64>,
    channel: ReleaseChannel,
    _settings_subscription: Option<gpui::Subscription>,
    _start: Option<Task<()>>,
    _events: Option<Task<()>>,
}

struct GlobalCompanion(Entity<Companion>);

impl Global for GlobalCompanion {}

impl EventEmitter<CompanionEvent> for Companion {}

pub(crate) fn init(cx: &mut App) {
    prefetch_computer_name(cx);
    let channel = ReleaseChannel::try_global(cx).unwrap_or(ReleaseChannel::Dev);
    let companion = cx.new(|cx| {
        let mut companion = Companion::new(channel, load_devices(channel, cx));
        let mut enabled = CompanionSettings::get_global(cx).enabled;
        let mut relay_url = CompanionSettings::get_global(cx).relay_url.clone();
        companion._settings_subscription =
            Some(cx.observe_global::<settings::SettingsStore>(move |this: &mut Companion, cx| {
                let settings = CompanionSettings::get_global(cx);
                if settings.enabled != enabled || settings.relay_url != relay_url {
                    enabled = settings.enabled;
                    relay_url = settings.relay_url.clone();
                    this.restart(cx);
                }
            }));
        companion.restart(cx);
        companion
    });
    cx.set_global(GlobalCompanion(companion));
}

impl Companion {
    fn new(channel: ReleaseChannel, devices: Vec<PairedDevice>) -> Self {
        Self {
            identity: None,
            connection: None,
            status: ConnectionStatus::Off,
            devices,
            online: HashSet::default(),
            pairing: None,
            pairing_token: None,
            last_sent_seq: 0,
            last_received_seq: HashMap::default(),
            channel,
            _settings_subscription: None,
            _start: None,
            _events: None,
        }
    }

    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalCompanion>()
            .map(|global| global.0.clone())
    }

    pub fn status(&self) -> &ConnectionStatus {
        &self.status
    }

    pub fn devices(&self) -> &[PairedDevice] {
        &self.devices
    }

    pub fn is_online(&self, device_id: &str) -> bool {
        self.online.contains(device_id)
    }

    pub fn pairing(&self) -> Option<&PairingState> {
        self.pairing.as_ref()
    }

    pub fn online_devices(&self) -> impl Iterator<Item = &String> {
        self.online.iter()
    }

    /// Asks the relay to notify phones that registered for `kind`. The relay
    /// sends fixed text under the thread's `title`, so the title is the only
    /// thing about the thread that leaves this machine, never its messages.
    /// Phones that muted `workspace_id` are left out.
    pub fn push(&self, kind: &str, thread_id: &str, title: Option<&str>, workspace_id: Option<&str>) {
        let push = json!({ "type": "push", "kind": kind, "thread_id": thread_id });
        self.push_to_unmuted(with_title(push, title), workspace_id);
    }

    /// Asks the relay to tell phones a terminal is waiting for input. The
    /// relay sends fixed text under the terminal's `title`, so nothing else
    /// about the terminal leaves this machine but its random id. Phones that
    /// muted `workspace_id` are left out.
    pub fn push_terminal(&self, terminal_id: &str, title: Option<&str>, workspace_id: Option<&str>) {
        let push = json!({ "type": "push", "kind": "terminal_input", "terminal_id": terminal_id });
        self.push_to_unmuted(with_title(push, title), workspace_id);
    }

    /// One push for every phone when none muted the workspace (the relay
    /// keeps its per-hour count low); otherwise one per phone that didn't,
    /// addressed with `device_id`. The workspace never reaches the relay.
    fn push_to_unmuted(&self, push: Value, workspace_id: Option<&str>) {
        let (muted, listening): (Vec<_>, Vec<_>) = self.devices.iter().partition(|device| {
            workspace_id.is_some_and(|id| device.muted_workspaces.iter().any(|muted| muted == id))
        });
        if listening.is_empty() {
            return;
        }
        if muted.is_empty() {
            self.send(push);
            return;
        }
        for device in listening {
            let mut addressed = push.clone();
            addressed["device_id"] = json!(device.device_id);
            self.send(addressed);
        }
    }

    /// Records the workspaces a phone wants no pushes for.
    pub fn set_muted_workspaces(&mut self, device_id: &str, workspace_ids: Vec<String>, cx: &mut Context<Self>) {
        let Some(device) = self.devices.iter_mut().find(|device| device.device_id == device_id) else {
            return;
        };
        if device.muted_workspaces == workspace_ids {
            return;
        }
        device.muted_workspaces = workspace_ids;
        save_devices(self.channel, &self.devices, cx);
    }

    fn set_all_offline(&mut self, cx: &mut Context<Self>) {
        for device_id in self.online.drain().collect::<Vec<_>>() {
            cx.emit(CompanionEvent::DeviceOffline(device_id));
        }
    }

    fn restart(&mut self, cx: &mut Context<Self>) {
        self.connection = None;
        self._events = None;
        self.set_all_offline(cx);
        self.pairing = None;
        self.pairing_token = None;
        let settings = CompanionSettings::get_global(cx).clone();
        if !settings.enabled {
            self._start = None;
            self.set_status(ConnectionStatus::Off, cx);
            return;
        }

        self.set_status(ConnectionStatus::Connecting, cx);
        let channel = self.channel;
        self._start = Some(cx.spawn(async move |this, cx| {
            let identity = match load_or_create_identity(channel, cx).await {
                Ok(identity) => Arc::new(identity),
                Err(error) => {
                    log::error!("companion: couldn't load the desktop identity: {error:#}");
                    this.update(cx, |this, cx| {
                        this.set_status(
                            ConnectionStatus::Failed {
                                error: format!("Couldn't access the keychain: {error:#}"),
                            },
                            cx,
                        )
                    })
                    .ok();
                    return;
                }
            };
            this.update(cx, |this, cx| {
                match RelayConnection::start(&settings.relay_url, identity.clone(), cx) {
                    Ok((connection, events)) => {
                        this.identity = Some(identity);
                        this.attach(connection, events, cx);
                    }
                    Err(error) => this.set_status(
                        ConnectionStatus::Failed {
                            error: format!("{error:#}"),
                        },
                        cx,
                    ),
                }
            })
            .ok();
        }));
    }

    fn attach(
        &mut self,
        connection: RelayConnection,
        mut events: UnboundedReceiver<RelayEvent>,
        cx: &mut Context<Self>,
    ) {
        self.connection = Some(connection);
        self._events = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |this, cx| this.handle_relay_event(event, cx))
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    fn set_status(&mut self, status: ConnectionStatus, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.emit(CompanionEvent::Changed);
            cx.notify();
        }
    }

    fn send(&self, message: Value) {
        if let Some(connection) = &self.connection {
            connection.send(&message);
        }
    }

    fn handle_relay_event(&mut self, event: RelayEvent, cx: &mut Context<Self>) {
        match event {
            RelayEvent::Connected => {
                log::info!("companion: connected to the relay");
                self.set_status(ConnectionStatus::Connected, cx);
                self.send(json!({ "type": "list_devices" }));
            }
            RelayEvent::Disconnected { error } => {
                self.set_all_offline(cx);
                self.set_status(ConnectionStatus::Reconnecting { error }, cx);
            }
            RelayEvent::Stopped { reason } => {
                self.set_all_offline(cx);
                self.set_status(ConnectionStatus::Stopped { reason }, cx);
            }
            RelayEvent::Message(message) => self.handle_relay_message(message, cx),
        }
    }

    pub(crate) fn handle_relay_message(&mut self, message: Value, cx: &mut Context<Self>) {
        match message["type"].as_str() {
            Some("devices") => self.reconcile_devices(&message["list"], cx),
            Some("presence") => {
                let Some(device_id) = message["device_id"].as_str() else {
                    return;
                };
                // Phones count frame `seq` per connection, so a phone that
                // reconnected starts over at 1.
                self.last_received_seq.remove(device_id);
                if !self.devices.iter().any(|device| device.device_id == device_id) {
                    return;
                }
                if message["online"].as_bool() == Some(true) {
                    // Every new connection gets the full state, even when the
                    // phone already had another one open.
                    log::info!("companion: phone {device_id} connected");
                    self.online.insert(device_id.to_string());
                    cx.emit(CompanionEvent::DeviceOnline(device_id.to_string()));
                } else if self.online.remove(device_id) {
                    log::info!("companion: phone {device_id} disconnected");
                    cx.emit(CompanionEvent::DeviceOffline(device_id.to_string()));
                }
                cx.emit(CompanionEvent::Changed);
                cx.notify();
            }
            Some("sealed") => {
                if let Err(error) = self.handle_pairing_request(&message, cx) {
                    log::warn!("companion: ignored a pairing request: {error:#}");
                }
            }
            Some("frame") => {
                if let Err(error) = self.handle_frame(&message, cx) {
                    log::warn!("companion: dropped a frame: {error:#}");
                }
            }
            Some("error") => log::warn!("companion: relay reported {}", message),
            other => log::debug!("companion: ignoring relay message {other:?}"),
        }
    }

    /// The local list is the source of truth because it holds the keys frames
    /// are encrypted with. The relay is re-taught phones it forgot, and told to
    /// drop phones this desktop can't talk to.
    fn reconcile_devices(&mut self, list: &Value, cx: &mut Context<Self>) {
        let Some(list) = list.as_array() else {
            return;
        };
        let mut known_to_relay = HashSet::default();
        for entry in list {
            let Some(device_id) = entry["device_id"].as_str() else {
                continue;
            };
            known_to_relay.insert(device_id.to_string());
            if !self.devices.iter().any(|device| device.device_id == device_id) {
                self.send(json!({ "type": "revoke_device", "device_id": device_id }));
                continue;
            }
            if entry["online"].as_bool() == Some(true) && self.online.insert(device_id.to_string())
            {
                cx.emit(CompanionEvent::DeviceOnline(device_id.to_string()));
            }
        }
        for device in &self.devices {
            if !known_to_relay.contains(&device.device_id) {
                self.send(authorize_message(device));
            }
        }
        cx.emit(CompanionEvent::Changed);
        cx.notify();
    }

    pub fn start_pairing(&mut self, cx: &mut Context<Self>) -> Result<()> {
        if self.status != ConnectionStatus::Connected {
            return Err(anyhow!("Lathe isn't connected to the companion relay"));
        }
        let identity = self.identity.as_ref().context("no desktop identity")?;
        let mut token = vec![0u8; 16];
        crypto::fill_random(&mut token);
        let relay_url = CompanionSettings::get_global(cx).relay_url.clone();
        let qr_payload = pairing_payload(
            &relay_url,
            identity,
            &crypto::encode_url_safe(&token),
            &desktop_name(self.channel),
        );
        self.send(json!({ "type": "pairing_open", "ttl_seconds": PAIRING_WINDOW.as_secs() }));
        self.pairing_token = Some(token);
        let web_link = pairing_web_link(&relay_url, &qr_payload);
        self.pairing = Some(PairingState::WaitingForScan {
            qr_payload,
            web_link,
            expires_at: SystemTime::now() + PAIRING_WINDOW,
        });
        cx.emit(CompanionEvent::Changed);
        cx.notify();
        Ok(())
    }

    pub fn cancel_pairing(&mut self, cx: &mut Context<Self>) {
        if self.pairing.take().is_some() {
            self.pairing_token = None;
            self.send(json!({ "type": "pairing_close" }));
            cx.emit(CompanionEvent::Changed);
            cx.notify();
        }
    }

    fn handle_pairing_request(&mut self, message: &Value, cx: &mut Context<Self>) -> Result<()> {
        let Some(PairingState::WaitingForScan { expires_at, .. }) = &self.pairing else {
            return Err(anyhow!("not pairing"));
        };
        if SystemTime::now() > *expires_at {
            return Err(anyhow!("the pairing window has expired"));
        }
        let identity = self.identity.as_ref().context("no desktop identity")?;
        let relay_device_id = message["device_id"]
            .as_str()
            .context("sealed request has no device id")?;
        let ciphertext = message["ciphertext"].as_str().context("missing ciphertext")?;
        let request: SealedPairRequest = serde_json::from_slice(&identity.unseal(ciphertext)?)
            .context("malformed pairing request")?;
        if request.kind != "pair_request" {
            return Err(anyhow!("unexpected sealed message {}", request.kind));
        }
        let token = self.pairing_token.as_deref().context("no pairing token")?;
        let presented = crypto::decode_either(&request.token).context("malformed pairing token")?;
        if !crypto::constant_time_eq(&presented, token) {
            return Err(anyhow!("wrong pairing token"));
        }
        // The relay routes the answer by the id the pairing socket connected
        // with, so the sealed request has to agree with it.
        if request.device_id != relay_device_id {
            return Err(anyhow!("device id doesn't match the pairing connection"));
        }
        crypto::decode_key(&request.device_sign_pk)?;
        crypto::decode_key(&request.device_box_pk)?;

        let device_name: String = request.device_name.chars().take(100).collect();
        self.pairing = Some(PairingState::AwaitingApproval(PairingRequest {
            device_id: request.device_id,
            device_name: if device_name.trim().is_empty() {
                "Phone".into()
            } else {
                device_name
            },
            sign_public_key: request.device_sign_pk,
            box_public_key: request.device_box_pk,
        }));
        cx.emit(CompanionEvent::Changed);
        cx.notify();
        Ok(())
    }

    /// Accepts or rejects the phone waiting for approval.
    pub fn respond_to_pairing(&mut self, accept: bool, cx: &mut Context<Self>) -> Result<()> {
        let Some(PairingState::AwaitingApproval(request)) = self.pairing.clone() else {
            return Err(anyhow!("no phone is waiting for approval"));
        };
        let identity = self.identity.clone().context("no desktop identity")?;
        let box_key = PublicKey::from_bytes(crypto::decode_key(&request.box_public_key)?);

        let reply = if accept {
            let device = PairedDevice {
                device_id: request.device_id.clone(),
                name: request.device_name.clone(),
                sign_public_key: request.sign_public_key.clone(),
                box_public_key: request.box_public_key.clone(),
                paired_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or_default(),
                muted_workspaces: Vec::new(),
            };
            self.send(authorize_message(&device));
            self.devices.retain(|existing| existing.device_id != device.device_id);
            self.devices.push(device);
            save_devices(self.channel, &self.devices, cx);
            json!({
                "type": "pair_accepted",
                "desktop_name": desktop_name(self.channel),
                "channel": self.channel.dev_name(),
            })
        } else {
            json!({ "type": "pair_rejected" })
        };

        let (nonce, ciphertext) = identity.encrypt_for(&box_key, reply.to_string().as_bytes())?;
        let seq = self.next_seq();
        self.send(json!({
            "type": "frame",
            "device_id": request.device_id,
            "seq": seq,
            "nonce": nonce,
            "ciphertext": ciphertext,
        }));
        self.send(json!({ "type": "pairing_close" }));
        self.pairing_token = None;
        self.pairing = accept.then(|| PairingState::Paired {
            device_name: request.device_name,
        });
        cx.emit(CompanionEvent::Changed);
        cx.notify();
        Ok(())
    }

    pub fn revoke(&mut self, device_id: &str, cx: &mut Context<Self>) {
        self.devices.retain(|device| device.device_id != device_id);
        self.online.remove(device_id);
        cx.emit(CompanionEvent::DeviceRemoved(device_id.to_string()));
        self.send(json!({ "type": "revoke_device", "device_id": device_id }));
        save_devices(self.channel, &self.devices, cx);
        cx.emit(CompanionEvent::Changed);
        cx.notify();
    }

    /// Encrypts a message for one paired phone and sends it if the relay is up.
    pub fn send_to_device(&mut self, device_id: &str, message: &Value) -> Result<()> {
        let identity = self.identity.as_ref().context("no desktop identity")?;
        let device = self
            .devices
            .iter()
            .find(|device| device.device_id == device_id)
            .context("unknown device")?;
        let box_key = PublicKey::from_bytes(crypto::decode_key(&device.box_public_key)?);
        let plaintext = message.to_string();
        // An oversized frame doesn't fail on its own: the relay or Cloudflare
        // drops the whole connection, taking every phone's session with it.
        if plaintext.len() > MAX_FRAME_PLAINTEXT_BYTES {
            return Err(anyhow!(
                "a {} message of {} bytes is too large for the relay",
                message["type"],
                plaintext.len()
            ));
        }
        let (nonce, ciphertext) = identity.encrypt_for(&box_key, plaintext.as_bytes())?;
        let seq = self.next_seq();
        self.send(json!({
            "type": "frame",
            "device_id": device_id,
            "seq": seq,
            "nonce": nonce,
            "ciphertext": ciphertext,
        }));
        Ok(())
    }

    /// Phones drop frames whose `seq` isn't above the last one they saw on
    /// their connection, and a phone's connection outlives a Lathe restart.
    /// Seeding from the clock keeps a restarted Lathe ahead of the old counter.
    fn next_seq(&mut self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default();
        self.last_sent_seq = (self.last_sent_seq + 1).max(now);
        self.last_sent_seq
    }

    fn handle_frame(&mut self, message: &Value, cx: &mut Context<Self>) -> Result<()> {
        let identity = self.identity.as_ref().context("no desktop identity")?;
        let device_id = message["device_id"].as_str().context("frame has no sender")?;
        let seq = message["seq"].as_u64().context("frame has no seq")?;
        if self
            .last_received_seq
            .get(device_id)
            .is_some_and(|last| seq <= *last)
        {
            return Err(anyhow!("replayed frame {seq} from {device_id}"));
        }
        let device = self
            .devices
            .iter()
            .find(|device| device.device_id == device_id)
            .context("frame from a device that isn't paired")?;
        let box_key = PublicKey::from_bytes(crypto::decode_key(&device.box_public_key)?);
        let plaintext = identity.decrypt_from(
            &box_key,
            message["nonce"].as_str().context("frame has no nonce")?,
            message["ciphertext"]
                .as_str()
                .context("frame has no ciphertext")?,
        )?;
        let message = serde_json::from_slice(&plaintext).context("frame is not JSON")?;
        self.last_received_seq.insert(device_id.to_string(), seq);
        cx.emit(CompanionEvent::PhoneMessage {
            device_id: device_id.to_string(),
            message,
        });
        Ok(())
    }
}

fn authorize_message(device: &PairedDevice) -> Value {
    json!({
        "type": "authorize_device",
        "device_id": device.device_id,
        "sign_pk": device.sign_public_key,
        "name": device.name,
    })
}

fn pairing_payload(relay_url: &str, identity: &Identity, token: &str, name: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("v", "1")
        .append_pair("relay", relay_url)
        .append_pair("desktop_id", &identity.desktop_id())
        .append_pair(
            "desktop_pk",
            &crypto::encode_url_safe(identity.box_public_key().as_bytes()),
        )
        .append_pair("token", token)
        .append_pair("name", name)
        .finish();
    format!("lathe-companion://pair?{query}")
}

/// The relay's `/pair` page with the pairing in the fragment, which browsers
/// never send to a server, so the token stays out of the relay's logs.
fn pairing_web_link(relay_url: &str, qr_payload: &str) -> Option<String> {
    let query = qr_payload.split_once('?')?.1;
    let mut url = url::Url::parse(relay_url).ok()?;
    let scheme = match url.scheme() {
        "wss" => "https",
        "ws" => "http",
        _ => return None,
    };
    url.set_scheme(scheme).ok()?;
    url.set_path("/pair");
    url.set_query(None);
    url.set_fragment(Some(query));
    Some(url.to_string())
}

/// "Lathe Beta on Chris's MacBook Pro". Each channel pairs separately, so the
/// app shows which one this is.
pub(crate) fn desktop_name(channel: ReleaseChannel) -> String {
    let computer = computer_name().unwrap_or_else(|| "this computer".into());
    format!("{} on {computer}", channel.display_name())
}

static COMPUTER_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The name people gave this computer, as Finder and AirDrop show it. The
/// network hostname can be whatever DHCP or a VPN assigned.
fn computer_name() -> Option<String> {
    COMPUTER_NAME.get().cloned()
}

/// Looks the name up once, off the main thread, ahead of the first pairing.
fn prefetch_computer_name(cx: &App) {
    cx.background_spawn(async {
        #[cfg(target_os = "macos")]
        let name = util::command::new_command("/usr/sbin/scutil")
            .args(["--get", "ComputerName"])
            .output()
            .await
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
        #[cfg(not(target_os = "macos"))]
        let name = host_name();
        if let Some(name) = name.filter(|name| !name.is_empty()) {
            COMPUTER_NAME.set(name).ok();
        }
    })
    .detach();
}

#[cfg(not(target_os = "macos"))]
fn host_name() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer is valid for its full length, and gethostname
        // writes at most that many bytes.
        let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        if result != 0 {
            return None;
        }
        let end = buffer.iter().position(|byte| *byte == 0)?;
        Some(String::from_utf8_lossy(&buffer[..end]).into_owned()).filter(|name| !name.is_empty())
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").ok()
    }
}

fn keychain_url(channel: ReleaseChannel) -> String {
    format!("lathe-companion://{}", channel.dev_name())
}

async fn load_or_create_identity(channel: ReleaseChannel, cx: &mut AsyncApp) -> Result<Identity> {
    let provider = cx.update(|cx| zed_credentials_provider::global(cx));
    let url = keychain_url(channel);
    if let Some((_, bytes)) = provider.read_credentials(&url, cx).await? {
        return Identity::from_bytes(&bytes);
    }
    let identity = Identity::generate();
    // The username must be non-empty for the macOS keychain read to find it.
    provider
        .write_credentials(&url, "desktop", &identity.to_bytes(), cx)
        .await?;
    log::info!("companion: created desktop identity {}", identity.desktop_id());
    Ok(identity)
}

fn load_devices(channel: ReleaseChannel, cx: &App) -> Vec<PairedDevice> {
    KeyValueStore::global(cx)
        .scoped(DEVICES_NAMESPACE)
        .read(channel.dev_name())
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
        .unwrap_or_default()
}

fn save_devices(channel: ReleaseChannel, devices: &[PairedDevice], cx: &App) {
    let Some(json) = serde_json::to_string(devices).log_err() else {
        return;
    };
    let store = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        store
            .scoped(DEVICES_NAMESPACE)
            .write(channel.dev_name().to_string(), json)
            .await
            .log_err();
    })
    .detach();
}


/// Longest title a push carries, in characters. The phone shows one line.
const MAX_PUSH_TITLE_CHARS: usize = 100;

/// Adds a trimmed, length-capped `title` to a push, when there is one.
fn with_title(mut push: Value, title: Option<&str>) -> Value {
    let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) else {
        return push;
    };
    let title = if title.chars().count() > MAX_PUSH_TITLE_CHARS {
        let mut capped: String = title.chars().take(MAX_PUSH_TITLE_CHARS - 1).collect();
        capped.push('\u{2026}');
        capped
    } else {
        title.to_string()
    };
    push["title"] = json!(title);
    push
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crypto_box::{SalsaBox, SecretKey, aead::{Aead as _, OsRng}};
    use ed25519_dalek::SigningKey;
    use futures::channel::mpsc;
    use gpui::TestAppContext;

    pub(crate) struct FakePhone {
        pub signing: SigningKey,
        pub box_secret: SecretKey,
        device_id: String,
    }

    impl FakePhone {
        pub fn new() -> Self {
            let mut id = [0u8; 6];
            crypto::fill_random(&mut id);
            Self {
                signing: SigningKey::generate(&mut OsRng),
                box_secret: SecretKey::generate(&mut OsRng),
                // Shaped like the app's ids: 12 alphanumerics, not tied to a key.
                device_id: id.iter().map(|byte| format!("{byte:02x}")).collect(),
            }
        }

        pub fn device_id(&self) -> String {
            self.device_id.clone()
        }

        /// `token` is the base64url value from the QR link. The app re-encodes
        /// it as standard base64 before sealing.
        fn sealed_request(&self, desktop: &PublicKey, token: &str) -> String {
            use base64::{Engine as _, engine::general_purpose::STANDARD};
            let token = STANDARD.encode(crypto::decode_either(token).unwrap());
            let request = json!({
                "type": "pair_request",
                "token": token,
                "device_id": self.device_id(),
                "device_name": "Test phone",
                "device_box_pk": STANDARD.encode(self.box_secret.public_key().as_bytes()),
                "device_sign_pk": STANDARD.encode(self.signing.verifying_key().as_bytes()),
            });
            STANDARD.encode(desktop.seal(&mut OsRng, request.to_string().as_bytes()).unwrap())
        }

        pub fn open(&self, desktop: &PublicKey, frame: &Value) -> Value {
            use base64::{Engine as _, engine::general_purpose::STANDARD};
            let nonce: [u8; 24] = STANDARD
                .decode(frame["nonce"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap();
            let ciphertext = STANDARD.decode(frame["ciphertext"].as_str().unwrap()).unwrap();
            let plaintext = SalsaBox::new(desktop, &self.box_secret)
                .decrypt(&nonce.into(), ciphertext.as_slice())
                .unwrap();
            serde_json::from_slice(&plaintext).unwrap()
        }
    }

    pub(crate) fn connected_companion(
        cx: &mut TestAppContext,
    ) -> (Entity<Companion>, mpsc::UnboundedReceiver<String>, Arc<Identity>) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.set_global(db::AppDatabase::test_new());
        });
        let identity = Arc::new(Identity::generate());
        let (connection, outgoing) = RelayConnection::for_test();
        let (_events_tx, events) = mpsc::unbounded();
        let companion = cx.new(|cx| {
            let mut companion = Companion::new(ReleaseChannel::Dev, Vec::new());
            companion.identity = Some(identity.clone());
            companion.attach(connection, events, cx);
            companion.handle_relay_event(RelayEvent::Connected, cx);
            companion
        });
        (companion, outgoing, identity)
    }

    pub(crate) fn add_paired(companion: &Entity<Companion>, phone: &FakePhone, cx: &mut TestAppContext) {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        companion.update(cx, |companion, _| {
            companion.devices.push(PairedDevice {
                device_id: phone.device_id(),
                name: "Test phone".into(),
                sign_public_key: STANDARD.encode(phone.signing.verifying_key().as_bytes()),
                box_public_key: STANDARD.encode(phone.box_secret.public_key().as_bytes()),
                paired_at: 0,
                muted_workspaces: Vec::new(),
            });
        });
    }

    /// Delivers a message from the phone the way the relay would.
    pub(crate) fn phone_says(
        companion: &Entity<Companion>,
        phone: &FakePhone,
        desktop: &PublicKey,
        seq: u64,
        message: Value,
        cx: &mut TestAppContext,
    ) {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        use crypto_box::aead::AeadCore as _;
        let nonce = SalsaBox::generate_nonce(&mut OsRng);
        let ciphertext = SalsaBox::new(desktop, &phone.box_secret)
            .encrypt(&nonce, message.to_string().as_bytes())
            .unwrap();
        companion.update(cx, |companion, cx| {
            companion.handle_relay_message(
                json!({
                    "type": "frame",
                    "device_id": phone.device_id(),
                    "seq": seq,
                    "nonce": STANDARD.encode(nonce),
                    "ciphertext": STANDARD.encode(ciphertext),
                }),
                cx,
            )
        });
    }

    pub(crate) fn drain(outgoing: &mut mpsc::UnboundedReceiver<String>) -> Vec<Value> {
        let mut messages = Vec::new();
        while let Ok(message) = outgoing.try_recv() {
            messages.push(serde_json::from_str(&message).unwrap());
        }
        messages
    }

    fn query_param(payload: &str, key: &str) -> String {
        let query = payload.split_once('?').unwrap().1;
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(name, _)| name == key)
            .unwrap()
            .1
            .into_owned()
    }

    #[gpui::test]
    async fn pairs_a_phone_end_to_end(cx: &mut TestAppContext) {
        let (companion, mut outgoing, identity) = connected_companion(cx);
        assert_eq!(drain(&mut outgoing), [json!({ "type": "list_devices" })]);

        companion.update(cx, |companion, cx| companion.start_pairing(cx)).unwrap();
        assert_eq!(
            drain(&mut outgoing),
            [json!({ "type": "pairing_open", "ttl_seconds": 300 })]
        );
        let payload = companion.read_with(cx, |companion, _| match companion.pairing() {
            Some(PairingState::WaitingForScan { qr_payload, .. }) => qr_payload.clone(),
            other => panic!("unexpected pairing state {other:?}"),
        });
        assert!(payload.starts_with("lathe-companion://pair?v=1&"));
        assert_eq!(query_param(&payload, "desktop_id"), identity.desktop_id());

        // The phone only knows what the QR code told it.
        let desktop_box = PublicKey::from_bytes(
            crypto::decode_key(&query_param(&payload, "desktop_pk")).unwrap(),
        );
        let phone = FakePhone::new();
        let sealed = phone.sealed_request(&desktop_box, &query_param(&payload, "token"));
        companion.update(cx, |companion, cx| {
            companion.handle_relay_message(
                json!({ "type": "sealed", "device_id": phone.device_id(), "ciphertext": sealed }),
                cx,
            )
        });
        companion.read_with(cx, |companion, _| match companion.pairing() {
            Some(PairingState::AwaitingApproval(request)) => {
                assert_eq!(request.device_name, "Test phone");
                assert_eq!(request.device_id, phone.device_id());
            }
            other => panic!("unexpected pairing state {other:?}"),
        });

        companion
            .update(cx, |companion, cx| companion.respond_to_pairing(true, cx))
            .unwrap();
        let sent = drain(&mut outgoing);
        assert_eq!(sent[0]["type"], "authorize_device");
        assert_eq!(sent[0]["device_id"], json!(phone.device_id()));
        assert_eq!(sent[1]["device_id"], json!(phone.device_id()));
        assert!(sent[1]["seq"].as_u64().unwrap() >= 1);
        assert_eq!(
            phone.open(&identity.box_public_key(), &sent[1]),
            json!({
                "type": "pair_accepted",
                "desktop_name": desktop_name(ReleaseChannel::Dev),
                "channel": "dev",
            })
        );
        assert_eq!(sent[2], json!({ "type": "pairing_close" }));
        companion.read_with(cx, |companion, _| {
            assert_eq!(companion.devices().len(), 1);
        });
    }

    #[test]
    fn web_links_carry_the_pairing_in_the_fragment() {
        let payload = "lathe-companion://pair?v=1&relay=wss%3A%2F%2Fcompanion.lathe-ide.com&desktop_id=abc&token=t0k&name=Lathe%20Beta";
        assert_eq!(
            pairing_web_link("wss://companion.lathe-ide.com", payload).as_deref(),
            Some("https://companion.lathe-ide.com/pair#v=1&relay=wss%3A%2F%2Fcompanion.lathe-ide.com&desktop_id=abc&token=t0k&name=Lathe%20Beta")
        );
        // A local relay keeps its port and plain http.
        assert_eq!(
            pairing_web_link("ws://localhost:8799/v1/connect?x=1", payload).as_deref(),
            Some("http://localhost:8799/pair#v=1&relay=wss%3A%2F%2Fcompanion.lathe-ide.com&desktop_id=abc&token=t0k&name=Lathe%20Beta")
        );
        assert_eq!(pairing_web_link("https://not-a-relay", payload), None);
        assert_eq!(pairing_web_link("wss://companion.lathe-ide.com", "no query"), None);
    }

    #[gpui::test]
    async fn a_new_code_replaces_the_old_one(cx: &mut TestAppContext) {
        let (companion, mut outgoing, identity) = connected_companion(cx);
        let payload = |companion: &Entity<Companion>, cx: &mut TestAppContext| {
            companion.read_with(cx, |companion, _| match companion.pairing() {
                Some(PairingState::WaitingForScan { qr_payload, .. }) => qr_payload.clone(),
                other => panic!("unexpected pairing state {other:?}"),
            })
        };
        companion.update(cx, |companion, cx| companion.start_pairing(cx)).unwrap();
        let first = payload(&companion, cx);
        companion.update(cx, |companion, cx| companion.start_pairing(cx)).unwrap();
        let second = payload(&companion, cx);
        assert_ne!(query_param(&first, "token"), query_param(&second, "token"));
        // Each code reopens pairing on the relay for its full window.
        let opened = drain(&mut outgoing)
            .into_iter()
            .filter(|message| message["type"] == "pairing_open")
            .count();
        assert_eq!(opened, 2);

        // A phone that scanned the old code is turned away.
        let phone = FakePhone::new();
        let sealed = phone.sealed_request(&identity.box_public_key(), &query_param(&first, "token"));
        companion.update(cx, |companion, cx| {
            companion.handle_relay_message(
                json!({ "type": "sealed", "device_id": phone.device_id(), "ciphertext": sealed }),
                cx,
            )
        });
        assert_eq!(payload(&companion, cx), second);
    }

    #[gpui::test]
    async fn ignores_pairing_requests_with_the_wrong_token(cx: &mut TestAppContext) {
        let (companion, mut outgoing, identity) = connected_companion(cx);
        companion.update(cx, |companion, cx| companion.start_pairing(cx)).unwrap();
        drain(&mut outgoing);
        let phone = FakePhone::new();
        let sealed = phone.sealed_request(&identity.box_public_key(), "Z3Vlc3NlZA");
        companion.update(cx, |companion, cx| {
            companion.handle_relay_message(
                json!({ "type": "sealed", "device_id": phone.device_id(), "ciphertext": sealed }),
                cx,
            )
        });
        companion.read_with(cx, |companion, _| {
            assert!(matches!(
                companion.pairing(),
                Some(PairingState::WaitingForScan { .. })
            ));
        });
    }

    #[gpui::test]
    async fn decrypts_frames_from_paired_phones_only(cx: &mut TestAppContext) {
        let (companion, mut outgoing, identity) = connected_companion(cx);
        let phone = FakePhone::new();
        add_paired(&companion, &phone, cx);
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let received = received.clone();
            cx.subscribe(&companion, move |_, event: &CompanionEvent, _| {
                if let CompanionEvent::PhoneMessage { message, .. } = event {
                    received.lock().unwrap().push(message.clone());
                }
            })
        });

        let encrypt = |message: &Value| {
            use base64::{Engine as _, engine::general_purpose::STANDARD};
            let nonce = SalsaBox::generate_nonce(&mut OsRng);
            let ciphertext = SalsaBox::new(&identity.box_public_key(), &phone.box_secret)
                .encrypt(&nonce, message.to_string().as_bytes())
                .unwrap();
            (STANDARD.encode(nonce), STANDARD.encode(ciphertext))
        };
        use crypto_box::aead::AeadCore as _;
        let (nonce, ciphertext) = encrypt(&json!({ "type": "cancel", "thread_id": "t1" }));
        let frame = |device_id: &str, seq: u64| {
            json!({ "type": "frame", "device_id": device_id, "seq": seq, "nonce": nonce, "ciphertext": ciphertext })
        };
        companion.update(cx, |companion, cx| {
            companion.handle_relay_message(frame(&phone.device_id(), 1), cx);
            companion.handle_relay_message(frame("someone-else", 1), cx);
            // A relay replaying an old frame doesn't get it delivered twice.
            companion.handle_relay_message(frame(&phone.device_id(), 1), cx);
        });
        assert_eq!(
            *received.lock().unwrap(),
            [json!({ "type": "cancel", "thread_id": "t1" })]
        );

        // After the phone reconnects its counter starts over.
        companion.update(cx, |companion, cx| {
            companion.handle_relay_message(
                json!({ "type": "presence", "device_id": phone.device_id(), "online": true }),
                cx,
            );
            companion.handle_relay_message(frame(&phone.device_id(), 1), cx);
        });
        assert_eq!(received.lock().unwrap().len(), 2);

        companion
            .update(cx, |companion, _| {
                companion.send_to_device(&phone.device_id(), &json!({ "type": "hello" }))
            })
            .unwrap();
        let sent = drain(&mut outgoing);
        let frame = sent.last().unwrap();
        assert_eq!(frame["device_id"], json!(phone.device_id()));
        assert_eq!(
            phone.open(&identity.box_public_key(), frame),
            json!({ "type": "hello" })
        );
    }

    #[gpui::test]
    async fn refuses_frames_too_large_for_the_relay(cx: &mut TestAppContext) {
        let (companion, mut outgoing, _) = connected_companion(cx);
        let phone = FakePhone::new();
        add_paired(&companion, &phone, cx);
        drain(&mut outgoing);
        let result = companion.update(cx, |companion, _| {
            companion.send_to_device(
                &phone.device_id(),
                &json!({ "type": "thread_snapshot", "padding": "x".repeat(MAX_FRAME_PLAINTEXT_BYTES) }),
            )
        });
        assert!(result.is_err());
        assert!(drain(&mut outgoing).is_empty());
    }

    #[test]
    fn push_titles_are_trimmed_and_capped() {
        let push = || json!({ "type": "push", "kind": "finished", "thread_id": "t1" });
        assert_eq!(with_title(push(), None), push());
        assert_eq!(with_title(push(), Some("   ")), push());
        assert_eq!(with_title(push(), Some(" Fix login ")), json!({ "type": "push", "kind": "finished", "thread_id": "t1", "title": "Fix login" }));
        let long = "é".repeat(MAX_PUSH_TITLE_CHARS + 5);
        let capped = with_title(push(), Some(&long))["title"].as_str().unwrap().to_string();
        assert_eq!(capped.chars().count(), MAX_PUSH_TITLE_CHARS);
        assert!(capped.ends_with('\u{2026}'));
    }

    #[gpui::test]
    async fn leaves_phones_that_muted_a_workspace_out_of_its_pushes(cx: &mut TestAppContext) {
        let (companion, mut outgoing, _) = connected_companion(cx);
        let quiet = FakePhone::new();
        let loud = FakePhone::new();
        add_paired(&companion, &quiet, cx);
        add_paired(&companion, &loud, cx);
        drain(&mut outgoing);

        // Nobody muted it: one push for every phone.
        companion.read_with(cx, |companion, _| companion.push("finished", "t1", None, Some("ws")));
        assert_eq!(drain(&mut outgoing), [json!({ "type": "push", "kind": "finished", "thread_id": "t1" })]);

        // One phone muted it: the push is addressed to the other only.
        companion.update(cx, |companion, cx| companion.set_muted_workspaces(&quiet.device_id(), vec!["ws".into()], cx));
        companion.read_with(cx, |companion, _| {
            companion.push("finished", "t1", None, Some("ws"));
            companion.push_terminal("term", None, Some("ws"));
            companion.push("finished", "t2", None, Some("other"));
        });
        assert_eq!(
            drain(&mut outgoing),
            [
                json!({ "type": "push", "kind": "finished", "thread_id": "t1", "device_id": loud.device_id() }),
                json!({ "type": "push", "kind": "terminal_input", "terminal_id": "term", "device_id": loud.device_id() }),
                json!({ "type": "push", "kind": "finished", "thread_id": "t2" }),
            ]
        );

        // Both muted it: nothing goes out.
        companion.update(cx, |companion, cx| companion.set_muted_workspaces(&loud.device_id(), vec!["ws".into()], cx));
        companion.read_with(cx, |companion, _| companion.push("finished", "t1", None, Some("ws")));
        assert!(drain(&mut outgoing).is_empty());
    }

    #[gpui::test]
    async fn reconciles_devices_with_the_relay(cx: &mut TestAppContext) {
        let (companion, mut outgoing, _) = connected_companion(cx);
        let phone = FakePhone::new();
        companion.update(cx, |companion, cx| {
            companion.devices.push(PairedDevice {
                device_id: phone.device_id(),
                name: "Kept".into(),
                sign_public_key: "c2lnbg==".into(),
                box_public_key: "Ym94".into(),
                paired_at: 0,
                muted_workspaces: Vec::new(),
            });
            companion.handle_relay_message(
                json!({ "type": "devices", "list": [
                    { "device_id": "stranger", "name": "Unknown", "online": true, "has_push": false }
                ]}),
                cx,
            );
        });
        let sent = drain(&mut outgoing);
        assert!(sent.contains(&json!({ "type": "revoke_device", "device_id": "stranger" })));
        assert!(sent.iter().any(|message| message["type"] == "authorize_device"
            && message["device_id"] == json!(phone.device_id())));
    }
}
