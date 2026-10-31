//! Connects live agent threads to paired phones: sends the thread list and
//! transcripts, applies what phones ask for, and triggers pushes.

use crate::{
    Companion, CompanionEvent, CompanionSettings, CompanionThreads, CompanionThreadsEvent,
    crypto::encode_url_safe,
    entries,
    protocol::{
        AgentSummary, ConfigCategory, ConfigChoice, ConfigKind, ConfigOption, DeltaOp, DesktopMessage, Entry, ErrorBody, ErrorCode, FEATURE_PULL_REQUESTS, PROTOCOL_VERSION, PhoneEnvelope,
        PhoneMessage, ThreadStatus, ThreadSummary, WorkspaceSummary,
    },
};
use acp_thread::{AcpThread, AcpThreadEvent, ElicitationEntryId, SelectedPermissionOutcome};
use agent_client_protocol::schema::v2 as acp;
use agent_ui::{
    Agent, AgentInitialContent, AgentPanel, AgentThreadSource, CreateThreadOptions,
    thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore},
};
use collections::{BTreeSet, HashMap, HashSet};
use gpui::{
    App, AppContext as _, Context, Entity, Global, SharedString, Subscription, Task,
    WeakEntity,
};
use release_channel::ReleaseChannel;
use settings::Settings as _;
use sha2::{Digest as _, Sha256};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use util::{ResultExt as _, path_list::PathList};
use workspace::MultiWorkspace;

/// Entries a snapshot or `load_older` page carries. Older ones load on demand.
const PAGE_SIZE: usize = 50;
/// Room for a snapshot's own fields around its entries.
const PAGE_OVERHEAD_BYTES: usize = 1024;
/// The most threads listed, newest first.
const MAX_LISTED_THREADS: usize = 100;
/// Streaming text updates an entry many times a second; batching them keeps the
/// relay traffic to about ten frames a second per thread.
const ENTRY_FLUSH_DELAY: Duration = Duration::from_millis(100);
const LIST_FLUSH_DELAY: Duration = Duration::from_millis(500);
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// External agents launch a process before their first turn.
const NEW_THREAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Message ids remembered per phone. A relay that replays an old frame with a
/// higher `seq` still can't make Lathe act on it twice.
const REMEMBERED_MESSAGES: usize = 512;
/// The app's limit for thread names.
const MAX_TITLE_CHARS: usize = 200;

