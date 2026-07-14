use super::{init_test, wait_for_condition};

use fs::{FakeFs, Fs, PathEventKind, RealFs};
use git::DOT_GIT;
use gpui::{BackgroundExecutor, BorrowAppContext, TestAppContext};
use postage::stream::Stream;
use pretty_assertions::assert_eq;
use serde_json::json;
use settings::{SettingsStore, WorktreeId};
use std::{path::Path, sync::Arc};
use util::{path, rel_path::rel_path, test::TempTree};
use worktree::{EntryKind, Event, Worktree};

#[gpui::test]
async fn test_deferred_watch_repository_above_root(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(executor);
    fs.insert_tree(
        path!("/root"),
        json!({
            ".git": {},
            "subproject": {
                "a.txt": "A"
            }
        }),
    )
    .await;
    let worktree = Worktree::local(
        path!("/root/subproject").as_ref(),
        true,
        fs.clone(),
        Arc::default(),
        true,
        WorktreeId::from_proto(0),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    worktree
        .update(cx, |worktree, _| {
            worktree.as_local().unwrap().scan_complete()
        })
        .await;
    cx.run_until_parked();

    worktree.update(cx, |worktree, cx| {
        worktree.as_local_mut().unwrap().set_defer_watch(true, cx);
    });
    worktree
        .update(cx, |worktree, _| {
            worktree.as_local().unwrap().scan_complete()
        })
        .await;
    cx.run_until_parked();

    let repos = worktree.update(cx, |worktree, _| {
        worktree.as_local().unwrap().repositories()
    });
    pretty_assertions::assert_eq!(repos, [Path::new(path!("/root")).into()]);
}

#[gpui::test]
async fn test_deferred_watch_symlinks_pointing_outside(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/root",
        json!({
            "dir1": {
                "deps": {},
                "src": {
                    "a.rs": "",
                },
            },
            "dir2": {
                "src": {
                    "c.rs": "",
                }
            },
        }),
    )
    .await;

    fs.create_symlink("/root/dir1/deps/dep-dir2".as_ref(), "../../dir2".into())
        .await
        .unwrap();

    let tree = Worktree::local(
        Path::new("/root/dir1"),
        true,
        fs.clone(),
        Default::default(),
        true,
        WorktreeId::from_proto(0),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    cx.run_until_parked();

    tree.update(cx, |tree, cx| {
        tree.as_local_mut().unwrap().set_defer_watch(true, cx);
    });
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
    cx.run_until_parked();

    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external))
                .collect::<Vec<_>>(),
            vec![
                (rel_path(""), false),
                (rel_path("deps"), false),
                (rel_path("deps/dep-dir2"), true),
                (rel_path("src"), false),
                (rel_path("src/a.rs"), false),
            ]
        );
    });

    tree.read_with(cx, |tree, _| {
        tree.as_local()
            .unwrap()
            .refresh_entries_for_paths(vec![rel_path("deps/dep-dir2").into()])
    })
    .recv()
    .await;

    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external))
                .collect::<Vec<_>>(),
            vec![
                (rel_path(""), false),
                (rel_path("deps"), false),
                (rel_path("deps/dep-dir2"), true),
                (rel_path("deps/dep-dir2/src"), true),
                (rel_path("src"), false),
                (rel_path("src/a.rs"), false),
            ]
        );
    });

    tree.read_with(cx, |tree, _| {
        tree.as_local()
            .unwrap()
            .refresh_entries_for_paths(vec![rel_path("deps/dep-dir2/src").into()])
    })
    .recv()
    .await;

    tree.read_with(cx, |tree, _| {
        assert!(
            tree.entry_for_path(rel_path("deps/dep-dir2/src/c.rs"))
                .is_some()
        );
    });

    fs.insert_file(Path::new("/root/dir2/src/new.rs"), b"".to_vec())
        .await;

    wait_for_condition(cx, |cx| {
        tree.read_with(cx, |tree, _| {
            tree.entry_for_path(rel_path("deps/dep-dir2/src/new.rs"))
                .is_some()
        })
    })
    .await;
}

