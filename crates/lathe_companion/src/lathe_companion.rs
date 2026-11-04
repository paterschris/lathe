//! Desktop side of the Lathe Companion mobile app: tracks live agent threads so
//! a paired phone can follow and continue them through the companion relay.

mod bridge;
mod companion;
mod companion_modal;
mod connection;
mod entries;
mod pull_requests;
mod terminals;
pub mod crypto;
pub mod protocol;

use acp_thread::{AcpThread, AcpThreadEvent, ForegroundActivity};
use agent_client_protocol::schema::v2 as acp;
use agent_ui::thread_metadata_store::ThreadMetadataStore;
use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, WeakEntity};
use protocol::ThreadStatus;
use settings::{RegisterSetting, Settings};

#[derive(Clone, Debug, RegisterSetting)]
pub struct CompanionSettings {
    pub enabled: bool,
    pub relay_url: String,
    pub keep_awake_while_agents_run: bool,
    pub terminal_input: bool,
}

impl Settings for CompanionSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let companion = content.companion.as_ref().unwrap();
        Self {
            enabled: companion.enabled.unwrap(),
            relay_url: companion.relay_url.clone().unwrap(),
            keep_awake_while_agents_run: companion.keep_awake_while_agents_run.unwrap(),
            terminal_input: companion.terminal_input.unwrap(),
        }
    }
}

pub use companion_modal::OpenCompanion;
pub use companion::{Companion, CompanionEvent, ConnectionStatus, PairedDevice, PairingState};

pub fn init(cx: &mut App) {
    companion::init(cx);
    companion_modal::register_title_bar_indicator(cx);
    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
        companion_modal::register(workspace)
    })
    .detach();
    init_threads(cx);
    terminals::init(cx);
    bridge::init(cx);
}

fn init_threads(cx: &mut App) {
    let threads = cx.new(|_| CompanionThreads::default());
    cx.set_global(GlobalCompanionThreads(threads.clone()));

    // Every agent thread is an `AcpThread` entity, whichever view or agent created
    // it, so observing creation covers the agent panel, subagents, and threads
    // loaded in the background without a hook in upstream code.
    cx.observe_new(move |_: &mut AcpThread, _, cx: &mut Context<AcpThread>| {
        // With the companion off, agent threads run exactly as they would
        // without this crate.
        if !CompanionSettings::get_global(cx).enabled {
            return;
        }
        let thread = cx.entity();
        let threads = threads.clone();
        // The thread is still being constructed and leased here; reading it
        // has to wait until the update that created it finishes.
        App::defer(cx, move |cx| {
            threads.update(cx, |threads, cx| threads.track(thread, cx));
        });
    })
    .detach();
}

struct GlobalCompanionThreads(Entity<CompanionThreads>);

impl Global for GlobalCompanionThreads {}

#[derive(Clone, Debug, PartialEq)]
pub enum CompanionThreadsEvent {
    ThreadAdded(acp::SessionId),
    ThreadRemoved(acp::SessionId),
    StatusChanged {
        session_id: acp::SessionId,
        status: ThreadStatus,
    },
    TitleChanged(acp::SessionId),
}

#[derive(Default)]
pub struct CompanionThreads {
    threads: HashMap<acp::SessionId, TrackedThread>,
}

struct TrackedThread {
    thread: WeakEntity<AcpThread>,
    is_subagent: bool,
    status: ThreadStatus,
    // `AcpThreadEvent::Error` is a one-off event rather than thread state, so the
    // error is remembered until the thread starts doing something else.
    errored: bool,
    _subscriptions: [Subscription; 2],
}

impl EventEmitter<CompanionThreadsEvent> for CompanionThreads {}