struct GlobalBridge(#[allow(dead_code)] Entity<Bridge>);

impl Global for GlobalBridge {}

pub(crate) fn init(cx: &mut App) {
    let (Some(companion), Some(threads)) = (Companion::global(cx), CompanionThreads::global(cx))
    else {
        return;
    };
    let bridge = cx.new(|cx| Bridge::new(companion, threads, cx));
    cx.set_global(GlobalBridge(bridge));
}

pub(crate) struct Bridge {
    companion: Entity<Companion>,
    threads: Entity<CompanionThreads>,
    watched: HashMap<acp::SessionId, Subscription>,
    streams: HashMap<acp::SessionId, Stream>,
    statuses: HashMap<acp::SessionId, ThreadStatus>,
    seen_messages: HashMap<String, VecDeque<String>>,
    pending_loads: HashMap<acp::SessionId, Vec<PendingSubscribe>>,
    pending_new_threads: HashMap<ThreadId, PendingNewThread>,
    list_flush: Option<Task<()>>,
    keep_awake: Option<keep_awake::Assertion>,
    _subscriptions: Vec<Subscription>,
}

/// A thread started from a phone, acked once it has a session the phone can
/// subscribe to.
struct PendingNewThread {
    device_id: String,
    client_msg_id: String,
    _timeout: Task<()>,
}

struct PendingSubscribe {
    device_id: String,
    client_msg_id: String,
    thread_id: String,
    _timeout: Task<()>,
}

/// A thread at least one phone is following.
struct Stream {
    thread_id: String,
    seq: u64,
    sent: Vec<Entry>,
    dirty: BTreeSet<usize>,
    removed_from: Option<usize>,
    subscribers: HashSet<String>,
    flush: Option<Task<()>>,
    /// Both a rename and the agent's own title update report the same title.
    last_title: Option<String>,
    /// A change reports its options twice: in the reply to the change and as
    /// the agent's own update.
    last_config: Option<Vec<ConfigOption>>,
}

impl Bridge {
    fn new(
        companion: Entity<Companion>,
        threads: Entity<CompanionThreads>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(&companion, |this, _, event, cx| {
                this.handle_companion_event(event, cx)
            }),
            cx.subscribe(&threads, |this, _, event, cx| this.handle_threads_event(event, cx)),
        ];
        let mut subscriptions = subscriptions;
        if let Some(store) = ThreadMetadataStore::try_global(cx) {
            subscriptions.push(cx.observe(&store, |this, _, cx| this.complete_new_threads(cx)));
        }
        let mut this = Self {
            companion,
            threads,
            watched: HashMap::default(),
            streams: HashMap::default(),
            statuses: HashMap::default(),
            seen_messages: HashMap::default(),
            pending_loads: HashMap::default(),
            pending_new_threads: HashMap::default(),
            list_flush: None,
            keep_awake: None,
            _subscriptions: subscriptions,
        };
        let live = this
            .threads
            .read(cx)
            .top_level_threads()
            .map(|(session_id, _, _)| session_id.clone())
            .collect::<Vec<_>>();
        for session_id in live {
            this.watch(&session_id, cx);
        }
        this
    }

    fn send(&self, device_id: &str, message: &DesktopMessage, cx: &mut Context<Self>) {
        let Some(value) = serde_json::to_value(message).log_err() else {
            return;
        };
        self.companion
            .update(cx, |companion, _| companion.send_to_device(device_id, &value))
            .log_err();
    }

    fn broadcast(&self, message: &DesktopMessage, cx: &mut Context<Self>) {
        let devices = self.online_devices(cx);
        for device_id in devices {
            self.send(&device_id, message, cx);
        }
    }

    fn online_devices(&self, cx: &App) -> Vec<String> {
        self.companion.read(cx).online_devices().cloned().collect()
    }

    fn ack(&self, device_id: &str, client_msg_id: &str, result: Result<(), ErrorBody>, cx: &mut Context<Self>) {
        self.ack_with_thread(device_id, client_msg_id, result.map(|()| None), cx);
    }

    fn ack_with_thread(
        &self,
        device_id: &str,
        client_msg_id: &str,
        result: Result<Option<String>, ErrorBody>,
        cx: &mut Context<Self>,
    ) {
        let (ok, error, thread_id) = match result {
            Ok(thread_id) => (true, None, thread_id),
            Err(error) => (false, Some(error), None),
        };
        self.send(
            device_id,
            &DesktopMessage::Ack {
                client_msg_id: client_msg_id.to_string(),
                ok,
                error,
                thread_id,
            },
            cx,
        );
    }

    fn handle_companion_event(&mut self, event: &CompanionEvent, cx: &mut Context<Self>) {
        match event {
            CompanionEvent::DeviceOnline(device_id) => self.greet(device_id, cx),
            CompanionEvent::DeviceOffline(device_id) | CompanionEvent::DeviceRemoved(device_id) => {
                self.forget_device(device_id)
            }
            CompanionEvent::PhoneMessage { device_id, message } => {
                self.handle_phone_message(device_id, message, cx)
            }
            CompanionEvent::Changed => {}
        }
    }

    fn greet(&mut self, device_id: &str, cx: &mut Context<Self>) {
        let channel = ReleaseChannel::try_global(cx).unwrap_or(ReleaseChannel::Dev);
        let hello = DesktopMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            lathe_version: release_channel::AppVersion::global(cx).to_string(),
            channel: channel.dev_name().to_string(),
            desktop_name: crate::companion::desktop_name(channel),
            features: vec![FEATURE_PULL_REQUESTS.to_string()],
        };
        self.send(device_id, &hello, cx);
        self.send_lists(device_id, cx);
        self.send_pending_prompts(device_id, None, cx);
    }

    fn forget_device(&mut self, device_id: &str) {
        self.seen_messages.remove(device_id);
        self.streams.retain(|_, stream| {
            stream.subscribers.remove(device_id);
            !stream.subscribers.is_empty()
        });
        // Pending loads stay: a phone that drops and reconnects while its
        // thread loads still gets the answer, and the timeout clears the rest.
    }

    fn send_lists(&self, device_id: &str, cx: &mut Context<Self>) {
        let threads = self.thread_list(cx);
        let workspaces = workspace_list(&threads, cx);
        self.send(device_id, &DesktopMessage::Workspaces { list: workspaces }, cx);
        self.send(device_id, &DesktopMessage::Threads { list: threads.into_iter().map(|(summary, _)| summary).collect() }, cx);
    }

    fn schedule_list_flush(&mut self, cx: &mut Context<Self>) {
        if self.list_flush.is_some() {
            return;
        }
        self.list_flush = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIST_FLUSH_DELAY).await;
            this.update(cx, |this, cx| {
                this.list_flush = None;
                for device_id in this.online_devices(cx) {
                    this.send_lists(&device_id, cx);
                }
            })
            .log_err();
        }));
    }

    /// Threads the phone can list, newest first, with the folders each belongs
    /// to.
    fn thread_list(&self, cx: &App) -> Vec<(ThreadSummary, PathList)> {
        let Some(store) = ThreadMetadataStore::try_global(cx) else {
            return Vec::new();
        };
        let threads = self.threads.read(cx);
        let mut metadata = store
            .read(cx)
            .entries()
            .filter(|metadata| !metadata.archived && !metadata.is_draft())
            .collect::<Vec<_>>();
        metadata.sort_by_key(|metadata| std::cmp::Reverse(metadata.updated_at));
        metadata
            .into_iter()
            .take(MAX_LISTED_THREADS)
            .map(|metadata| {
                let live_status = metadata
                    .session_id
                    .as_ref()
                    .and_then(|session_id| threads.status(session_id));
                let status = live_status.unwrap_or(ThreadStatus::Idle);
                let summary = ThreadSummary {
                    thread_id: metadata.thread_id.to_key_string(),
                    session_id: metadata.session_id.as_ref().map(|id| id.0.to_string()),
                    agent_id: metadata.agent_id.0.to_string(),
                    title: metadata
                        .title_override
                        .clone()
                        .or_else(|| metadata.title.clone())
                        .map(|title| title.to_string())
                        .unwrap_or_else(|| "New Thread".into()),
                    workspace_id: workspace_id(metadata.folder_paths()),
                    updated_at: metadata.updated_at.to_rfc3339(),
                    status,
                    needs_attention: status.needs_attention(),
                    is_loaded: live_status.is_some(),
                };
                (summary, metadata.folder_paths().clone())
            })
            .collect()
    }

    fn thread_metadata(&self, thread_id: &str, cx: &App) -> Option<ThreadMetadata> {
        let store = ThreadMetadataStore::try_global(cx)?;
        store
            .read(cx)
            .entries()
            .find(|metadata| metadata.thread_id.to_key_string() == thread_id)
            .cloned()
    }

    fn live_thread(&self, thread_id: &str, cx: &App) -> Option<(acp::SessionId, Entity<AcpThread>)> {
        let session_id = self.thread_metadata(thread_id, cx)?.session_id?;
        let thread = self.threads.read(cx).thread(&session_id)?;
        Some((session_id, thread))
    }

    fn handle_threads_event(&mut self, event: &CompanionThreadsEvent, cx: &mut Context<Self>) {
        match event {
            CompanionThreadsEvent::ThreadAdded(session_id) => {
                self.watch(session_id, cx);
                self.complete_pending_loads(session_id, cx);
                self.schedule_list_flush(cx);
            }
            CompanionThreadsEvent::ThreadRemoved(session_id) => {
                self.watched.remove(session_id);
                self.streams.remove(session_id);
                self.statuses.remove(session_id);
                self.update_keep_awake(cx);
                self.schedule_list_flush(cx);
            }
            CompanionThreadsEvent::StatusChanged { session_id, status } => {
                self.status_changed(session_id, *status, cx)
            }
            CompanionThreadsEvent::TitleChanged(session_id) => {
                if let Some(title) = self.display_title(session_id, cx) {
                    self.send_title(session_id, title, cx);
                }
                self.schedule_list_flush(cx);
            }
        }
    }

    fn watch(&mut self, session_id: &acp::SessionId, cx: &mut Context<Self>) {
        let Some(thread) = self.threads.read(cx).thread(session_id) else {
            return;
        };
        if thread.read(cx).parent_session_id().is_some() || self.watched.contains_key(session_id) {
            return;
        }
        let subscription = cx.subscribe(&thread, {
            let session_id = session_id.clone();
            move |this, thread, event, cx| this.handle_thread_event(&session_id, &thread, event, cx)
        });
        self.watched.insert(session_id.clone(), subscription);
        if let Some(status) = self.threads.read(cx).status(session_id) {
            self.statuses.insert(session_id.clone(), status);
        }
    }

    fn status_changed(&mut self, session_id: &acp::SessionId, status: ThreadStatus, cx: &mut Context<Self>) {
        if !self.watched.contains_key(session_id) {
            return;
        }
        let previous = self.statuses.insert(session_id.clone(), status);
        self.send_delta(session_id, DeltaOp::Status { status }, cx);
        self.schedule_list_flush(cx);
        self.update_keep_awake(cx);

        let kind = match status {
            ThreadStatus::WaitingAuthorization => "needs_authorization",
            ThreadStatus::WaitingInput => "needs_input",
            ThreadStatus::Error => "error",
            ThreadStatus::Idle if previous == Some(ThreadStatus::Running) => "finished",
            ThreadStatus::Idle | ThreadStatus::Running => return,
        };
        // Someone looking at the thread on their phone already sees this.
        let watched_on_phone = self
            .streams
            .get(session_id)
            .is_some_and(|stream| !stream.subscribers.is_empty());
        if watched_on_phone {
            return;
        }
        if let Some(thread_id) = CompanionThreads::thread_id(session_id, cx) {
            self.companion.read(cx).push(kind, &thread_id);
        }
    }

    fn handle_thread_event(
        &mut self,
        session_id: &acp::SessionId,
        thread: &Entity<AcpThread>,
        event: &AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            AcpThreadEvent::NewEntry => {
                let index = thread.read(cx).entries().len().saturating_sub(1);
                self.mark_dirty(session_id, |stream| {
                    stream.dirty.insert(index);
                }, cx);
            }
            AcpThreadEvent::EntryUpdated(index) => {
                let index = *index;
                self.mark_dirty(session_id, |stream| {
                    stream.dirty.insert(index);
                }, cx);
            }
            AcpThreadEvent::EntriesRemoved(range) => {
                let start = range.start;
                self.mark_dirty(session_id, |stream| {
                    stream.removed_from = Some(stream.removed_from.map_or(start, |from| from.min(start)));
                }, cx);
            }
            AcpThreadEvent::ToolAuthorizationRequested(request_id) => {
                let Some(thread_id) = CompanionThreads::thread_id(session_id, cx) else {
                    return;
                };
                let thread = thread.read(cx);
                if let Some(request) = thread.permission_request(*request_id) {
                    let message = entries::tool_authorization(&thread_id, thread, request, cx);
                    self.broadcast(&message, cx);
                }
            }
            AcpThreadEvent::ToolAuthorizationReceived(request_id) => {
                let Some(thread_id) = CompanionThreads::thread_id(session_id, cx) else {
                    return;
                };
                let request_id = serde_json::to_value(request_id)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_default();
                self.broadcast(&DesktopMessage::ToolAuthorizationResolved { thread_id, request_id }, cx);
            }
            AcpThreadEvent::ElicitationRequested(id) => {
                let Some(thread_id) = CompanionThreads::thread_id(session_id, cx) else {
                    return;
                };
                let message = thread
                    .read(cx)
                    .elicitation(id)
                    .and_then(|(_, elicitation)| entries::elicitation_prompt(&thread_id, elicitation));
                if let Some(message) = message {
                    self.broadcast(&message, cx);
                }
            }
            AcpThreadEvent::ConfigOptionsUpdated(options) => {
                let options = convert_config_options(options);
                self.send_config(session_id, options, cx);
            }
            AcpThreadEvent::ElicitationResponded(id) => {
                let Some(thread_id) = CompanionThreads::thread_id(session_id, cx) else {
                    return;
                };
                self.broadcast(
                    &DesktopMessage::ElicitationResolved {
                        thread_id,
                        elicitation_id: id.0.to_string(),
                    },
                    cx,
                );
            }
            _ => {}
        }
    }

    fn mark_dirty(&mut self, session_id: &acp::SessionId, mark: impl FnOnce(&mut Stream), cx: &mut Context<Self>) {
        let Some(stream) = self.streams.get_mut(session_id) else {
            return;
        };
        mark(stream);
        if stream.flush.is_some() {
            return;
        }
        let session_id = session_id.clone();
        stream.flush = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ENTRY_FLUSH_DELAY).await;
            this.update(cx, |this, cx| this.flush_entries(&session_id, cx)).log_err();
        }));
    }

    fn flush_entries(&mut self, session_id: &acp::SessionId, cx: &mut Context<Self>) {
        let Some(thread) = self.threads.read(cx).thread(session_id) else {
            return;
        };
        let Some(stream) = self.streams.get_mut(session_id) else {
            return;
        };
        stream.flush = None;
        let thread = thread.read(cx);
        let current = thread.entries();
        let mut ops = Vec::new();

        if let Some(from) = stream.removed_from.take()
            && from < stream.sent.len()
        {
            ops.push(DeltaOp::RemoveEntries {
                start: from,
                end: stream.sent.len(),
            });
            stream.sent.truncate(from);
        }
        for index in std::mem::take(&mut stream.dirty) {
            if index >= stream.sent.len() || index >= current.len() {
                continue;
            }
            let entry = entries::entry(thread, &current[index], cx);
            if entry != stream.sent[index] {
                stream.sent[index] = entry.clone();
                ops.push(DeltaOp::UpdateEntry { index, entry });
            }
        }
        for index in stream.sent.len()..current.len() {
            let entry = entries::entry(thread, &current[index], cx);
            stream.sent.push(entry.clone());
            ops.push(DeltaOp::NewEntry { entry });
        }

        for op in ops {
            self.send_delta(session_id, op, cx);
        }
    }

    fn send_config(&mut self, session_id: &acp::SessionId, options: Vec<ConfigOption>, cx: &mut Context<Self>) {
        let Some(stream) = self.streams.get_mut(session_id) else {
            return;
        };
        if stream.last_config.as_ref() == Some(&options) {
            return;
        }
        stream.last_config = Some(options.clone());
        let message = DesktopMessage::ThreadConfig {
            thread_id: stream.thread_id.clone(),
            options,
        };
        let subscribers = stream.subscribers.iter().cloned().collect::<Vec<_>>();
        for device_id in subscribers {
            self.send(&device_id, &message, cx);
        }
    }

    fn send_title(&mut self, session_id: &acp::SessionId, title: String, cx: &mut Context<Self>) {
        let Some(stream) = self.streams.get_mut(session_id) else {
            return;
        };
        if stream.last_title.as_ref() == Some(&title) {
            return;
        }
        stream.last_title = Some(title.clone());
        self.send_delta(session_id, DeltaOp::Title { title }, cx);
    }

    fn send_delta(&mut self, session_id: &acp::SessionId, op: DeltaOp, cx: &mut Context<Self>) {
        let Some(stream) = self.streams.get_mut(session_id) else {
            return;
        };
        stream.seq += 1;
        let message = DesktopMessage::ThreadDelta {
            thread_id: stream.thread_id.clone(),
            seq: stream.seq,
            op,
        };
        let subscribers = stream.subscribers.iter().cloned().collect::<Vec<_>>();
        for device_id in subscribers {
            self.send(&device_id, &message, cx);
        }
    }

    fn handle_phone_message(&mut self, device_id: &str, message: &serde_json::Value, cx: &mut Context<Self>) {
        let envelope = match serde_json::from_value::<PhoneEnvelope>(message.clone()) {
            Ok(envelope) => envelope,
            Err(error) => {
                log::warn!("companion: couldn't parse a phone message: {error}");
                if let Some(client_msg_id) = message["client_msg_id"].as_str() {
                    self.ack(
                        device_id,
                        client_msg_id,
                        Err(ErrorBody::new(ErrorCode::BadMessage, "Lathe doesn't understand this request. Update Lathe or the app.")),
                        cx,
                    );
                }
                return;
            }
        };

        let seen = self.seen_messages.entry(device_id.to_string()).or_default();
        if seen.contains(&envelope.client_msg_id) {
            log::warn!("companion: ignored a repeated message {}", envelope.client_msg_id);
            return;
        }
        seen.push_back(envelope.client_msg_id.clone());
        if seen.len() > REMEMBERED_MESSAGES {
            seen.pop_front();
        }

        let client_msg_id = envelope.client_msg_id;
        let result = match envelope.message {
            PhoneMessage::Subscribe { thread_id, .. } => {
                // A deferred subscribe acks once the thread has loaded.
                match self.subscribe(device_id, &client_msg_id, &thread_id, cx) {
                    Ok(true) => return,
                    Ok(false) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            PhoneMessage::Unsubscribe { thread_id } => {
                self.streams.retain(|_, stream| {
                    if stream.thread_id == thread_id {
                        stream.subscribers.remove(device_id);
                    }
                    !stream.subscribers.is_empty()
                });
                Ok(())
            }
            PhoneMessage::Refresh {} => {
                self.send_lists(device_id, cx);
                self.send_pending_prompts(device_id, None, cx);
                Ok(())
            }
            PhoneMessage::LoadOlder { thread_id, before_index } => {
                self.load_older(device_id, &thread_id, before_index, cx)
            }
            PhoneMessage::SendMessage { thread_id, text } => self.send_message(&thread_id, text, cx),
            PhoneMessage::Authorize { thread_id, request_id, option_id } => {
                self.authorize(&thread_id, &request_id, &option_id, cx)
            }
            PhoneMessage::RespondElicitation { thread_id, elicitation_id, value } => {
                self.respond_elicitation(&thread_id, &elicitation_id, value, cx)
            }
            PhoneMessage::Cancel { thread_id } => match self.live_thread(&thread_id, cx) {
                Some((_, thread)) => {
                    thread.update(cx, |thread, cx| thread.cancel(cx)).detach();
                    Ok(())
                }
                None => Err(not_loaded()),
            },
            PhoneMessage::RenameThread { thread_id, title } => self.rename(&thread_id, &title, cx),
            PhoneMessage::SetConfigOption { thread_id, config_id, value } => {
                match self.set_config_option(device_id, &client_msg_id, &thread_id, &config_id, &value, cx) {
                    // Acked once the agent has applied it.
                    Ok(()) => return,
                    Err(error) => Err(error),
                }
            }
            PhoneMessage::NewThread { workspace_id, agent_id, text } => {
                match self.new_thread(device_id, &client_msg_id, &workspace_id, &agent_id, text, cx) {
                    // Acked once the thread has started.
                    Ok(()) => return,
                    Err(error) => Err(error),
                }
            }
            message @ (PhoneMessage::ListPullRequests { .. }
            | PhoneMessage::GetPullRequest { .. }
            | PhoneMessage::GetPullRequestDiff { .. }
            | PhoneMessage::SubmitReview { .. }
            | PhoneMessage::ReplyReviewComment { .. }
            | PhoneMessage::CreateReviewComment { .. }) => {
                // Acked once the host has answered, after any replies.
                let task = crate::pull_requests::handle(message, cx);
                let device_id = device_id.to_string();
                cx.spawn(async move |this, cx| {
                    let result = task.await;
                    this.update(cx, |this, cx| {
                        let result = result.map(|messages| {
                            for message in &messages {
                                this.send(&device_id, message, cx);
                            }
                        });
                        this.ack(&device_id, &client_msg_id, result, cx);
                    })
                    .log_err();
                })
                .detach();
                return;
            }
            PhoneMessage::Unpair {} => {
                self.ack(device_id, &client_msg_id, Ok(()), cx);
                let device_id = device_id.to_string();
                self.companion.update(cx, |companion, cx| companion.revoke(&device_id, cx));
                return;
            }
        };
        self.ack(device_id, &client_msg_id, result, cx);
    }

    /// Returns `Ok(true)` when the thread is loading and the ack will follow.
    fn subscribe(
        &mut self,
        device_id: &str,
        client_msg_id: &str,
        thread_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, ErrorBody> {
        let metadata = self
            .thread_metadata(thread_id, cx)
            .ok_or_else(|| ErrorBody::new(ErrorCode::ThreadNotFound, "This thread no longer exists."))?;
        let session_id = metadata
            .session_id
            .clone()
            .ok_or_else(|| ErrorBody::new(ErrorCode::ThreadNotFound, "This thread hasn't started yet."))?;

        if self.threads.read(cx).thread(&session_id).is_some() {
            self.start_stream(device_id, &session_id, thread_id, cx);
            return Ok(false);
        }

        log::info!("companion: phone {device_id} opened thread {thread_id}, loading it in Lathe");
        if !load_in_background(&metadata, cx) {
            log::info!("companion: no open window shows the project of thread {thread_id}");
            return Err(ErrorBody::new(
                ErrorCode::WorkspaceNotOpen,
                "Open this project in Lathe to see the thread.",
            ));
        }
        let timeout = cx.spawn({
            let session_id = session_id.clone();
            let client_msg_id = client_msg_id.to_string();
            async move |this, cx| {
                cx.background_executor().timer(LOAD_TIMEOUT).await;
                this.update(cx, |this, cx| this.fail_pending_load(&session_id, &client_msg_id, cx))
                    .log_err();
            }
        });
        self.pending_loads.entry(session_id).or_default().push(PendingSubscribe {
            device_id: device_id.to_string(),
            client_msg_id: client_msg_id.to_string(),
            thread_id: thread_id.to_string(),
            _timeout: timeout,
        });
        Ok(true)
    }

    fn complete_pending_loads(&mut self, session_id: &acp::SessionId, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_loads.remove(session_id) else {
            return;
        };
        log::info!("companion: thread for session {session_id} loaded for {} request(s)", pending.len());
        for pending in pending {
            self.start_stream(&pending.device_id, session_id, &pending.thread_id, cx);
            self.ack(&pending.device_id, &pending.client_msg_id, Ok(()), cx);
        }
    }

    fn fail_pending_load(&mut self, session_id: &acp::SessionId, client_msg_id: &str, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_loads.get_mut(session_id) else {
            return;
        };
        let Some(index) = pending.iter().position(|pending| pending.client_msg_id == client_msg_id) else {
            return;
        };
        let failed = pending.remove(index);
        log::warn!("companion: thread {} didn't load within {}s", failed.thread_id, LOAD_TIMEOUT.as_secs());
        if pending.is_empty() {
            self.pending_loads.remove(session_id);
        }
        self.ack(
            &failed.device_id,
            &failed.client_msg_id,
            Err(ErrorBody::new(ErrorCode::ThreadNotFound, "Lathe couldn't open this thread.")),
            cx,
        );
    }

    fn start_stream(&mut self, device_id: &str, session_id: &acp::SessionId, thread_id: &str, cx: &mut Context<Self>) {
        let Some(thread) = self.threads.read(cx).thread(session_id) else {
            return;
        };
        self.watch(session_id, cx);
        if !self.streams.contains_key(session_id) {
            let last_title = self.display_title(session_id, cx);
            let sent = {
                let thread = thread.read(cx);
                thread
                    .entries()
                    .iter()
                    .map(|entry| entries::entry(thread, entry, cx))
                    .collect()
            };
            self.streams.insert(
                session_id.clone(),
                Stream {
                    thread_id: thread_id.to_string(),
                    seq: 0,
                    sent,
                    dirty: BTreeSet::default(),
                    removed_from: None,
                    subscribers: HashSet::default(),
                    flush: None,
                    last_title,
                    last_config: None,
                },
            );
        } else {
            // Bring the cache up to date so this phone's snapshot matches the
            // deltas the others are about to get.
            self.flush_entries(session_id, cx);
        }
        let Some(stream) = self.streams.get_mut(session_id) else {
            return;
        };
        stream.subscribers.insert(device_id.to_string());
        let start_index = page_start(&stream.sent, stream.sent.len());
        let config_options = phone_config_options(thread.read(cx), cx);
        stream.last_config = config_options.clone();
        let snapshot = DesktopMessage::ThreadSnapshot {
            thread_id: thread_id.to_string(),
            seq: stream.seq,
            start_index,
            entries: stream.sent[start_index..].to_vec(),
            has_more_before: start_index > 0,
            config_options,
        };
        self.send(device_id, &snapshot, cx);
        self.send_pending_prompts(device_id, Some(session_id), cx);
    }

    fn load_older(&mut self, device_id: &str, thread_id: &str, before_index: usize, cx: &mut Context<Self>) -> Result<(), ErrorBody> {
        let stream = self
            .streams
            .values()
            .find(|stream| stream.thread_id == thread_id && stream.subscribers.contains(device_id))
            .ok_or_else(|| ErrorBody::new(ErrorCode::ThreadNotFound, "Open the thread first."))?;
        let end = before_index.min(stream.sent.len());
        let start_index = page_start(&stream.sent, end);
        let message = DesktopMessage::ThreadOlder {
            thread_id: thread_id.to_string(),
            start_index,
            entries: stream.sent[start_index..end].to_vec(),
            has_more_before: start_index > 0,
        };
        self.send(device_id, &message, cx);
        Ok(())
    }

    fn send_message(&mut self, thread_id: &str, text: String, cx: &mut Context<Self>) -> Result<(), ErrorBody> {
        let (_, thread) = self.live_thread(thread_id, cx).ok_or_else(not_loaded)?;
        let submission = thread.update(cx, |thread, cx| thread.send(vec![text.into()], cx));
        cx.spawn(async move |_, _| {
            submission.response.await.log_err();
        })
        .detach();
        Ok(())
    }

    fn authorize(&mut self, thread_id: &str, request_id: &str, option_id: &str, cx: &mut Context<Self>) -> Result<(), ErrorBody> {
        let (_, thread) = self.live_thread(thread_id, cx).ok_or_else(not_loaded)?;
        let not_found = || ErrorBody::new(ErrorCode::RequestNotFound, "This request was already answered.");
        let (id, kind) = {
            let thread = thread.read(cx);
            let request = thread
                .pending_permission_requests()
                .find(|request| entries::request_id(request) == request_id)
                .ok_or_else(not_found)?;
            let option = entries::permission_options(request)
                .into_iter()
                .find(|option| option.option_id == option_id)
                .ok_or_else(not_found)?;
            (request.id, option.kind)
        };
        // Standing rules change what agents may do without asking, so they're
        // only set at the desk.
        let kind = match kind {
            crate::protocol::PermissionOptionKind::AllowOnce => acp::PermissionOptionKind::AllowOnce,
            crate::protocol::PermissionOptionKind::RejectOnce => acp::PermissionOptionKind::RejectOnce,
            crate::protocol::PermissionOptionKind::AllowAlways
            | crate::protocol::PermissionOptionKind::RejectAlways => {
                return Err(ErrorBody::new(
                    ErrorCode::Unsupported,
                    "\"Always\" choices can only be made in Lathe.",
                ));
            }
        };
        let outcome = SelectedPermissionOutcome::new(acp::PermissionOptionId::new(option_id.to_string()), kind);
        thread.update(cx, |thread, cx| thread.authorize_permission_request(id, outcome, cx));
        Ok(())
    }

    /// Changes the model or effort of one thread. Unlike the desk's
    /// selector, this doesn't make the value the default for new threads.
    fn set_config_option(
        &mut self,
        device_id: &str,
        client_msg_id: &str,
        thread_id: &str,
        config_id: &str,
        value: &serde_json::Value,
        cx: &mut Context<Self>,
    ) -> Result<(), ErrorBody> {
        let (session_id, thread) = self.live_thread(thread_id, cx).ok_or_else(not_loaded)?;
        let unavailable = || ErrorBody::new(ErrorCode::BadMessage, "That setting can't be changed from the phone.");
        let provider = {
            let thread = thread.read(cx);
            thread.connection().session_config_options(thread.session_id(), cx)
        }
        .ok_or_else(unavailable)?;
        let option = convert_config_options(&provider.config_options())
            .into_iter()
            .find(|option| option.config_id == config_id)
            .ok_or_else(unavailable)?;
        let value = config_value(&option, value)?;
        log::info!("companion: phone {device_id} set {config_id} on thread {thread_id}");
        let task = provider.set_config_option(acp::SessionConfigId::new(config_id.to_string()), value, cx);
        let device_id = device_id.to_string();
        let client_msg_id = client_msg_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(options) => {
                    this.send_config(&session_id, convert_config_options(&options), cx);
                    this.ack(&device_id, &client_msg_id, Ok(()), cx);
                }
                Err(error) => {
                    log::warn!("companion: the agent refused a setting change: {error:#}");
                    this.ack(
                        &device_id,
                        &client_msg_id,
                        Err(ErrorBody::new(ErrorCode::Internal, format!("The agent didn't accept the change: {error}"))),
                        cx,
                    );
                }
            })
            .log_err();
        })
        .detach();
        Ok(())
    }

    fn new_thread(
        &mut self,
        device_id: &str,
        client_msg_id: &str,
        workspace_id: &str,
        agent_id: &str,
        text: String,
        cx: &mut Context<Self>,
    ) -> Result<(), ErrorBody> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err(ErrorBody::new(ErrorCode::BadMessage, "Write a first message for the thread."));
        }
        let (_, workspace, window) = open_workspace_paths(cx)
            .into_iter()
            .find(|(paths, _, _)| self::workspace_id(paths) == workspace_id)
            .ok_or_else(|| ErrorBody::new(ErrorCode::WorkspaceNotOpen, "Open this project in Lathe to start a thread."))?;
        let created = window
            .update(cx, |_, window, cx| -> Result<ThreadId, ErrorBody> {
                let workspace = workspace
                    .upgrade()
                    .ok_or_else(|| ErrorBody::new(ErrorCode::WorkspaceNotOpen, "That Lathe window closed."))?;
                let panel = workspace
                    .read(cx)
                    .panel::<AgentPanel>(cx)
                    .ok_or_else(|| ErrorBody::new(ErrorCode::WorkspaceNotOpen, "This project has no agent panel open."))?;
                let available = available_agents(&workspace, cx);
                let agent = available
                    .into_iter()
                    .find(|(agent, _)| agent.id().0.as_ref() == agent_id)
                    .map(|(agent, _)| agent)
                    .ok_or_else(|| ErrorBody::new(ErrorCode::AgentNotFound, "That agent isn't available in this project."))?;
                // Created alongside the panel's other threads, so the thread
                // showing on the desk stays put.
                Ok(panel.update(cx, |panel, cx| {
                    panel.create_thread_with_options(
                        CreateThreadOptions {
                            agent: Some(agent),
                            initial_content: Some(AgentInitialContent::ContentBlock {
                                blocks: vec![text.into()],
                                auto_submit: true,
                            }),
                            ..CreateThreadOptions::default()
                        },
                        AgentThreadSource::AgentPanel,
                        window,
                        cx,
                    )
                }))
            })
            .map_err(|_| ErrorBody::new(ErrorCode::WorkspaceNotOpen, "That Lathe window closed."))??;
        log::info!("companion: phone {device_id} started thread {} with {agent_id}", created.to_key_string());

        let timeout = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NEW_THREAD_TIMEOUT).await;
            this.update(cx, |this, cx| {
                let Some(pending) = this.pending_new_threads.remove(&created) else {
                    return;
                };
                log::warn!("companion: thread {} didn't start within {}s", created.to_key_string(), NEW_THREAD_TIMEOUT.as_secs());
                this.ack(
                    &pending.device_id,
                    &pending.client_msg_id,
                    Err(ErrorBody::new(ErrorCode::Internal, "The agent didn't start. Check Lathe.")),
                    cx,
                );
            })
            .log_err();
        });
        self.pending_new_threads.insert(
            created,
            PendingNewThread {
                device_id: device_id.to_string(),
                client_msg_id: client_msg_id.to_string(),
                _timeout: timeout,
            },
        );
        self.complete_new_threads(cx);
        Ok(())
    }

    /// A new thread is ready for the phone once it has a session: before that
    /// it can't be subscribed to.
    fn complete_new_threads(&mut self, cx: &mut Context<Self>) {
        if self.pending_new_threads.is_empty() {
            return;
        }
        let Some(store) = ThreadMetadataStore::try_global(cx) else {
            return;
        };
        let started = self
            .pending_new_threads
            .keys()
            .filter(|thread_id| store.read(cx).entry(**thread_id).is_some_and(|metadata| metadata.session_id.is_some()))
            .copied()
            .collect::<Vec<_>>();
        for thread_id in started {
            if let Some(pending) = self.pending_new_threads.remove(&thread_id) {
                self.ack_with_thread(&pending.device_id, &pending.client_msg_id, Ok(Some(thread_id.to_key_string())), cx);
            }
        }
        self.schedule_list_flush(cx);
    }

    /// Renames the thread the way the sidebar does: through the thread's view
    /// wherever one is open, which also updates its title editor and saves the
    /// rename, and straight to the saved title otherwise.
    fn rename(&mut self, thread_id: &str, title: &str, cx: &mut Context<Self>) -> Result<(), ErrorBody> {
        let title = title.trim();
        if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
            return Err(ErrorBody::new(
                ErrorCode::BadMessage,
                format!("A thread name has to be 1 to {MAX_TITLE_CHARS} characters."),
            ));
        }
        let metadata = self
            .thread_metadata(thread_id, cx)
            .ok_or_else(|| ErrorBody::new(ErrorCode::ThreadNotFound, "This thread no longer exists."))?;
        let title = SharedString::from(title.to_string());
        if !rename_in_open_views(metadata.thread_id, &title, cx)
            && let Some(store) = ThreadMetadataStore::try_global(cx)
        {
            store.update(cx, |store, cx| {
                store.set_title_override(metadata.thread_id, title.clone(), cx)
            });
        }
        if let Some(session_id) = &metadata.session_id {
            self.send_title(session_id, title.to_string(), cx);
        }
        self.schedule_list_flush(cx);
        Ok(())
    }

    /// The name the phone shows: the user's rename when there is one,
    /// otherwise the agent's title.
    fn display_title(&self, session_id: &acp::SessionId, cx: &App) -> Option<String> {
        let store = ThreadMetadataStore::try_global(cx)?;
        let renamed = store
            .read(cx)
            .entry_by_session(session_id)
            .and_then(|metadata| metadata.title_override.clone());
        renamed
            .or_else(|| self.threads.read(cx).thread(session_id)?.read(cx).title())
            .map(|title| title.to_string())
    }

    fn respond_elicitation(
        &mut self,
        thread_id: &str,
        elicitation_id: &str,
        value: serde_json::Map<String, serde_json::Value>,
        cx: &mut Context<Self>,
    ) -> Result<(), ErrorBody> {
        let (_, thread) = self.live_thread(thread_id, cx).ok_or_else(not_loaded)?;
        let id = ElicitationEntryId(Arc::from(elicitation_id));
        if thread.read(cx).elicitation(&id).is_none() {
            return Err(ErrorBody::new(ErrorCode::RequestNotFound, "This question was already answered."));
        }
        let content = serde_json::from_value(serde_json::Value::Object(value))
            .map_err(|_| ErrorBody::new(ErrorCode::BadMessage, "The answer doesn't match the form."))?;
        let response = acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
            acp::ElicitationAcceptAction::new().content(Some(content)),
        ));
        thread.update(cx, |thread, cx| thread.respond_to_elicitation(&id, response, cx));
        Ok(())
    }

    /// Prompts waiting for an answer. Sent to every phone, not only those
    /// following the thread, so a push tap or a reconnect shows the sheet.
    fn send_pending_prompts(&self, device_id: &str, only: Option<&acp::SessionId>, cx: &mut Context<Self>) {
        let mut messages = Vec::new();
        for (session_id, thread, _) in self.threads.read(cx).top_level_threads() {
            if only.is_some_and(|only| only != session_id) {
                continue;
            }
            let (Some(thread), Some(thread_id)) = (thread.upgrade(), CompanionThreads::thread_id(session_id, cx)) else {
                continue;
            };
            let thread = thread.read(cx);
            for request in thread.pending_permission_requests() {
                messages.push(entries::tool_authorization(&thread_id, thread, request, cx));
            }
            messages.extend(entries::pending_elicitations(&thread_id, thread));
        }
        for message in messages {
            self.send(device_id, &message, cx);
        }
    }

    fn update_keep_awake(&mut self, cx: &mut Context<Self>) {
        let wanted = cfg!(target_os = "macos")
            && CompanionSettings::get_global(cx).keep_awake_while_agents_run
            && !self.companion.read(cx).devices().is_empty()
            && self.statuses.values().any(|status| *status != ThreadStatus::Idle && *status != ThreadStatus::Error);
        if wanted && self.keep_awake.is_none() {
            self.keep_awake = keep_awake::Assertion::acquire().log_err();
        } else if !wanted {
            self.keep_awake = None;
        }
    }
}