#[gpui::test]
async fn test_scan_symlinks_expanded(cx: &mut TestAppContext) {
    init_test(cx);

    // scan_symlinks defaults to Expanded — no settings change needed.

    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/root",
        json!({
            "dir1": {
                "deps": {
                    // symlink target placed here by create_symlink below
                },
                "src": {
                    "a.rs": "",
                },
            },
            "dir2": {
                "src": {
                    "b.rs": "",
                }
            }
        }),
    )
    .await;

    fs.create_symlink("/root/dir1/deps/dep-dir2".as_ref(), "../../dir2".into())
        .await
        .unwrap();

    let tree = Worktree::local(
        Path::new("/root/dir1"),
        true,
        fs.clone(),
        Default::default(),
        true,
        WorktreeId::from_proto(0),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    // With the default scan_symlinks = Expanded, the symlinked directory
    // should appear as an UnloadedDir entry with no children visible.
    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external))
                .collect::<Vec<_>>(),
            vec![
                (rel_path(""), false),
                (rel_path("deps"), false),
                (rel_path("deps/dep-dir2"), true),
                (rel_path("src"), false),
                (rel_path("src/a.rs"), false),
            ]
        );

        assert_eq!(
            tree.entry_for_path(rel_path("deps/dep-dir2")).unwrap().kind,
            EntryKind::UnloadedDir
        );
    });

    // Manually expand the symlinked directory.
    tree.read_with(cx, |tree, _| {
        tree.as_local()
            .unwrap()
            .refresh_entries_for_paths(vec![rel_path("deps/dep-dir2").into()])
    })
    .recv()
    .await;

    // After expansion, dep-dir2's immediate children are visible. Subdirectories
    // within it are present but not yet scanned.
    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external))
                .collect::<Vec<_>>(),
            vec![
                (rel_path(""), false),
                (rel_path("deps"), false),
                (rel_path("deps/dep-dir2"), true),
                (rel_path("deps/dep-dir2/src"), true),
                (rel_path("src"), false),
                (rel_path("src/a.rs"), false),
            ]
        );

        assert_eq!(
            tree.entry_for_path(rel_path("deps/dep-dir2/src"))
                .unwrap()
                .kind,
            EntryKind::UnloadedDir
        );
    });

    // Expand the subdirectory inside the symlinked directory.
    tree.read_with(cx, |tree, _| {
        tree.as_local()
            .unwrap()
            .refresh_entries_for_paths(vec![rel_path("deps/dep-dir2/src").into()])
    })
    .recv()
    .await;

    // After expanding the subdirectory, its files are visible.
    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external))
                .collect::<Vec<_>>(),
            vec![
                (rel_path(""), false),
                (rel_path("deps"), false),
                (rel_path("deps/dep-dir2"), true),
                (rel_path("deps/dep-dir2/src"), true),
                (rel_path("deps/dep-dir2/src/b.rs"), true),
                (rel_path("src"), false),
                (rel_path("src/a.rs"), false),
            ]
        );
    });
}

#[cfg(unix)]
#[gpui::test]
async fn test_real_fs_scan_symlinks_expanded(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    init_test(cx);

    // scan_symlinks defaults to Expanded — no settings change needed.

    let temp_root = TempTree::new(json!({
        "project": {
            "deps": {},
            "src": {
                "a.rs": "",
            },
        },
        "external": {
            "src": {
                "b.rs": "",
            },
        },
    }));

    std::os::unix::fs::symlink(
        "../../external",
        temp_root.path().join("project/deps/dep-external"),
    )
    .unwrap();

    let project_root = temp_root.path().join("project");
    let tree = Worktree::local(
        project_root.as_path(),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        true,
        WorktreeId::from_proto(0),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    // Before expansion, the symlinked directory should appear as an UnloadedDir
    // with no children visible.
    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external))
                .collect::<Vec<_>>(),
            vec![
                (rel_path(""), false),
                (rel_path("deps"), false),
                (rel_path("deps/dep-external"), true),
                (rel_path("src"), false),
                (rel_path("src/a.rs"), false),
            ]
        );

        assert_eq!(
            tree.entry_for_path(rel_path("deps/dep-external"))
                .unwrap()
                .kind,
            EntryKind::UnloadedDir
        );
    });

    // Manually expand the symlinked directory. This is the case #51382 was
    // added to fix; if this assertion fails it's a regression of that fix on
    // real filesystems.
    tree.read_with(cx, |tree, _| {
        tree.as_local()
            .unwrap()
            .refresh_entries_for_paths(vec![rel_path("deps/dep-external").into()])
    })
    .recv()
    .await;

    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external))
                .collect::<Vec<_>>(),
            vec![
                (rel_path(""), false),
                (rel_path("deps"), false),
                (rel_path("deps/dep-external"), true),
                (rel_path("deps/dep-external/src"), true),
                (rel_path("src"), false),
                (rel_path("src/a.rs"), false),
            ]
        );
    });
}