impl CompanionThreads {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalCompanionThreads>()
            .map(|global| global.0.clone())
    }

    /// Top-level threads the phone can list. Subagent threads are reached through
    /// their parent's tool calls instead.
    pub fn top_level_threads(
        &self,
    ) -> impl Iterator<Item = (&acp::SessionId, &WeakEntity<AcpThread>, ThreadStatus)> {
        self.threads
            .iter()
            .filter(|(_, tracked)| !tracked.is_subagent)
            .map(|(session_id, tracked)| (session_id, &tracked.thread, tracked.status))
    }

    pub fn thread(&self, session_id: &acp::SessionId) -> Option<Entity<AcpThread>> {
        self.threads.get(session_id)?.thread.upgrade()
    }

    pub fn status(&self, session_id: &acp::SessionId) -> Option<ThreadStatus> {
        Some(self.threads.get(session_id)?.status)
    }

    /// Lathe's own thread id for a session, as the phone and the thread history
    /// know it. `None` until the metadata store has recorded the session.
    pub fn thread_id(session_id: &acp::SessionId, cx: &App) -> Option<String> {
        let store = ThreadMetadataStore::try_global(cx)?;
        let metadata = store.read(cx).entry_by_session(session_id)?;
        Some(metadata.thread_id.to_key_string())
    }

    fn track(&mut self, thread: Entity<AcpThread>, cx: &mut Context<Self>) {
        let (session_id, is_subagent) = {
            let thread = thread.read(cx);
            (
                thread.session_id().clone(),
                thread.parent_session_id().is_some(),
            )
        };
        if self.threads.contains_key(&session_id) {
            return;
        }

        let subscriptions = [
            cx.subscribe(&thread, {
                let session_id = session_id.clone();
                move |this, thread, event, cx| {
                    this.handle_thread_event(&session_id, &thread, event, cx)
                }
            }),
            cx.observe_release(&thread, {
                let session_id = session_id.clone();
                move |this, _, cx| {
                    if this.threads.remove(&session_id).is_some() {
                        log::debug!("companion: stopped tracking thread {session_id}");
                        cx.emit(CompanionThreadsEvent::ThreadRemoved(session_id.clone()));
                    }
                }
            }),
        ];

        let status = thread_status(thread.read(cx), false);
        log::debug!(
            "companion: tracking thread {session_id} (subagent: {is_subagent}, status: {status:?})"
        );
        self.threads.insert(
            session_id.clone(),
            TrackedThread {
                thread: thread.downgrade(),
                is_subagent,
                status,
                errored: false,
                _subscriptions: subscriptions,
            },
        );
        cx.emit(CompanionThreadsEvent::ThreadAdded(session_id));
    }

    fn handle_thread_event(
        &mut self,
        session_id: &acp::SessionId,
        thread: &Entity<AcpThread>,
        event: &AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(tracked) = self.threads.get_mut(session_id) else {
            return;
        };
        match event {
            AcpThreadEvent::TitleUpdated => {
                cx.emit(CompanionThreadsEvent::TitleChanged(session_id.clone()));
                return;
            }
            AcpThreadEvent::Error | AcpThreadEvent::LoadError(_) => tracked.errored = true,
            AcpThreadEvent::StatusChanged
            | AcpThreadEvent::ActivityConfirmedIdle
            | AcpThreadEvent::ToolAuthorizationRequested(_)
            | AcpThreadEvent::ToolAuthorizationReceived(_)
            | AcpThreadEvent::ElicitationRequested(_)
            | AcpThreadEvent::ElicitationResponded(_)
            | AcpThreadEvent::Stopped { .. } => {}
            // Entry churn while streaming is the bulk of the traffic, so it is
            // dismissed before any work is done.
            _ => return,
        }

        if thread.read(cx).foreground_activity().is_active() {
            tracked.errored = false;
        }
        let status = thread_status(thread.read(cx), tracked.errored);
        if status != tracked.status {
            tracked.status = status;
            log::debug!("companion: thread {session_id} is now {status:?}");
            cx.emit(CompanionThreadsEvent::StatusChanged {
                session_id: session_id.clone(),
                status,
            });
        }
    }
}