/// Where a page ending at `end` starts: at most `PAGE_SIZE` entries, and no
/// more than fit in one frame. Always at least one entry, which `fit` keeps
/// small enough on its own.
fn page_start(entries: &[Entry], end: usize) -> usize {
    let budget = crate::companion::MAX_FRAME_PLAINTEXT_BYTES - PAGE_OVERHEAD_BYTES;
    let mut start = end;
    let mut used = 0;
    while start > 0 && end - start < PAGE_SIZE {
        let size = entries::entry_size(&entries[start - 1]) + 1;
        if start < end && used + size > budget {
            break;
        }
        used += size;
        start -= 1;
    }
    start
}

/// The thread's model and effort choices, as the phone sees them. Modes and
/// anything else stay on the desk: some modes skip permission prompts.
fn phone_config_options(thread: &AcpThread, cx: &App) -> Option<Vec<ConfigOption>> {
    let provider = thread.connection().session_config_options(thread.session_id(), cx)?;
    let options = convert_config_options(&provider.config_options());
    (!options.is_empty()).then_some(options)
}

/// Only a value the agent offered is passed on.
fn config_value(option: &ConfigOption, value: &serde_json::Value) -> Result<acp::SessionConfigOptionValue, ErrorBody> {
    match (&option.kind, value) {
        (ConfigKind::Select { options, .. }, serde_json::Value::String(value))
            if options.iter().any(|choice| &choice.value == value) =>
        {
            Ok(acp::SessionConfigOptionValue::id(value.clone()))
        }
        (ConfigKind::Boolean { .. }, serde_json::Value::Bool(value)) => Ok(acp::SessionConfigOptionValue::boolean(*value)),
        _ => Err(ErrorBody::new(ErrorCode::BadMessage, "That choice isn't offered for this thread.")),
    }
}