#[gpui::test]
async fn test_dot_git_dir_event_does_not_suppress_children(
    executor: BackgroundExecutor,
    cx: &mut TestAppContext,
) {
    // On Windows, modifying a file inside .git causes ReadDirectoryChangesW to also emit
    // a Modify event for the .git directory itself (because its last-write timestamp changes).
    // When these events arrive in the same batch, a naive ancestor-based dedup would collapse
    // all child events into the .git directory event, losing the information about which
    // specific files changed. This test verifies that the git-related event processing happens
    // before the dedup, so that meaningful .git child events still trigger UpdatedGitRepositories.
    init_test(cx);

    let fs = FakeFs::new(executor.clone());
    let project_dir = Path::new(path!("/project"));
    fs.insert_tree(
        project_dir,
        json!({
            ".git": {},
            "src": {
                "main.rs": "fn main() {}",
            },
        }),
    )
    .await;

    let worktree = Worktree::local(
        project_dir,
        true,
        fs.clone(),
        Default::default(),
        true,
        WorktreeId::from_proto(0),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    worktree
        .update(cx, |worktree, _| {
            worktree.as_local().unwrap().scan_complete()
        })
        .await;
    cx.run_until_parked();

    let dot_git = project_dir.join(DOT_GIT);

    // Case 1: Events for .git AND .git/index.lock should NOT emit UpdatedGitRepositories
    // (index.lock is in the skipped files list)
    {
        let mut events = cx.events(&worktree);
        fs.pause_events();
        fs.emit_fs_event(dot_git.clone(), Some(PathEventKind::Changed));
        fs.emit_fs_event(dot_git.join("index.lock"), Some(PathEventKind::Created));
        fs.unpause_events_and_flush();
        executor.run_until_parked();

        let got_git_update = drain_git_repo_updates(&mut events);
        assert!(
            !got_git_update,
            "should NOT emit UpdatedGitRepositories when .git batch only contains index.lock"
        );
    }

    // Case 2: Event for just .git (bare directory event) should NOT emit UpdatedGitRepositories
    {
        let mut events = cx.events(&worktree);
        fs.pause_events();
        fs.emit_fs_event(dot_git.clone(), Some(PathEventKind::Changed));
        fs.unpause_events_and_flush();
        executor.run_until_parked();

        let got_git_update = drain_git_repo_updates(&mut events);
        assert!(
            !got_git_update,
            "should NOT emit UpdatedGitRepositories for a bare .git directory event"
        );
    }

    // Case 3: Events for .git AND .git/index should emit UpdatedGitRepositories
    {
        let mut events = cx.events(&worktree);
        fs.pause_events();
        fs.emit_fs_event(dot_git.clone(), Some(PathEventKind::Changed));
        fs.emit_fs_event(dot_git.join("index"), Some(PathEventKind::Changed));
        fs.unpause_events_and_flush();
        executor.run_until_parked();

        let got_git_update = drain_git_repo_updates(&mut events);
        assert!(
            got_git_update,
            "should emit UpdatedGitRepositories when .git batch contains index"
        );
    }

    // Case 4: Event for .git/index only should emit UpdatedGitRepositories
    {
        let mut events = cx.events(&worktree);
        fs.pause_events();
        fs.emit_fs_event(dot_git.join("index"), Some(PathEventKind::Changed));
        fs.unpause_events_and_flush();
        executor.run_until_parked();

        let got_git_update = drain_git_repo_updates(&mut events);
        assert!(
            got_git_update,
            "should emit UpdatedGitRepositories for a .git/index event"
        );
    }

    {
        let mut events = cx.events(&worktree);
        fs.pause_events();
        fs.emit_fs_event(dot_git, Some(PathEventKind::Rescan));
        fs.unpause_events_and_flush();
        executor.run_until_parked();

        let got_git_update = drain_git_repo_updates(&mut events);
        assert!(
            got_git_update,
            "should emit UpdatedGitRepositories for a .git rescan event"
        );
    }

    {
        let mut events = cx.events(&worktree);
        fs.pause_events();
        fs.emit_fs_event(project_dir, Some(PathEventKind::Rescan));
        fs.unpause_events_and_flush();
        executor.run_until_parked();

        let got_git_update = drain_git_repo_updates(&mut events);
        assert!(
            got_git_update,
            "should emit UpdatedGitRepositories for a .git rescan event"
        );
    }
}

fn drain_git_repo_updates(events: &mut futures::channel::mpsc::UnboundedReceiver<Event>) -> bool {
    let mut found = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, Event::UpdatedGitRepositories(_)) {
            found = true;
        }
    }
    found
}

