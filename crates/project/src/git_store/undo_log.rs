use crate::git_store::RepositoryId;
use gpui::SharedString;
use std::collections::VecDeque;
use std::time::SystemTime;

pub const MAX_UNDO_ENTRIES_PER_REPO: usize = 50;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct UndoId(u64);

#[derive(Debug, Clone)]
pub struct UndoEntry {
    pub id: UndoId,
    pub repository_id: RepositoryId,
    pub label: SharedString,
    pub timestamp: SystemTime,
    pub action: UndoAction,
}

#[derive(Debug, Clone)]
pub enum UndoAction {
    /// Move a branch ref (or the current HEAD) back to a prior SHA.
    /// `is_current` toggles between `branch -f` and `reset --hard`, because
    /// `git branch -f` refuses to operate on the checked-out branch.
    RestoreBranchTip {
        branch: String,
        sha: String,
        is_current: bool,
    },
    /// Delete a tag that was just created.
    DeleteTag { name: String },
    /// Recreate a branch that was deleted.
    RecreateBranch { name: String, sha: String },
    /// Rename a branch back to its prior name.
    RenameBranch { from: String, to: String },
    /// Apply a stash entry that was created defensively (e.g. before a
    /// destructive discard). The stash is located by its message (set when
    /// the defensive stash was created) so the undo survives the user pushing
    /// additional stash entries on top.
    PopStashByMessage { message: String },
}

#[derive(Default)]
pub struct GitUndoLog {
    entries: VecDeque<UndoEntry>,
    next_id: u64,
}

impl GitUndoLog {
    pub fn record(
        &mut self,
        repository_id: RepositoryId,
        label: impl Into<SharedString>,
        action: UndoAction,
    ) -> UndoId {
        let id = UndoId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        self.entries.push_back(UndoEntry {
            id,
            repository_id,
            label: label.into(),
            timestamp: SystemTime::now(),
            action,
        });
        self.evict_overflow(repository_id);
        id
    }

    fn evict_overflow(&mut self, repository_id: RepositoryId) {
        while self
            .entries
            .iter()
            .filter(|entry| entry.repository_id == repository_id)
            .count()
            > MAX_UNDO_ENTRIES_PER_REPO
        {
            let Some(pos) = self
                .entries
                .iter()
                .position(|entry| entry.repository_id == repository_id)
            else {
                break;
            };
            self.entries.remove(pos);
        }
    }

    pub fn entries_for(
        &self,
        repository_id: RepositoryId,
    ) -> impl DoubleEndedIterator<Item = &UndoEntry> {
        self.entries
            .iter()
            .filter(move |entry| entry.repository_id == repository_id)
    }

    pub fn latest_for(&self, repository_id: RepositoryId) -> Option<&UndoEntry> {
        self.entries_for(repository_id).next_back()
    }

    pub fn take(&mut self, id: UndoId) -> Option<UndoEntry> {
        let pos = self.entries.iter().position(|entry| entry.id == id)?;
        self.entries.remove(pos)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn clear_for(&mut self, repository_id: RepositoryId) {
        self.entries
            .retain(|entry| entry.repository_id != repository_id);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(n: u64) -> RepositoryId {
        RepositoryId(n)
    }

    #[test]
    fn records_and_retrieves_entries_in_order() {
        let mut log = GitUndoLog::default();
        let a = log.record(
            repo(1),
            "first",
            UndoAction::DeleteTag {
                name: "tag-a".into(),
            },
        );
        let b = log.record(
            repo(1),
            "second",
            UndoAction::DeleteTag {
                name: "tag-b".into(),
            },
        );
        assert_eq!(log.latest_for(repo(1)).unwrap().id, b);

        let entry = log.take(a).unwrap();
        assert_eq!(entry.label.as_ref(), "first");
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn evicts_oldest_when_over_capacity() {
        let mut log = GitUndoLog::default();
        let mut ids = Vec::new();
        for i in 0..MAX_UNDO_ENTRIES_PER_REPO + 5 {
            ids.push(log.record(
                repo(7),
                format!("op-{i}"),
                UndoAction::DeleteTag {
                    name: format!("t-{i}"),
                },
            ));
        }
        assert_eq!(log.entries_for(repo(7)).count(), MAX_UNDO_ENTRIES_PER_REPO);
        // The first 5 should be gone.
        for stale in &ids[..5] {
            assert!(log.entries.iter().all(|entry| entry.id != *stale));
        }
    }

    #[test]
    fn eviction_is_per_repository() {
        let mut log = GitUndoLog::default();
        for i in 0..MAX_UNDO_ENTRIES_PER_REPO + 10 {
            log.record(
                repo(1),
                "x",
                UndoAction::DeleteTag {
                    name: format!("t-{i}"),
                },
            );
        }
        let bonus = log.record(
            repo(2),
            "alone",
            UndoAction::DeleteTag {
                name: "lone".into(),
            },
        );
        assert_eq!(log.entries_for(repo(2)).count(), 1);
        assert_eq!(log.latest_for(repo(2)).unwrap().id, bonus);
    }
}