fn convert_config_options(options: &[acp::SessionConfigOption]) -> Vec<ConfigOption> {
    options
        .iter()
        .filter_map(|option| {
            let category = match option.category.as_ref()? {
                acp::SessionConfigOptionCategory::Model => ConfigCategory::Model,
                acp::SessionConfigOptionCategory::ThoughtLevel => ConfigCategory::ThoughtLevel,
                _ => return None,
            };
            let kind = match &option.kind {
                acp::SessionConfigKind::Select(select) => {
                    let choice = |choice: &acp::SessionConfigSelectOption, group: Option<&str>| ConfigChoice {
                        value: choice.value.0.to_string(),
                        name: choice.name.clone(),
                        description: choice.description.clone(),
                        group: group.map(str::to_string),
                    };
                    let options = match &select.options {
                        acp::SessionConfigSelectOptions::Ungrouped(options) => {
                            options.iter().map(|option| choice(option, None)).collect()
                        }
                        acp::SessionConfigSelectOptions::Grouped(groups) => groups
                            .iter()
                            .flat_map(|group| group.options.iter().map(|option| choice(option, Some(&group.name))))
                            .collect(),
                        _ => return None,
                    };
                    ConfigKind::Select {
                        current_value: select.current_value.0.to_string(),
                        options,
                    }
                }
                acp::SessionConfigKind::Boolean(boolean) => ConfigKind::Boolean {
                    current_value: boolean.current_value,
                },
                _ => return None,
            };
            Some(ConfigOption {
                config_id: option.config_id.0.to_string(),
                name: option.name.clone(),
                description: option.description.clone(),
                category,
                kind,
            })
        })
        .collect()
}

