//! Consent for downloading and executing binaries that Lathe fetches on the user's behalf.
//!
//! Lathe downloads several kinds of executable code as a side effect of opening a file: its own
//! Node.js runtime, npm packages, language servers, debug adapters, and files requested by
//! extensions. The `allow_binary_downloads` setting can refuse all of that outright. This module
//! implements the middle setting, `prompt_before_binary_downloads`, where each download is instead
//! put to the user.
//!
//! Requests arrive from background threads deep inside the download paths, so they are funneled
//! through a global [`BinaryDownloadConsent`] entity that coalesces everything currently pending
//! into a single question and fans the answers back out over oneshot channels. This mirrors the
//! way [`crate::trusted_worktrees`] gates language server startup on worktree trust.
//!
//! Approvals are keyed by name *and* version. Remembering an approval by name alone would let the
//! next auto-update fetch new, unreviewed code silently, which is the behavior this feature exists
//! to prevent. An approved version of [`ALWAYS`] opts out of that and approves future updates too.

use std::sync::Arc;

use collections::HashMap;
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use settings::{Settings as _, update_settings_file};

use crate::project_settings::ProjectSettings;

/// The approved-version value that approves an item permanently, including future updates.
pub const ALWAYS: &str = "*";

/// Something Lathe wants to download and then execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryDownloadRequest {
    /// The name the user would recognize, such as `rust-analyzer` or `Node.js`. This is the key
    /// that approvals are remembered under, so it must be stable across versions.
    pub name: SharedString,
    /// The version being fetched, used to decide whether a remembered approval still applies.
    /// `None` when the source cannot describe a version, in which case only a permanent approval
    /// applies and the user is otherwise asked every time.
    pub version: Option<SharedString>,
    /// Where the bytes come from, shown so the user can see the host they are trusting.
    pub url: SharedString,
    /// What kind of thing this is, used to group the prompt.
    pub kind: BinaryDownloadKind,
    /// Whether the source publishes a checksum that Lathe will verify. Downloads without one
    /// cannot be integrity checked, which is worth surfacing at the moment of approval.
    pub verified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BinaryDownloadKind {
    NodeRuntime,
    NpmPackages,
    LanguageServer,
    DebugAdapter,
    Extension,
}

impl BinaryDownloadKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::NodeRuntime => "Node.js runtime",
            Self::NpmPackages => "npm packages",
            Self::LanguageServer => "Language server",
            Self::DebugAdapter => "Debug adapter",
            Self::Extension => "Extension",
        }
    }
}

/// What the user decided about a single pending request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentDecision {
    /// Allow this exact version, and remember it.
    ApproveVersion,
    /// Allow this item forever, including future updates.
    ApproveAlways,
    /// Refuse. Not persisted; see the module docs.
    Deny,
}

/// The outcome of asking for permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentOutcome {
    Granted,
    Denied,
}

pub struct BinaryDownloadConsent {
    pending: Vec<PendingRequest>,
    /// Denials are remembered for the session only, so that a refused language server does not
    /// re-prompt on every keystroke, while still not becoming a permanent decision the user has
    /// forgotten making.
    session_denials: HashMap<(SharedString, Option<SharedString>), ()>,
}

struct PendingRequest {
    request: BinaryDownloadRequest,
    respond: oneshot::Sender<ConsentOutcome>,
}

/// Emitted whenever the pending set changes, so a UI can show or refresh the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingRequestsChanged;

impl EventEmitter<PendingRequestsChanged> for BinaryDownloadConsent {}

/// A request posted from a background download path, with the channel to answer it on.
type ConsentCall = (BinaryDownloadRequest, oneshot::Sender<ConsentOutcome>);

/// Send + Sync handle to the consent broker, safe to capture in the download paths. The broker
/// itself lives on the main thread, so requests are posted over a channel rather than by holding
/// an `AsyncApp`, which is not `Send`.
#[derive(Clone)]
pub struct ConsentRequester(mpsc::UnboundedSender<ConsentCall>);

impl ConsentRequester {
    /// Ask whether `request` may proceed. Resolves to `Denied` if the broker has gone away.
    pub async fn request(&self, request: BinaryDownloadRequest) -> ConsentOutcome {
        let (respond, receive) = oneshot::channel();
        if self.0.unbounded_send((request, respond)).is_err() {
            return ConsentOutcome::Denied;
        }
        receive.await.unwrap_or(ConsentOutcome::Denied)
    }
}

struct GlobalBinaryDownloadConsent {
    entity: Entity<BinaryDownloadConsent>,
    requester: ConsentRequester,
    _service_task: Task<()>,
}

impl Global for GlobalBinaryDownloadConsent {}

impl BinaryDownloadConsent {
    pub fn init(cx: &mut App) {
        let entity = cx.new(|_| Self {
            pending: Vec::new(),
            session_denials: HashMap::default(),
        });

        let (sender, mut receiver) = mpsc::unbounded::<ConsentCall>();
        let service_task = cx.spawn({
            let entity = entity.downgrade();
            async move |cx| {
                while let Some((request, respond)) = receiver.next().await {
                    let queued = entity.update(cx, |consent, cx| consent.request(request, cx));
                    match queued {
                        Ok(receive) => {
                            cx.background_spawn(async move {
                                let outcome = receive.await.unwrap_or(ConsentOutcome::Denied);
                                respond.send(outcome).ok();
                            })
                            .detach();
                        }
                        Err(_) => {
                            respond.send(ConsentOutcome::Denied).ok();
                        }
                    }
                }
            }
        });

        cx.set_global(GlobalBinaryDownloadConsent {
            entity,
            requester: ConsentRequester(sender),
            _service_task: service_task,
        });
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalBinaryDownloadConsent>()
            .map(|global| global.entity.clone())
    }