#[gpui::test]
async fn test_scan_symlinks_always(cx: &mut TestAppContext) {
    init_test(cx);

    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.scan_symlinks =
                    Some(settings::ScanSymlinksSetting::Always);
            });
        });
    });

    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/root",
        json!({
            "dir1": {
                "deps": {
                    // symlink target placed here by create_symlink below
                },
                "src": {
                    "a.rs": "",
                },
            },
            "dir2": {
                "src": {
                    "b.rs": "",
                }
            }
        }),
    )
    .await;

    fs.create_symlink("/root/dir1/deps/dep-dir2".as_ref(), "../../dir2".into())
        .await
        .unwrap();

    let tree = Worktree::local(
        Path::new("/root/dir1"),
        true,
        fs.clone(),
        Default::default(),
        true,
        WorktreeId::from_proto(0),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    // With scan_symlinks = Always, the symlinked directory's contents should be
    // fully visible on the first scan without any manual expansion.
    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external))
                .collect::<Vec<_>>(),
            vec![
                (rel_path(""), false),
                (rel_path("deps"), false),
                (rel_path("deps/dep-dir2"), true),
                (rel_path("deps/dep-dir2/src"), true),
                (rel_path("deps/dep-dir2/src/b.rs"), true),
                (rel_path("src"), false),
                (rel_path("src/a.rs"), false),
            ]
        );
    });
}

#[gpui::test(iterations = 10)]
async fn test_circular_symlinks_always(cx: &mut TestAppContext) {
    init_test(cx);

    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.scan_symlinks =
                    Some(settings::ScanSymlinksSetting::Always);
            });
        });
    });

    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/root",
        json!({
            "project": {
                "lib": {
                    "a": {
                        "a.txt": ""
                    }
                },
                "deps": {}
            },
            "outside": {
                "data.txt": ""
            }
        }),
    )
    .await;

    fs.create_symlink("/root/project/deps/ext".as_ref(), "../../outside".into())
        .await
        .unwrap();
    fs.create_symlink("/root/outside/back".as_ref(), "../../project".into())
        .await
        .unwrap();

    let tree = Worktree::local(
        Path::new("/root/project"),
        true,
        fs.clone(),
        Default::default(),
        true,
        WorktreeId::from_proto(0),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    tree.read_with(cx, |tree, _| {
        let entries: Vec<_> = tree
            .entries(true, 0)
            .map(|entry| (entry.path.as_ref(), entry.is_external))
            .collect();

        assert_eq!(
            entries,
            vec![
                (rel_path(""), false),
                (rel_path("deps"), false),
                (rel_path("deps/ext"), true),
                (rel_path("deps/ext/data.txt"), true),
                (rel_path("lib"), false),
                (rel_path("lib/a"), false),
                (rel_path("lib/a/a.txt"), false),
            ]
        );
    });
}