fn thread_status(thread: &AcpThread, errored: bool) -> ThreadStatus {
    if thread.pending_permission_requests().next().is_some() {
        return ThreadStatus::WaitingAuthorization;
    }
    match thread.foreground_activity() {
        ForegroundActivity::RequiresAction => ThreadStatus::WaitingInput,
        ForegroundActivity::Running => ThreadStatus::Running,
        ForegroundActivity::Idle | ForegroundActivity::Unconfirmed | ForegroundActivity::Unknown
            if errored =>
        {
            ThreadStatus::Error
        }
        ForegroundActivity::Idle | ForegroundActivity::Unconfirmed | ForegroundActivity::Unknown => {
            ThreadStatus::Idle
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::{
        AgentConnection as _, PermissionOptions, SelectedPermissionOutcome, StubAgentConnection,
    };
    use agent_client_protocol::schema::v1 as acp_v1;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use std::{path::Path, rc::Rc};
    use util::{path, path_list::PathList};

    pub(crate) fn enable_companion_for_test(cx: &mut App) {
        use gpui::UpdateGlobal as _;
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.companion.get_or_insert_default().enabled = Some(true);
            });
        });
    }

    #[gpui::test]
    async fn tracks_thread_status_through_a_permission_request(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            enable_companion_for_test(cx);
            init_threads(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;

        let tool_call_id = acp::ToolCallId::new("1");
        let connection = Rc::new(StubAgentConnection::new().with_permission_requests(
            HashMap::from_iter([(
                tool_call_id.clone(),
                PermissionOptions::Flat(vec![acp::PermissionOption::new(
                    "allow",
                    "Allow",
                    acp::PermissionOptionKind::AllowOnce,
                )]),
            )]),
        ));
        let thread = cx
            .update(|cx| {
                connection.clone().new_session(
                    project,
                    PathList::new(&[Path::new(path!("/test"))]),
                    cx,
                )
            })
            .await
            .expect("stub session");
        cx.run_until_parked();

        let threads = cx.update(|cx| CompanionThreads::global(cx).expect("initialized"));
        let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());
        let events = Rc::new(std::cell::RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            cx.subscribe(&threads, {
                let events = events.clone();
                move |_, event: &CompanionThreadsEvent, _| events.borrow_mut().push(event.clone())
            })
        });
        threads.read_with(cx, |threads, _| {
            assert_eq!(threads.status(&session_id), Some(ThreadStatus::Idle));
            assert_eq!(threads.top_level_threads().count(), 1);
        });

        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::ToolCall(
            acp_v1::ToolCall::new(acp_v1::ToolCallId::new(tool_call_id.0.clone()), "Edit file")
                .kind(acp_v1::ToolKind::Edit),
        )]);
        let response = thread.update(cx, |thread, cx| thread.send_raw("Hi", cx));
        cx.run_until_parked();

        threads.read_with(cx, |threads, _| {
            assert_eq!(
                threads.status(&session_id),
                Some(ThreadStatus::WaitingAuthorization)
            );
        });
        assert!(events.borrow().contains(&CompanionThreadsEvent::StatusChanged {
            session_id: session_id.clone(),
            status: ThreadStatus::WaitingAuthorization,
        }));

        thread.update(cx, |thread, cx| {
            thread.authorize_tool_call(
                tool_call_id,
                SelectedPermissionOutcome::new(
                    acp::PermissionOptionId::new("allow"),
                    acp::PermissionOptionKind::AllowOnce,
                ),
                cx,
            )
        });
        response.await.expect("prompt completes");
        cx.run_until_parked();
        threads.read_with(cx, |threads, _| {
            assert_eq!(threads.status(&session_id), Some(ThreadStatus::Idle));
        });

        // Release observers run when effects are flushed, which `update` does and
        // `run_until_parked` alone does not.
        cx.update(|_| drop(thread));
        cx.run_until_parked();
        threads.read_with(cx, |threads, _| {
            assert_eq!(threads.status(&session_id), None);
        });
        assert!(
            events
                .borrow()
                .contains(&CompanionThreadsEvent::ThreadRemoved(session_id))
        );
    }
}