    /// A handle the download paths can hold across threads.
    pub fn requester(cx: &App) -> Option<ConsentRequester> {
        cx.try_global::<GlobalBinaryDownloadConsent>()
            .map(|global| global.requester.clone())
    }

    /// Everything currently waiting on an answer, oldest first.
    pub fn pending(&self) -> impl Iterator<Item = &BinaryDownloadRequest> {
        self.pending.iter().map(|pending| &pending.request)
    }

    /// Decide whether `request` may proceed, consulting the settings first and only queuing a
    /// question for the user when the answer is not already recorded.
    pub fn request(
        &mut self,
        request: BinaryDownloadRequest,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<ConsentOutcome> {
        let (respond, receive) = oneshot::channel();

        let settings = ProjectSettings::get_global(cx);
        if settings.allow_binary_downloads {
            respond.send(ConsentOutcome::Granted).ok();
            return receive;
        }

        if is_approved(&settings.approved_binary_downloads, &request) {
            respond.send(ConsentOutcome::Granted).ok();
            return receive;
        }

        if !settings.prompt_before_binary_downloads {
            respond.send(ConsentOutcome::Denied).ok();
            return receive;
        }

        let denial_key = (request.name.clone(), request.version.clone());
        if self.session_denials.contains_key(&denial_key) {
            respond.send(ConsentOutcome::Denied).ok();
            return receive;
        }

        self.pending.push(PendingRequest { request, respond });
        cx.emit(PendingRequestsChanged);
        cx.notify();
        receive
    }

    /// Apply the user's decisions. Any pending request not named in `decisions` is left pending.
    pub fn resolve(
        &mut self,
        decisions: &[(BinaryDownloadRequest, ConsentDecision)],
        cx: &mut Context<Self>,
    ) {
        let mut to_persist: Vec<(SharedString, SharedString)> = Vec::new();

        for (request, decision) in decisions {
            let outcome = match decision {
                ConsentDecision::ApproveVersion => {
                    // Nothing to pin to, so record it as permanent rather than silently
                    // approving every future version under a bogus key.
                    let version = request.version.clone().unwrap_or_else(|| ALWAYS.into());
                    to_persist.push((request.name.clone(), version));
                    ConsentOutcome::Granted
                }
                ConsentDecision::ApproveAlways => {
                    to_persist.push((request.name.clone(), ALWAYS.into()));
                    ConsentOutcome::Granted
                }
                ConsentDecision::Deny => {
                    self.session_denials
                        .insert((request.name.clone(), request.version.clone()), ());
                    ConsentOutcome::Denied
                }
            };

            // Answer every pending request matching this item, not just the first, so duplicate
            // requests from separate worktrees are all released by a single decision.
            let mut index = 0;
            while index < self.pending.len() {
                if &self.pending[index].request == request {
                    let pending = self.pending.remove(index);
                    pending.respond.send(outcome).ok();
                } else {
                    index += 1;
                }
            }
        }

        if !to_persist.is_empty() {
            persist_approvals(to_persist, cx);
        }

        cx.emit(PendingRequestsChanged);
        cx.notify();
    }

    /// Deny everything pending, used when the prompt is dismissed.
    pub fn deny_all(&mut self, cx: &mut Context<Self>) {
        for pending in self.pending.drain(..) {
            self.session_denials.insert(
                (
                    pending.request.name.clone(),
                    pending.request.version.clone(),
                ),
                (),
            );
            pending.respond.send(ConsentOutcome::Denied).ok();
        }
        cx.emit(PendingRequestsChanged);
        cx.notify();
    }
}

fn is_approved(approvals: &HashMap<Arc<str>, Arc<str>>, request: &BinaryDownloadRequest) -> bool {
    let Some(approved) = approvals.get(request.name.as_ref()) else {
        return false;
    };
    if approved.as_ref() == ALWAYS {
        return true;
    }
    // Without a version there is nothing to pin the approval to, so re-ask rather than reuse it.
    request
        .version
        .as_ref()
        .is_some_and(|version| approved.as_ref() == version.as_ref())
}

fn persist_approvals(approvals: Vec<(SharedString, SharedString)>, cx: &mut App) {
    let fs = <dyn fs::Fs>::global(cx);
    update_settings_file(fs, cx, move |content, _| {
        let approved = content.approved_binary_downloads.get_or_insert_default();
        for (name, version) in approvals {
            approved.insert(name.to_string(), version.to_string());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: &str, version: &str) -> BinaryDownloadRequest {
        BinaryDownloadRequest {
            name: name.to_string().into(),
            version: Some(version.to_string().into()),
            url: "https://example.com/download".into(),
            kind: BinaryDownloadKind::LanguageServer,
            verified: true,
        }
    }

    #[test]
    fn approval_is_scoped_to_the_approved_version() {
        let mut approvals = HashMap::default();
        approvals.insert(Arc::from("rust-analyzer"), Arc::from("0.3.2000"));

        assert!(is_approved(&approvals, &request("rust-analyzer", "0.3.2000")));
        // An update is new code that was never reviewed, so it must ask again.
        assert!(!is_approved(
            &approvals,
            &request("rust-analyzer", "0.3.2100")
        ));
        assert!(!is_approved(&approvals, &request("gopls", "0.3.2000")));
    }

    #[test]
    fn always_approves_future_versions() {
        let mut approvals = HashMap::default();
        approvals.insert(Arc::from("rust-analyzer"), Arc::from(ALWAYS));

        assert!(is_approved(&approvals, &request("rust-analyzer", "0.3.2000")));
        assert!(is_approved(
            &approvals,
            &request("rust-analyzer", "0.3.2100")
        ));
    }
}