#[gpui::test]
async fn test_scan_symlinks_always_respects_gitignore(cx: &mut TestAppContext) {
    init_test(cx);

    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.scan_symlinks =
                    Some(settings::ScanSymlinksSetting::Always);
            });
        });
    });

    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/root",
        json!({
            "project": {
                ".gitignore": "ignored-dep\n",
                "deps": {}
            },
            "external-included": {
                "src": {
                    "included.rs": ""
                }
            },
            "external-ignored": {
                "src": {
                    "ignored.rs": ""
                }
            }
        }),
    )
    .await;

    fs.create_symlink(
        "/root/project/deps/included-dep".as_ref(),
        "../../external-included".into(),
    )
    .await
    .unwrap();
    fs.create_symlink(
        "/root/project/deps/ignored-dep".as_ref(),
        "../../external-ignored".into(),
    )
    .await
    .unwrap();

    let tree = Worktree::local(
        Path::new("/root/project"),
        true,
        fs.clone(),
        Default::default(),
        true,
        WorktreeId::from_proto(0),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external, entry.is_ignored))
                .collect::<Vec<_>>(),
            vec![
                (rel_path(""), false, false),
                (rel_path(".gitignore"), false, false),
                (rel_path("deps"), false, false),
                (rel_path("deps/ignored-dep"), true, true),
                (rel_path("deps/included-dep"), true, false),
                (rel_path("deps/included-dep/src"), true, false),
                (rel_path("deps/included-dep/src/included.rs"), true, false),
            ]
        );

        assert_eq!(
            tree.entry_for_path(rel_path("deps/ignored-dep"))
                .unwrap()
                .kind,
            EntryKind::UnloadedDir
        );
    });
}

// Real-fs counterparts to the FakeFs scan_symlinks tests above. FakeFs does not
// model `fs::canonicalize` against a real filesystem, so platform-specific
// canonicalization or readdir behavior is not covered by the FakeFs tests.
// These tests use a real temp directory and a real symlink to exercise the
// production path on the host platform.
#[cfg(unix)]
#[gpui::test]
async fn test_real_fs_scan_symlinks_always(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    init_test(cx);

    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.scan_symlinks =
                    Some(settings::ScanSymlinksSetting::Always);
            });
        });
    });

    let temp_root = TempTree::new(json!({
        "project": {
            "deps": {},
            "src": {
                "a.rs": "",
            },
        },
        "external": {
            "src": {
                "b.rs": "",
            },
        },
    }));

    // Relative symlink: from temp_root/project/deps/, `../../external` resolves
    // to temp_root/external — outside the worktree root at temp_root/project.
    std::os::unix::fs::symlink(
        "../../external",
        temp_root.path().join("project/deps/dep-external"),
    )
    .unwrap();

    let project_root = temp_root.path().join("project");
    let tree = Worktree::local(
        project_root.as_path(),
        true,
        Arc::new(RealFs::new(None, cx.executor())),
        Default::default(),
        true,
        WorktreeId::from_proto(0),
        &mut cx.to_async(),
    )
    .await
    .unwrap();

    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;

    tree.read_with(cx, |tree, _| {
        assert_eq!(
            tree.entries(true, 0)
                .map(|entry| (entry.path.as_ref(), entry.is_external))
                .collect::<Vec<_>>(),
            vec![
                (rel_path(""), false),
                (rel_path("deps"), false),
                (rel_path("deps/dep-external"), true),
                (rel_path("deps/dep-external/src"), true),
                (rel_path("deps/dep-external/src/b.rs"), true),
                (rel_path("src"), false),
                (rel_path("src/a.rs"), false),
            ]
        );
    });
}

// NOTE: test_repo_exclude_anchored_pattern and
// test_linked_worktree_gitfile_event_preserves_repo were NOT restored here. They
// fail on separate fork divergences unrelated to the scan_symlinks fix, and each
// needs its own decision:
//   - anchored `.git/info/exclude` patterns: the fork changed build_gitignore to
//     root at the file's parent instead of the work-dir root, so `vendor/cache`
//     no longer matches only at the top level.
//   - linked-worktree gitfile events: root_repo_common_dir returns a different
//     (non-None) path than upstream after a .git gitfile change.
// See the extraction plan's R5 notes.