fn not_loaded() -> ErrorBody {
    ErrorBody::new(ErrorCode::ThreadNotFound, "Open the thread first.")
}

/// Stable id for a set of project folders, so a workspace keeps its id across
/// Lathe restarts.
fn workspace_id(paths: &PathList) -> String {
    let mut hasher = Sha256::new();
    for path in paths.ordered_paths() {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update([0]);
    }
    encode_url_safe(&hasher.finalize()[..12])
}

fn workspace_name(paths: &PathList) -> String {
    let names = paths
        .ordered_paths()
        .filter_map(|path| path.file_name().map(|name| name.to_string_lossy().into_owned()))
        .collect::<Vec<_>>();
    if names.is_empty() {
        "No project".into()
    } else {
        names.join(", ")
    }
}

fn open_workspace_paths(cx: &App) -> Vec<(PathList, WeakEntity<workspace::Workspace>, gpui::WindowHandle<MultiWorkspace>)> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .flat_map(|window| {
            window
                .read(cx)
                .map(|multi_workspace| {
                    multi_workspace
                        .workspaces()
                        .map(|workspace| {
                            let paths = workspace
                                .read(cx)
                                .project()
                                .read(cx)
                                .worktree_paths(cx)
                                .folder_path_list()
                                .clone();
                            (paths, workspace.downgrade(), window)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect()
}

/// The open workspace with this id, if any window shows it.
pub(crate) fn open_workspace(workspace_id: &str, cx: &App) -> Option<Entity<workspace::Workspace>> {
    open_workspace_paths(cx)
        .into_iter()
        .find(|(paths, _, _)| self::workspace_id(paths) == workspace_id)
        .and_then(|(_, workspace, _)| workspace.upgrade())
}

/// The external agents a new thread from the phone can use in this
/// workspace's project. Lathe's built-in agent isn't offered there.
fn available_agents(workspace: &Entity<workspace::Workspace>, cx: &App) -> Vec<(Agent, String)> {
    let mut agents = Vec::new();
    let store = workspace.read(cx).project().read(cx).agent_server_store().read(cx);
    for agent_id in store.external_agents() {
        let name = store
            .agent_display_name(agent_id)
            .unwrap_or_else(|| agent_id.0.clone());
        agents.push((Agent::from(agent_id.clone()), name.to_string()));
    }
    agents
}

fn workspace_agents(workspace: &WeakEntity<workspace::Workspace>, cx: &App) -> Option<(Vec<AgentSummary>, String)> {
    let workspace = workspace.upgrade()?;
    let panel = workspace.read(cx).panel::<AgentPanel>(cx)?;
    let agents = available_agents(&workspace, cx)
        .into_iter()
        .map(|(agent, name)| AgentSummary {
            agent_id: agent.id().0.to_string(),
            name,
        })
        .collect::<Vec<AgentSummary>>();
    // No agents means the phone hides "New thread" for this workspace.
    if agents.is_empty() {
        return None;
    }
    let selected = panel.read(cx).selected_agent(cx).id().0.to_string();
    // The panel may have the built-in agent selected, which the phone can't
    // offer; the first listed agent stands in for it.
    let default = if agents.iter().any(|agent| agent.agent_id == selected) {
        selected
    } else {
        agents[0].agent_id.clone()
    };
    Some((agents, default))
}

fn workspace_list(threads: &[(ThreadSummary, PathList)], cx: &App) -> Vec<WorkspaceSummary> {
    let open = open_workspace_paths(cx);
    let mut seen = HashSet::default();
    let mut list = Vec::new();
    let candidates = open
        .iter()
        .map(|(paths, _, _)| paths.clone())
        .chain(threads.iter().map(|(_, paths)| paths.clone()));
    for paths in candidates {
        let id = workspace_id(&paths);
        if !seen.insert(id.clone()) {
            continue;
        }
        let open_workspace = open.iter().find(|(open_paths, _, _)| *open_paths == paths);
        let agents = open_workspace.and_then(|(_, workspace, _)| workspace_agents(workspace, cx));
        list.push(WorkspaceSummary {
            workspace_id: id,
            name: workspace_name(&paths),
            paths: paths
                .ordered_paths()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            is_open: open_workspace.is_some(),
            default_agent_id: agents.as_ref().map(|(_, default)| default.clone()),
            agents: agents.map(|(agents, _)| agents),
        });
    }
    list
}

/// Returns whether any open view showed the thread.
fn rename_in_open_views(thread_id: ThreadId, title: &SharedString, cx: &mut App) -> bool {
    let mut renamed = false;
    for (_, workspace, window) in open_workspace_paths(cx) {
        renamed |= window
            .update(cx, |_, window, cx| {
                let Some(panel) = workspace
                    .upgrade()
                    .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
                else {
                    return false;
                };
                let Some(thread_view) = panel
                    .read(cx)
                    .conversation_view_for_id(&thread_id, cx)
                    .and_then(|view| view.read(cx).root_thread_view())
                else {
                    return false;
                };
                thread_view.update(cx, |thread_view, cx| thread_view.rename(title.clone(), window, cx));
                true
            })
            .unwrap_or(false);
    }
    renamed
}

/// Opens the thread in the agent panel of a window showing its project, without
/// focusing that window.
fn load_in_background(metadata: &ThreadMetadata, cx: &mut App) -> bool {
    let Some((_, workspace, window)) = open_workspace_paths(cx)
        .into_iter()
        .find(|(paths, _, _)| paths == metadata.folder_paths())
    else {
        return false;
    };
    let metadata = metadata.clone();
    window
        .update(cx, |_, window, cx| {
            let panel = workspace
                .upgrade()
                .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx));
            let Some(panel) = panel else {
                return false;
            };
            panel.update(cx, |panel, cx| {
                panel.load_agent_thread(
                    Agent::from(metadata.agent_id.clone()),
                    metadata.thread_id,
                    Some(metadata.folder_paths().clone()),
                    metadata.title.clone(),
                    false,
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            });
            true
        })
        .unwrap_or(false)
}

mod keep_awake {
    /// Keeps the Mac from idle-sleeping while held, through `caffeinate`, which
    /// also exits on its own if Lathe dies.
    pub struct Assertion {
        // Held only so dropping the assertion kills the process.
        #[cfg(target_os = "macos")]
        _caffeinate: util::command::Child,
    }

    impl Assertion {
        #[cfg(target_os = "macos")]
        pub fn acquire() -> anyhow::Result<Self> {
            use util::command::{Stdio, new_command};
            let child = new_command("/usr/bin/caffeinate")
                .args(["-i", "-w", &std::process::id().to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            Ok(Self { _caffeinate: child })
        }

        #[cfg(not(target_os = "macos"))]
        pub fn acquire() -> anyhow::Result<Self> {
            anyhow::bail!("keeping the computer awake is only supported on macOS")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::tests::{FakePhone, add_paired, connected_companion, drain, phone_says};
    use acp_thread::{AgentConnection as _, PermissionOptions, StubAgentConnection};
    use agent_client_protocol::schema::v1 as acp_v1;
    use agent_ui::thread_metadata_store::ThreadId;
    use gpui::TestAppContext;
    use project::{FakeFs, Project, WorktreePaths};
    use serde_json::{Value, json};
    use std::{path::Path, rc::Rc};
    use util::path;

    struct Harness {
        bridge: Entity<Bridge>,
        companion: Entity<Companion>,
        outgoing: futures::channel::mpsc::UnboundedReceiver<String>,
        identity: Arc<crate::crypto::Identity>,
        phone: FakePhone,
        seq: u64,
    }

    impl Harness {
        /// Everything the phone received since the last call, decrypted.
        fn received(&mut self) -> Vec<Value> {
            drain(&mut self.outgoing)
                .into_iter()
                .filter(|message| message["type"] == "frame")
                .map(|frame| self.phone.open(&self.identity.box_public_key(), &frame))
                .collect()
        }

        /// Messages sent to the relay itself, such as pushes.
        fn split(&mut self) -> (Vec<Value>, Vec<Value>) {
            let all = drain(&mut self.outgoing);
            let (frames, relay): (Vec<_>, Vec<_>) =
                all.into_iter().partition(|message| message["type"] == "frame");
            let frames = frames
                .into_iter()
                .map(|frame| self.phone.open(&self.identity.box_public_key(), &frame))
                .collect();
            (frames, relay)
        }

        fn say(&mut self, message: Value, cx: &mut TestAppContext) {
            self.seq += 1;
            phone_says(&self.companion, &self.phone, &self.identity.box_public_key(), self.seq, message, cx);
            cx.run_until_parked();
        }
    }

    fn of_type<'a>(messages: &'a [Value], kind: &str) -> Vec<&'a Value> {
        messages.iter().filter(|message| message["type"] == kind).collect()
    }

    #[gpui::test]
    async fn follows_and_drives_a_thread_from_the_phone(cx: &mut TestAppContext) {
        let (companion, outgoing, identity) = connected_companion(cx);
        cx.update(|cx| {
            crate::tests::enable_companion_for_test(cx);
            crate::init_threads(cx);
            ThreadMetadataStore::init_global(cx);
            release_channel::init_test(semver::Version::new(1, 2, 3), ReleaseChannel::Dev, cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;

        let tool_call_id = acp::ToolCallId::new("1");
        let connection = Rc::new(StubAgentConnection::new().with_permission_requests(
            HashMap::from_iter([(
                tool_call_id.clone(),
                PermissionOptions::Flat(vec![
                    acp::PermissionOption::new("once", "Allow", acp::PermissionOptionKind::AllowOnce),
                    acp::PermissionOption::new("always", "Always allow", acp::PermissionOptionKind::AllowAlways),
                ]),
            )]),
        ));
        let folders = PathList::new(&[Path::new(path!("/test"))]);
        let thread = cx
            .update(|cx| connection.clone().new_session(project, folders.clone(), cx))
            .await
            .unwrap();
        let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());
        let thread_id = ThreadId::new();
        // The store reloads from disk on startup, which would replace a thread
        // saved before the reload finishes.
        cx.update(|cx| ThreadMetadataStore::global(cx).read(cx).reload_task()).await;
        cx.update(|cx| {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(
                    ThreadMetadata {
                        thread_id,
                        session_id: Some(session_id.clone()),
                        agent_id: project::AgentId("claude-acp".into()),
                        title: Some("Fix the relay".into()),
                        title_override: None,
                        updated_at: chrono::Utc::now(),
                        created_at: None,
                        interacted_at: None,
                        workspace_id: None,
                        worktree_paths: WorktreePaths::from_folder_paths(&folders),
                        remote_connection: None,
                        archived: false,
                    },
                    cx,
                )
            })
        });
        cx.run_until_parked();

        let threads = cx.update(|cx| CompanionThreads::global(cx).unwrap());
        let bridge = cx.new(|cx| Bridge::new(companion.clone(), threads, cx));
        let phone = FakePhone::new();
        add_paired(&companion, &phone, cx);
        let mut harness = Harness { bridge, companion, outgoing, identity, phone, seq: 0 };
        drain(&mut harness.outgoing);

        // Connecting gets the phone the full picture.
        let device_id = harness.phone.device_id();
        harness.companion.update(cx, |companion, cx| {
            companion.handle_relay_message(json!({ "type": "presence", "device_id": device_id, "online": true }), cx)
        });
        cx.run_until_parked();
        let received = harness.received();
        assert_eq!(received[0]["type"], "hello");
        assert_eq!(received[0]["lathe_version"], "1.2.3");
        assert_eq!(received[0]["features"], json!(["pull_requests"]));
        let workspaces = of_type(&received, "workspaces");
        assert_eq!(workspaces[0]["list"][0]["name"], "test");
        let listed = &of_type(&received, "threads")[0]["list"][0];
        assert_eq!(listed["thread_id"], json!(thread_id.to_key_string()));
        assert_eq!(listed["title"], "Fix the relay");
        assert_eq!(listed["is_loaded"], true);
        assert_eq!(listed["status"], "idle");
        assert_eq!(listed["workspace_id"], workspaces[0]["list"][0]["workspace_id"]);

        let thread_key = thread_id.to_key_string();
        harness.say(json!({ "type": "subscribe", "client_msg_id": "m1", "thread_id": thread_key }), cx);
        let received = harness.received();
        assert_eq!(
            of_type(&received, "thread_snapshot")[0],
            &json!({ "type": "thread_snapshot", "thread_id": thread_key, "seq": 0, "start_index": 0, "entries": [], "has_more_before": false })
        );
        assert_eq!(of_type(&received, "ack")[0], &json!({ "type": "ack", "client_msg_id": "m1", "ok": true }));

        // Renaming sticks as a user title, reaches subscribers as a delta, and
        // shows in the next thread list.
        harness.say(json!({ "type": "rename_thread", "client_msg_id": "r1", "thread_id": thread_key, "title": "  Relay ping fix  " }), cx);
        cx.executor().advance_clock(LIST_FLUSH_DELAY * 2);
        cx.run_until_parked();
        let received = harness.received();
        assert_eq!(of_type(&received, "ack")[0], &json!({ "type": "ack", "client_msg_id": "r1", "ok": true }));
        assert_eq!(
            of_type(&received, "thread_delta")[0]["op"],
            json!({ "kind": "title", "title": "Relay ping fix" })
        );
        assert_eq!(of_type(&received, "threads").last().unwrap()["list"][0]["title"], "Relay ping fix");
        cx.update(|cx| {
            let store = ThreadMetadataStore::global(cx);
            let metadata = store.read(cx).entry(thread_id).unwrap();
            assert_eq!(metadata.title_override.as_deref(), Some("Relay ping fix"));
        });
        harness.say(json!({ "type": "rename_thread", "client_msg_id": "r2", "thread_id": thread_key, "title": "x".repeat(201) }), cx);
        harness.say(json!({ "type": "rename_thread", "client_msg_id": "r3", "thread_id": "missing", "title": "Name" }), cx);
        let codes = harness
            .received()
            .iter()
            .filter(|message| message["type"] == "ack")
            .map(|ack| ack["error"]["code"].clone())
            .collect::<Vec<_>>();
        assert_eq!(codes, [json!("bad_message"), json!("thread_not_found")]);

        // The phone sends a message; the agent asks for permission.
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::ToolCall(
            acp_v1::ToolCall::new(acp_v1::ToolCallId::new("1"), "Edit relay.rs").kind(acp_v1::ToolKind::Edit),
        )]);
        harness.say(json!({ "type": "send_message", "client_msg_id": "m2", "thread_id": thread_key, "text": "Fix it" }), cx);
        cx.executor().advance_clock(ENTRY_FLUSH_DELAY * 2);
        cx.run_until_parked();
        let (received, relay) = harness.split();
        assert_eq!(of_type(&received, "ack")[0]["ok"], true);
        let ops = of_type(&received, "thread_delta")
            .iter()
            .map(|delta| delta["op"].clone())
            .collect::<Vec<_>>();
        assert!(ops.contains(&json!({ "kind": "new_entry", "entry": { "kind": "user_message", "text": "Fix it" } })), "{ops:#?}");
        assert!(ops.iter().any(|op| op["entry"]["kind"] == "tool_call" && op["entry"]["title"] == "Edit relay.rs"), "{ops:#?}");
        assert!(ops.contains(&json!({ "kind": "status", "status": "waiting_authorization" })), "{ops:#?}");
        let seqs = of_type(&received, "thread_delta").iter().map(|delta| delta["seq"].as_u64().unwrap()).collect::<Vec<_>>();
        // The rename above was seq 1, so these continue from 2 without gaps.
        assert_eq!(seqs, (2..2 + seqs.len() as u64).collect::<Vec<_>>(), "deltas must be contiguous");
        let prompt = of_type(&received, "tool_authorization")[0].clone();
        assert_eq!(prompt["tool_title"], "Edit relay.rs");
        assert_eq!(prompt["options"].as_array().unwrap().len(), 2);
        // The phone is watching, so no push.
        assert!(of_type(&relay, "push").is_empty());

        // Standing rules stay on the desktop.
        let request_id = prompt["request_id"].clone();
        harness.say(json!({ "type": "authorize", "client_msg_id": "m3", "thread_id": thread_key, "request_id": request_id, "option_id": "always" }), cx);
        let received = harness.received();
        assert_eq!(of_type(&received, "ack")[0]["error"]["code"], "unsupported");

        harness.say(json!({ "type": "authorize", "client_msg_id": "m4", "thread_id": thread_key, "request_id": request_id, "option_id": "once" }), cx);
        cx.executor().advance_clock(ENTRY_FLUSH_DELAY * 2);
        cx.run_until_parked();
        let received = harness.received();
        assert_eq!(of_type(&received, "ack")[0], &json!({ "type": "ack", "client_msg_id": "m4", "ok": true }));
        assert_eq!(of_type(&received, "tool_authorization_resolved")[0]["request_id"], request_id);
        thread.read_with(cx, |thread, _| assert_eq!(thread.pending_permission_requests().count(), 0));

        // A replayed message is ignored rather than acted on twice.
        harness.seq += 1;
        phone_says(&harness.companion, &harness.phone, &harness.identity.box_public_key(), harness.seq,
            json!({ "type": "authorize", "client_msg_id": "m4", "thread_id": thread_key, "request_id": request_id, "option_id": "once" }), cx);
        cx.run_until_parked();
        assert!(harness.received().is_empty());

        // Once nobody is watching, the next stop is pushed.
        harness.say(json!({ "type": "unsubscribe", "client_msg_id": "m5", "thread_id": thread_key }), cx);
        harness.received();
        harness.bridge.update(cx, |bridge, cx| {
            bridge.status_changed(&session_id, ThreadStatus::Running, cx);
            bridge.status_changed(&session_id, ThreadStatus::Idle, cx);
        });
        let (_, relay) = harness.split();
        assert_eq!(
            of_type(&relay, "push"),
            [&json!({ "type": "push", "kind": "finished", "thread_id": thread_key })]
        );
    }

    #[gpui::test]
    async fn answers_unknown_requests_with_errors(cx: &mut TestAppContext) {
        let (companion, outgoing, identity) = connected_companion(cx);
        cx.update(|cx| {
            crate::init_threads(cx);
            ThreadMetadataStore::init_global(cx);
        });
        let threads = cx.update(|cx| CompanionThreads::global(cx).unwrap());
        let bridge = cx.new(|cx| Bridge::new(companion.clone(), threads, cx));
        let phone = FakePhone::new();
        add_paired(&companion, &phone, cx);
        let mut harness = Harness { bridge, companion, outgoing, identity, phone, seq: 0 };
        drain(&mut harness.outgoing);

        harness.say(json!({ "type": "subscribe", "client_msg_id": "m1", "thread_id": "missing" }), cx);
        harness.say(json!({ "type": "new_thread", "client_msg_id": "m2", "workspace_id": "w", "agent_id": "a", "text": "hi" }), cx);
        harness.say(json!({ "type": "new_thread", "client_msg_id": "m4", "workspace_id": "w", "agent_id": "a", "text": "   " }), cx);
        harness.say(json!({ "type": "launch_missiles", "client_msg_id": "m3" }), cx);
        harness.say(json!({ "type": "list_pull_requests", "client_msg_id": "p1", "workspace_id": "w", "filter": "open", "reviewing": false }), cx);
        harness.say(json!({ "type": "get_pull_request_diff", "client_msg_id": "p2", "workspace_id": "w", "repo_id": "r", "number": 1 }), cx);
        harness.say(
            json!({ "type": "reply_review_comment", "client_msg_id": "p3", "workspace_id": "w", "repo_id": "r", "number": 1, "comment_id": "x", "body": "hi" }),
            cx,
        );
        harness.say(json!({ "type": "submit_review", "client_msg_id": "p4", "workspace_id": "w", "repo_id": "r", "number": 1, "verdict": "merge" }), cx);
        harness.say(
            json!({ "type": "create_review_comment", "client_msg_id": "p5", "workspace_id": "w", "repo_id": "r", "number": 1, "path": "src/a.rs", "line": 3, "side": "new", "body": "Why?" }),
            cx,
        );
        harness.say(
            json!({ "type": "create_review_comment", "client_msg_id": "p6", "workspace_id": "w", "repo_id": "r", "number": 1, "path": "src/a.rs", "line": 3, "side": "new", "body": "  " }),
            cx,
        );
        harness.say(
            json!({ "type": "create_review_comment", "client_msg_id": "p7", "workspace_id": "w", "repo_id": "r", "number": 1, "path": "src/a.rs", "line": 3, "side": "middle", "body": "Why?" }),
            cx,
        );
        let codes = harness
            .received()
            .iter()
            .map(|ack| (ack["client_msg_id"].clone(), ack["error"]["code"].clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            codes,
            [
                (json!("m1"), json!("thread_not_found")),
                (json!("m2"), json!("workspace_not_open")),
                (json!("m4"), json!("bad_message")),
                (json!("m3"), json!("bad_message")),
                (json!("p1"), json!("workspace_not_open")),
                (json!("p2"), json!("workspace_not_open")),
                (json!("p3"), json!("bad_message")),
                (json!("p4"), json!("bad_message")),
                (json!("p5"), json!("workspace_not_open")),
                (json!("p6"), json!("bad_message")),
                (json!("p7"), json!("bad_message")),
            ]
        );
    }

    #[gpui::test]
    async fn acks_a_new_thread_once_it_has_a_session(cx: &mut TestAppContext) {
        let (companion, outgoing, identity) = connected_companion(cx);
        cx.update(|cx| {
            crate::init_threads(cx);
            ThreadMetadataStore::init_global(cx);
        });
        cx.update(|cx| ThreadMetadataStore::global(cx).read(cx).reload_task()).await;
        let threads = cx.update(|cx| CompanionThreads::global(cx).unwrap());
        let bridge = cx.new(|cx| Bridge::new(companion.clone(), threads, cx));
        let phone = FakePhone::new();
        add_paired(&companion, &phone, cx);
        let mut harness = Harness { bridge, companion, outgoing, identity, phone, seq: 0 };
        drain(&mut harness.outgoing);

        // What `new_thread` leaves behind after asking the panel for a thread.
        let thread_id = ThreadId::new();
        let device_id = harness.phone.device_id();
        harness.bridge.update(cx, |bridge, _| {
            bridge.pending_new_threads.insert(
                thread_id,
                PendingNewThread {
                    device_id,
                    client_msg_id: "n1".into(),
                    _timeout: Task::ready(()),
                },
            );
        });
        let save = |session_id: Option<acp::SessionId>, cx: &mut TestAppContext| {
            cx.update(|cx| {
                ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                    store.save(
                        ThreadMetadata {
                            thread_id,
                            session_id,
                            agent_id: project::AgentId("claude-acp".into()),
                            title: None,
                            title_override: None,
                            updated_at: chrono::Utc::now(),
                            created_at: None,
                            interacted_at: None,
                            workspace_id: None,
                            worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[Path::new(path!("/test"))])),
                            remote_connection: None,
                            archived: false,
                        },
                        cx,
                    )
                })
            });
            cx.run_until_parked();
        };

        // A draft without a session can't be subscribed to yet.
        save(None, cx);
        assert!(harness.received().iter().all(|message| message["type"] != "ack"));

        save(Some(acp::SessionId::new("s1")), cx);
        let received = harness.received();
        let ack = received.iter().find(|message| message["type"] == "ack").unwrap();
        assert_eq!(
            ack,
            &json!({ "type": "ack", "client_msg_id": "n1", "ok": true, "thread_id": thread_id.to_key_string() })
        );
        harness.bridge.read_with(cx, |bridge, _| assert!(bridge.pending_new_threads.is_empty()));
    }

    fn select(id: &str, category: Option<acp::SessionConfigOptionCategory>, values: &[&str]) -> acp::SessionConfigOption {
        let options = values
            .iter()
            .map(|value| acp::SessionConfigSelectOption::new(value.to_string(), value.to_uppercase()))
            .collect::<Vec<_>>();
        let mut option = acp::SessionConfigOption::select(id.to_string(), id.to_uppercase(), values[0].to_string(), options);
        option.category = category;
        option
    }

    #[test]
    fn only_model_and_effort_reach_the_phone() {
        let options = convert_config_options(&[
            select("model", Some(acp::SessionConfigOptionCategory::Model), &["opus", "sonnet"]),
            select("effort", Some(acp::SessionConfigOptionCategory::ThoughtLevel), &["low", "high"]),
            // Some modes skip permission prompts, so they stay on the desk.
            select("mode", Some(acp::SessionConfigOptionCategory::Mode), &["default", "bypassPermissions"]),
            select("uncategorized", None, &["a"]),
        ]);
        assert_eq!(
            options.iter().map(|option| option.config_id.as_str()).collect::<Vec<_>>(),
            ["model", "effort"]
        );
        assert_eq!(options[1].category, ConfigCategory::ThoughtLevel);
        assert_eq!(
            options[0].kind,
            ConfigKind::Select {
                current_value: "opus".into(),
                options: vec![
                    ConfigChoice { value: "opus".into(), name: "OPUS".into(), description: None, group: None },
                    ConfigChoice { value: "sonnet".into(), name: "SONNET".into(), description: None, group: None },
                ],
            }
        );
    }

    #[test]
    fn only_offered_values_are_passed_on() {
        let options = convert_config_options(&[select("effort", Some(acp::SessionConfigOptionCategory::ThoughtLevel), &["low", "high"])]);
        let effort = &options[0];
        assert_eq!(config_value(effort, &json!("high")).ok(), Some(acp::SessionConfigOptionValue::id("high")));
        assert!(config_value(effort, &json!("max")).is_err());
        assert!(config_value(effort, &json!(true)).is_err());
        assert!(config_value(effort, &json!(null)).is_err());
    }

    #[test]
    fn pages_stay_within_one_frame() {
        let big = |index: usize| Entry::AssistantMessage { markdown: format!("{index}{}", "x".repeat(40_000)) };
        let entries = (0..120).map(big).collect::<Vec<_>>();
        let start = page_start(&entries, entries.len());
        let page = &entries[start..];
        let size: usize = page.iter().map(entries::entry_size).sum();
        assert!(size <= crate::companion::MAX_FRAME_PLAINTEXT_BYTES, "{size}");
        assert!(page.len() >= 3 && page.len() < PAGE_SIZE, "{}", page.len());

        let small = (0..120).map(|index| Entry::UserMessage { text: index.to_string() }).collect::<Vec<_>>();
        assert_eq!(page_start(&small, small.len()), 120 - PAGE_SIZE);
        assert_eq!(page_start(&small, 10), 0);
        assert_eq!(page_start(&small, 0), 0);
    }

    #[test]
    fn workspace_ids_are_stable_and_distinct() {
        let first = PathList::new(&[Path::new("/a")]);
        let second = PathList::new(&[Path::new("/b")]);
        assert_eq!(workspace_id(&first), workspace_id(&first.clone()));
        assert_ne!(workspace_id(&first), workspace_id(&second));
        assert_eq!(workspace_name(&PathList::new(&[Path::new("/code/lathe"), Path::new("/code/app")])), "lathe, app");
    }
}

#[cfg(test)]
mod off_tests {
    use super::*;
    use acp_thread::{AgentConnection as _, StubAgentConnection};
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use std::{path::Path, rc::Rc};
    use util::path;

    #[gpui::test]
    async fn ignores_threads_while_the_companion_is_off(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            crate::init_threads(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let connection = Rc::new(StubAgentConnection::new());
        let _thread = cx
            .update(|cx| connection.new_session(project, PathList::new(&[Path::new(path!("/test"))]), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        let threads = cx.update(|cx| CompanionThreads::global(cx).unwrap());
        threads.read_with(cx, |threads, _| assert_eq!(threads.top_level_threads().count(), 0));
    }
}
