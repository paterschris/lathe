//! Lathe-owned data types for the extended git operations (file history, merge,
//! rebase, tags, reflog, progress streaming).
//!
//! These types live in a sibling module of [`crate::repository`] rather than
//! inline in that upstream-owned file, so the fork's additions stay separable
//! across upstream merges. They are re-exported from [`crate::repository`] so
//! existing `git::repository::{...}` import paths keep resolving unchanged.

use crate::Oid;
use crate::repository::RepoPath;
use gpui::SharedString;
use smol::channel::Sender;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct FileHistoryEntry {
    pub sha: SharedString,
    pub subject: SharedString,
    pub message: SharedString,
    pub commit_timestamp: i64,
    pub author_name: SharedString,
    pub author_email: SharedString,
}

#[derive(Debug, Clone)]
pub struct FileHistory {
    pub entries: Vec<FileHistoryEntry>,
    pub path: RepoPath,
}

/// A single progress update from a long-running git operation. Parsed from
/// `git fetch/push/pull --progress` output on stderr.
#[derive(Debug, Clone)]
pub struct GitProgressEvent {
    /// E.g. "Receiving objects", "Resolving deltas", "Counting objects".
    pub phase: SharedString,
    /// Percentage 0..=100 when git reports one. `None` for status lines that
    /// don't include a percentage (e.g. "Cloning into 'foo'...").
    pub percent: Option<u8>,
    /// The raw message after the phase prefix, useful for surfacing transfer
    /// rates and counts ("(300/635), 1.2 MiB | 500 KiB/s").
    pub message: SharedString,
}

impl GitProgressEvent {
    /// Parse a single line of git progress output. Git emits patterns like:
    ///
    /// ```text
    /// Receiving objects:  47% (300/635), 1.2 MiB | 500 KiB/s
    /// Resolving deltas: 100% (50/50), done.
    /// Counting objects: 12, done.
    /// Cloning into 'foo'...
    /// ```
    ///
    /// Returns `None` for lines that don't fit this shape so the caller can
    /// safely drop unrecognised stderr noise.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let (phase, rest) = line.split_once(':')?;
        // Heuristic: phase must look like a label (letters / spaces) and not
        // contain a path separator — otherwise we'd misinterpret "fatal:" or
        // ssh URLs as progress.
        if phase.is_empty() || phase.chars().any(|c| c == '/' || c == '\\') {
            return None;
        }
        let rest = rest.trim_start();
        let percent = rest
            .split_once('%')
            .and_then(|(prefix, _)| prefix.trim_start().parse::<u8>().ok())
            .filter(|p| *p <= 100);
        Some(Self {
            phase: phase.trim().to_string().into(),
            percent,
            message: rest.to_string().into(),
        })
    }
}

pub type GitProgressSender = Sender<GitProgressEvent>;

#[derive(Debug, Clone, Default)]
pub struct MergeOptions {
    /// Always create a merge commit, even when a fast-forward would be possible.
    pub no_ff: bool,
    /// Refuse to merge unless a fast-forward is possible.
    pub ff_only: bool,
    /// Squash the merged commits into the index without recording a merge commit.
    pub squash: bool,
    /// Optional commit message to use for the merge commit.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RebaseOptions {
    /// Rebase onto a different base than the upstream branch.
    pub onto: Option<String>,
    /// Autosquash !fixup / !squash commits during rebase.
    pub autosquash: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseAction {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
}

impl RebaseAction {
    pub fn as_command(self) -> &'static str {
        match self {
            RebaseAction::Pick => "pick",
            RebaseAction::Reword => "reword",
            RebaseAction::Edit => "edit",
            RebaseAction::Squash => "squash",
            RebaseAction::Fixup => "fixup",
            RebaseAction::Drop => "drop",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RebaseTodoEntry {
    pub action: RebaseAction,
    /// Commit SHA the action applies to. Ignored for `Drop` if you choose to omit.
    pub commit: String,
    /// When `action` is `Reword`, the replacement commit message. `None` keeps
    /// the existing message. Ignored for other actions.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseInProgressAction {
    Continue,
    Skip,
    Abort,
}

#[derive(Debug, Clone)]
pub struct ReflogEntry {
    /// SHA before the operation that produced this entry.
    pub old_oid: String,
    /// SHA after the operation.
    pub new_oid: String,
    /// Ref name this entry belongs to (e.g. `refs/heads/main`, `HEAD`).
    pub ref_name: String,
    /// Human-readable description (e.g. "commit: foo", "reset: moving to HEAD~").
    pub message: String,
    /// Unix timestamp.
    pub timestamp: i64,
}

#[derive(Debug, Clone)]
pub struct Tag {
    pub name: SharedString,
    pub target: Oid,
    /// True for annotated tags, false for lightweight tags.
    pub annotated: bool,
    /// Message for annotated tags. None for lightweight tags.
    pub message: Option<String>,
}
